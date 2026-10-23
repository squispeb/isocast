//! What this Linux machine is and what it can capture, plus the
//! folder-level checks the engine's disk and recording tasks rely on.
//!
//! Everything here reads standard interfaces (/proc, /sys, the
//! desktop's `hyprctl`, PipeWire's `pactl`, this machine's
//! `ffprobe`); nothing needs a build-time dependency. Devices keep
//! stable ids across sessions: PipeWire `node.name` for audio,
//! `/dev/v4l/by-id` for cameras, Hyprland connector names for
//! displays — what a setup file's `device` field stores.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_engine::{CameraRequest, CaptureFormat, MediaInfo, Probe, Volume};
use isocast_protocol::api::Machine;
use isocast_protocol::devices::{
    AudioOutput, Camera, Devices, Display, Microphone, Permission, PermissionAction,
    PermissionDetail, PermissionKind, PermissionsReport, Window,
};
use isocast_protocol::{Canvas, Codec, SourceKind};

/// Audio devices and desktop state come from helpers present on every
/// Omarchy install; their absence is a note, not an error (a daemon
/// over SSH has no Hyprland session, and it still lists cameras).
const DESKTOP_HINT: &str = "install or log in to a desktop session: displays and windows come \
     from `hyprctl monitors`/`hyprctl clients` when the daemon runs inside one, and system audio \
     needs PipeWire's PulseAudio compatibility (`pactl list short sources`)";

// ---------------------------------------------------------------------------
// The dead-simple types the trait's inert slots want.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The clock and the thread.
// ---------------------------------------------------------------------------

use crate::audio;
use crate::camera;
pub(crate) use crate::pacing::host_time;

/// The calling thread's own CPU time (`CLOCK_THREAD_CPUTIME_ID`).
pub(crate) fn thread_cpu_time() -> Option<Duration> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writes the calling thread's CPU time into `time`.
    let status = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
    (status == 0).then(|| Duration::new(time.tv_sec.max(0) as u64, time.tv_nsec.max(0) as u32))
}

/// CPU seconds this process has used, user and kernel time together
/// (`getrusage`): the daemon's own load, and the governor's.
pub(crate) fn process_cpu_seconds() -> Option<f64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage fills only the correctly sized output structure.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: filled in by getrusage above.
    let usage = unsafe { usage.assume_init() };
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    Some(seconds(usage.ru_utime) + seconds(usage.ru_stime))
}

/// Clock ticks the machine's CPUs have spent since boot, summed over all
/// of them (`/proc/stat`'s `cpu` line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CpuTicks {
    /// Every column but idle and iowait.
    pub busy: u64,
    /// Every column, idle included.
    pub total: u64,
}

impl CpuTicks {
    /// Parse one `cpu ` line of `/proc/stat`: user nice system idle iowait
    /// irq softirq steal (guest time is already counted in user and nice).
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let mut words = line.split_whitespace();
        if words.next()? != "cpu" {
            return None;
        }
        let ticks: Vec<u64> = words.filter_map(|value| value.parse().ok()).collect();
        if ticks.len() < 8 {
            return None;
        }
        let total: u64 = ticks[..8].iter().sum();
        Some(Self {
            busy: total - ticks[3] - ticks[4],
            total,
        })
    }

    /// How busy the CPUs were between `earlier` and `self`, in percent of
    /// all of them; `None` if no ticks passed (or the counters went back).
    pub(crate) fn percent_since(&self, earlier: &Self) -> Option<f64> {
        let total = self.total.checked_sub(earlier.total).filter(|t| *t > 0)?;
        let busy = self.busy.checked_sub(earlier.busy)?;
        Some((busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
    }
}

pub(crate) fn cpu_ticks() -> Option<CpuTicks> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    stat.lines().find_map(CpuTicks::parse)
}

/// Busy core-seconds across the machine since boot, from `/proc/stat`'s
/// `cpu` line, in clock ticks.
pub(crate) fn system_cpu_seconds() -> Option<f64> {
    let busy = cpu_ticks()?.busy;
    // SAFETY: sysconf only reads a configuration value.
    let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (per_second > 0).then(|| busy as f64 / per_second as f64)
}

// ---------------------------------------------------------------------------
// What this machine is (doctor and tune read it).
// ---------------------------------------------------------------------------

pub(crate) fn machine() -> Machine {
    Machine {
        chip: cpu_model().unwrap_or_else(|| "unknown".to_string()),
        memory_gb: meminfo_gb(),
        cores: std::thread::available_parallelism()
            .map(|value| value.get() as u32)
            .unwrap_or(0),
        os: linux_name(),
        hardware_encoders: hardware_encoders(),
    }
}

fn cpu_model() -> Option<String> {
    let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    info.lines()
        .find_map(|line| line.strip_prefix("model name"))
        .and_then(|rest| rest.split_once(':'))
        .map(|(_, name)| name.trim().to_string())
}

fn meminfo_gb() -> f64 {
    let Some(info) = std::fs::read_to_string("/proc/meminfo").ok() else {
        return 0.0;
    };
    info.lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.trim().split_once(' '))
        .and_then(|(value, _unit)| value.trim().parse::<u64>().ok())
        .map(|kb| kb as f64 / (1 << 20) as f64)
        .unwrap_or(0.0)
}

fn linux_name() -> String {
    let release = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"));
    release
        .ok()
        .and_then(|release| {
            let name = release
                .lines()
                .find_map(|line| line.strip_prefix("NAME="))
                .map(|value| value.trim_matches('"'));
            let version = release
                .lines()
                .find_map(|line| line.strip_prefix("VERSION_ID="))
                .map(|value| value.trim_matches('"'));
            name.map(|name| match version {
                Some(version) => format!("Linux {name} {version}"),
                None => format!("Linux {name}"),
            })
        })
        .unwrap_or_else(|| "Linux".to_string())
}

/// Encoder names that FFmpeg lists for this machine's video hardware,
/// once per process. Whether one really runs here is `tune`'s job (it
/// benchmarks); Ampere-family GPUs, like this box's RTX 3060 Ti, cannot
/// run `av1_nvenc` even when FFmpeg built with it.
///
/// This is the doctor's list of *hardware* encoders, so FFmpeg's
/// software ones (libx264, libx265, prores_ks) are left out: a doctor
/// that counted libx264 as an H.264 hardware encoder would tell a
/// CPU-only box it has one. What identifies the hardware ones here is
/// the device suffix each carries, and which suffixes can run is decided
/// by the devices this box actually has — FFmpeg lists every family it
/// was built with, 29 names on this one-GPU machine, Intel's QSV and
/// AMD's AMF among them.
pub(crate) fn hardware_encoders() -> Vec<std::string::String> {
    static LIST: OnceLock<Vec<String>> = OnceLock::new();
    LIST.get_or_init(|| {
        let Ok(output) = Command::new(ffmpeg_path())
            .arg("-hide_banner")
            .arg("-encoders")
            .output()
        else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let hardware = video_hardware();
        let mut names = BTreeSet::new();
        for line in text.lines() {
            let Some((name, suffix)) = encoder_name(line) else {
                continue;
            };
            if !hardware.runs(suffix) {
                continue;
            }
            // The name itself is what setups see in the engine's
            // encoder lists; nothing else about the line matters.
            names.insert(name);
        }
        names.into_iter().collect()
    })
    .clone()
}

/// The video hardware this box has: what tells which of FFmpeg's
/// hardware encoder families could run at all.
#[derive(Default, Debug)]
struct VideoHardware {
    /// PCI vendor ids of the DRM render nodes: 0x10de NVIDIA, 0x8086
    /// Intel, 0x1002 AMD.
    vendors: BTreeSet<u32>,
    /// Whether there is a DRM render node at all (a GPU with a KMS
    /// driver, which is what VAAPI, Vulkan video and the rest need).
    render_node: bool,
    /// Whether there is a V4L2 video node (the SoC codecs the
    /// `*_v4l2m2m` encoders drive).
    video_node: bool,
}

impl VideoHardware {
    /// Whether an encoder in FFmpeg's `suffix` family has what it needs
    /// here. The families this cannot judge — a VAAPI driver that lacks
    /// the codec, Ampere's missing AV1 encoder — are `tune`'s to find by
    /// running them.
    fn runs(&self, suffix: &str) -> bool {
        const NVIDIA: u32 = 0x10de;
        const INTEL: u32 = 0x8086;
        const AMD: u32 = 0x1002;
        match suffix {
            "_nvenc" | "_cuda" => self.vendors.contains(&NVIDIA),
            // VAAPI encoders come from Mesa, for Intel and AMD GPUs;
            // NVIDIA's VAAPI driver is a decode shim.
            "_vaapi" | "_opencl" => self.vendors.contains(&INTEL) || self.vendors.contains(&AMD),
            "_qsv" => self.vendors.contains(&INTEL),
            "_amf" => self.vendors.contains(&AMD),
            "_vulkan" => self.render_node,
            "_v4l2m2m" => self.video_node,
            // Android, Apple, Rockchip and Windows/X11-only paths:
            // nothing on a Wayland desktop drives them.
            _ => false,
        }
    }
}

/// What [`VideoHardware`] reads from the machine.
fn video_hardware() -> VideoHardware {
    let mut hardware = VideoHardware::default();
    // The nodes live in /dev; their PCI ids only in sysfs.
    if let Ok(entries) = std::fs::read_dir("/dev/dri") {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("renderD") {
                hardware.render_node = true;
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("renderD") {
                continue;
            }
            let vendor = entry.path().join("device/vendor");
            let Ok(text) = std::fs::read_to_string(vendor) else {
                continue;
            };
            if let Ok(id) = u32::from_str_radix(text.trim().trim_start_matches("0x"), 16) {
                hardware.vendors.insert(id);
            }
        }
    }
    hardware.video_node = has_video_node();
    hardware
}

/// Whether any `/dev/video*` node exists.
fn has_video_node() -> bool {
    std::fs::read_dir("/dev")
        .map(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .strip_prefix("video")
                    .is_some_and(|number| number.parse::<u32>().is_ok())
            })
        })
        .unwrap_or(false)
}

/// The NVENC encoder for a codec, when this box's ffmpeg has one: what a
/// session probe has to start, and what the writer chooses.
fn nvenc_for(codec: Codec) -> Option<&'static str> {
    let name = match codec {
        Codec::Hevc => "hevc_nvenc",
        Codec::H264 => "h264_nvenc",
        // ProRes encodes on the CPU (`prores_ks`), which has no sessions
        // to run out of.
        _ => return None,
    };
    hardware_encoders()
        .iter()
        .any(|listed| listed == name)
        .then_some(name)
}

/// How many of `files` simultaneous encodes of `codec` this box's video
/// hardware starts, or `None` when the question doesn't apply or can't be
/// answered: fewer than two files (nothing to contend for), no NVENC
/// encoder for the codec, or no ffmpeg to probe with. Only NVENC caps
/// sessions; VAAPI, Vulkan video and the SoC codecs have nothing to find.
///
/// The count is measured by running that many encodes at once, so it
/// holds sessions while it runs and must not be asked during a recording.
/// This box's driver (610.57.4) leaves the consumer cap behind — twelve
/// concurrent `hevc_nvenc` encodes open — but a driver that caps at three
/// would refuse the fourth file of a program-plus-three-ISOs take.
pub(crate) fn concurrent_encodes(
    codec: Codec,
    files: u32,
) -> Option<isocast_engine::EncoderSessions> {
    /// More than any consumer-capable driver allows, and the bound on
    /// what this probe will start.
    const MOST: u32 = 12;
    /// A legal NVENC picture (64×64 is refused as too small) for few
    /// enough frames that a session opens and closes quickly.
    const PROBE_INPUT: &str = "color=black:s=256x256:r=30:d=0.5";

    if files < 2 {
        return None;
    }
    let encoder = nvenc_for(codec)?;
    let asked = files.min(MOST);
    let mut children = Vec::new();
    for _ in 0..asked {
        let Ok(child) = Command::new(ffmpeg_path())
            .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .args(["-f", "lavfi", "-i", PROBE_INPUT])
            .args(["-c:v", encoder, "-f", "null", "-"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
        else {
            // No ffmpeg, or none that can start: nothing measured.
            break;
        };
        children.push(child);
    }
    let asked = children.len() as u32;
    if asked < 2 {
        return None;
    }
    let mut opened = 0;
    let mut refused = None;
    for child in children {
        match child.wait_with_output() {
            Ok(done) if done.status.success() => opened += 1,
            Ok(done) => {
                let said = String::from_utf8_lossy(&done.stderr);
                if refused.is_none() {
                    refused = said
                        .lines()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                        .map(|line| line.trim().to_string());
                }
            }
            Err(_) => {}
        }
    }
    Some(isocast_engine::EncoderSessions {
        asked,
        opened,
        refused,
    })
}

/// The video-hardware families FFmpeg names by suffix on their encoders
/// (`hevc_nvenc`, `h264_vaapi`, `prores_ks_vulkan`, …).
const HARDWARE_SUFFIXES: [&str; 15] = [
    "_nvenc",
    "_vaapi",
    "_qsv",
    "_v4l2m2m",
    "_amf",
    "_mediacodec",
    "_vulkan",
    "_opencl",
    "_videotoolbox",
    "_cuda",
    "_omx",
    "_rkmpp",
    "_d3d12va",
    "_dxva2",
    "_vdpau",
];

/// The nice value calibration's helper runs at: behind interactive work,
/// the value macOS's helper takes too.
const CALIBRATION_NICE: libc::c_int = 15;

/// Lower the calling thread's priority for the calibration helper. Linux
/// nice is per thread, and a thread (or child process) inherits its
/// creator's, so the measurement calls this on its main thread and again
/// on each encode thread before they start their writers. Unprivileged:
/// it only ever lowers. Returns what was set, for the profile's notes.
pub(crate) fn calibration_priority() -> Option<&'static str> {
    // SAFETY: setpriority with who 0 changes only the calling thread's nice.
    let status = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, CALIBRATION_NICE) };
    (status == 0).then_some("nice 15")
}

/// What calibration's measurements are filed under: plain `ffmpeg`, since
/// the rows mix NVENC and software encoders (ProRes is `prores_ks`); each
/// row's `hardware_accelerated` says which. Not `videotoolbox`, so the
/// capacity fit, which is fitted for Apple's engines, never takes these
/// rows as its input.
pub(crate) fn encoder_backend() -> &'static str {
    "ffmpeg"
}

/// Whether an FFmpeg encoder name is a hardware encoder, by its family
/// suffix (`hevc_nvenc`, `h264_vaapi`); `libx265` and `prores_ks` are not.
pub(crate) fn is_hardware_encoder(name: &str) -> bool {
    HARDWARE_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// The encoder's name and hardware family from one `-encoders` line, like
/// ` V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)`:
/// the first word is the six flag characters, the second the name. (The
/// name is the word *after* the flags — reading the flags' tail `....D`
/// as the name once left this list empty on a box with NVENC, and the
/// doctor said encoding would fall back to the CPU.)
pub(crate) fn encoder_name(line: &str) -> Option<(String, &'static str)> {
    let mut words = line.split_whitespace();
    let flags = words.next()?;
    if flags.len() != 6 || !flags.starts_with('V') {
        return None;
    }
    let name = words.next()?;
    let suffix = HARDWARE_SUFFIXES
        .iter()
        .find(|suffix| name.ends_with(**suffix))?;
    Some((name.to_string(), suffix))
}

// ---------------------------------------------------------------------------
// What Isocast can capture (the doctor's device check and setups).
// ---------------------------------------------------------------------------

pub(crate) fn devices() -> Result<Devices> {
    let mut devices = Devices::default();
    let mut notes = Vec::new();
    desktops(&mut devices, &mut notes);
    cameras(&mut devices.cameras);
    audio(&mut devices, &mut notes);
    devices.notes = notes;
    devices.permissions.screen_recording = Permission::Granted;
    devices.permissions.camera = Permission::Granted;
    devices.permissions.microphone = Permission::Granted;
    Ok(devices)
}

/// Displays and windows, through the desktop's own state.
fn desktops(devices: &mut Devices, notes: &mut Vec<String>) {
    let Ok(json) = Command::new("hyprctl").arg("monitors").arg("-j").output() else {
        notes.push(DESKTOP_HINT.to_string());
        return;
    };
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&json.stdout) else {
        notes.push(DESKTOP_HINT.to_string());
        return;
    };
    let mut displays: Vec<Display> = Vec::new();
    for monitor in parsed.as_array().into_iter().flatten() {
        let Some(name) = monitor["name"].as_str() else {
            continue;
        };
        let Some(width) = monitor["width"].as_u64() else {
            continue;
        };
        let Some(height) = monitor["height"].as_u64() else {
            continue;
        };
        displays.push(Display {
            id: name.to_string(),
            name: monitor["description"].as_str().unwrap_or(name).to_string(),
            main: monitor["x"].as_i64() == Some(0) && monitor["y"].as_i64() == Some(0),
            built_in: monitor["description"]
                .as_str()
                .is_some_and(|d| d.contains("built-in")),
            width: width as u32,
            height: height as u32,
            refresh_rate: monitor["refreshRate"].as_f64().unwrap_or(0.0),
        });
    }
    devices.displays = displays;

    if let Ok(clients) = Command::new("hyprctl").arg("clients").arg("-j").output() {
        devices.windows = windows(&clients.stdout);
    }
}

/// The windows Hyprland reports, from `hyprctl clients -j`. There is no
/// small integer id as on macOS, so a window's id is its `stableId`
/// (`1800000e`, a 32-bit number that is also the `identifier` the
/// compositor's toplevel protocol gives the window, and does not change
/// while the window lives), else the low 32 bits of its `address`
/// (`0x6242fd272870`). Ids are for listing only: setups match windows by
/// app and title, which is what Hyprland carries (`class` and `title`,
/// with `initialClass` standing in for a bundle id) and what capture
/// follows (toplevel.rs).
fn windows(clients: &[u8]) -> Vec<Window> {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(clients) else {
        return Vec::new();
    };
    let mut windows = Vec::new();
    for client in parsed.as_array().into_iter().flatten() {
        let Some(id) = client["stableId"]
            .as_str()
            .and_then(|stable| u32::from_str_radix(stable, 16).ok())
            .or_else(|| {
                client["address"]
                    .as_str()
                    .and_then(|address| {
                        u64::from_str_radix(address.trim_start_matches("0x"), 16).ok()
                    })
                    .map(|address| address as u32)
            })
        else {
            continue;
        };
        let (Some(width), Some(height)) = (client["size"][0].as_u64(), client["size"][1].as_u64())
        else {
            continue;
        };
        windows.push(Window {
            id,
            app: client["class"].as_str().unwrap_or_default().to_string(),
            bundle_id: client["initialClass"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            title: client["title"].as_str().map(|s| s.to_string()),
            width: width as u32,
            height: height as u32,
        });
    }
    windows
}

/// Cameras over V4L2: camera.rs owns the ioctls and sysfs, so this is
/// the device list it builds (ids from `/dev/v4l/by-id`, names, USB
/// identity and the formats a setup can ask for).
fn cameras(out: &mut Vec<Camera>) {
    out.extend(camera::devices());
}

/// Microphones and outputs from PipeWire's PulseAudio compatibility.
fn audio(devices: &mut Devices, notes: &mut Vec<String>) {
    match microphones() {
        Ok(sources) => devices.microphones = sources,
        Err(err) => {
            notes.push(DESKTOP_HINT.to_string());
            isocast_diagnostics::debug!("devices: listing microphones: {err:#}");
        }
    }
    match outputs() {
        Ok(sinks) => devices.audio_outputs = sinks,
        Err(err) => {
            notes.push(DESKTOP_HINT.to_string());
            isocast_diagnostics::debug!("devices: listing outputs: {err:#}");
        }
    }
}

/// Every input PipeWire has, read from `pactl`'s JSON: a microphone is
/// named by PipeWire's own `node.name` (which is also what a capture
/// stream targets) and shown by the description an audio panel shows,
/// and a sink's monitor is one of them too (its description says
/// "Monitor of ..."), which is how a setup can name one output's sound.
pub(crate) fn microphones() -> Result<Vec<Microphone>> {
    let json = pactl_list("sources")?;
    let default = pactl_default("Default Source:");
    Ok(parsed_rows(&json, |source| {
        let id = source["name"].as_str()?;
        let spec = source["sample_specification"].as_str().unwrap_or_default();
        Some(Microphone {
            name: source["description"].as_str().unwrap_or(id).to_string(),
            model: String::new(),
            manufacturer: String::new(),
            default: id == default,
            sample_rates: vec![sample_rate_of(spec)],
            channels: channels_of(spec),
            // PipeWire names positions (front-left, aux0), not inputs;
            // the protocol's names are the device's own, so none here.
            channel_names: Vec::new(),
            id: id.to_string(),
        })
    }))
}

/// Every sink PipeWire has, in the same shape.
pub(crate) fn outputs() -> Result<Vec<AudioOutput>> {
    let json = pactl_list("sinks")?;
    let default = pactl_default("Default Sink:");
    Ok(parsed_rows(&json, |sink| {
        let id = sink["name"].as_str()?;
        let spec = sink["sample_specification"].as_str().unwrap_or_default();
        Some(AudioOutput {
            name: sink["description"].as_str().unwrap_or(id).to_string(),
            default: id == default,
            sample_rate: sample_rate_of(spec),
            channels: channels_of(spec),
            id: id.to_string(),
        })
    }))
}

/// The default output's id, which is what a system-audio capture takes
/// its monitor from and what "default" plays through.
pub(crate) fn default_output() -> Result<String> {
    let default = pactl_default("Default Sink:");
    if default.is_empty() {
        bail!(
            "PipeWire has no default output to play through; set one in the desktop's sound \
             settings (or with `pactl set-default-sink`)"
        );
    }
    Ok(default)
}

/// The output a setup names for playback: an id, a name, or part of a
/// name, matched as macOS matches its outputs (an exact id or name, then
/// the same ignoring case, then the shortest name containing the query).
/// The error lists every output as `name (id)`.
pub(crate) fn find_output(query: &str) -> Result<AudioOutput> {
    let sinks = outputs().context("listing audio outputs")?;
    let lower = query.to_lowercase();
    sinks
        .iter()
        .find(|sink| sink.id == query || sink.name == query)
        .or_else(|| sinks.iter().find(|sink| sink.name.to_lowercase() == lower))
        .or_else(|| {
            sinks
                .iter()
                .filter(|sink| sink.name.to_lowercase().contains(&lower))
                .min_by_key(|sink| sink.name.len())
        })
        .cloned()
        .with_context(|| {
            let valid: Vec<String> = sinks
                .iter()
                .map(|sink| format!("{} ({})", sink.name, sink.id))
                .collect();
            format!(
                "no audio output matches {query:?}; valid: \"default\", {}",
                valid.join(", ")
            )
        })
}

/// The channel count to capture `microphone` in, and which of those
/// channels go on (from 0), for a source's `channels` (from 1): a one- or
/// two-channel device's own signal by default, a larger one's inputs
/// 1 + 2 (as on macOS). The error names the channel and lists the valid.
pub(crate) fn microphone_channels(
    microphone: &Microphone,
    channels: Option<&[u32]>,
) -> Result<(u32, Option<Vec<usize>>)> {
    let available = microphone.channels.max(1);
    let Some(channels) = channels else {
        return Ok(match available {
            1 | 2 => (available, None),
            _ => (available, Some(vec![0, 1])),
        });
    };
    if channels.is_empty() || channels.len() > 2 {
        bail!("channels: give one channel (mono) or two (stereo)");
    }
    if let Some((i, channel)) = channels
        .iter()
        .enumerate()
        .find(|(_, c)| **c == 0 || **c > available)
    {
        bail!(
            "channels[{i}]: {:?} has no input channel {channel}; valid: 1 to {available}",
            microphone.name
        );
    }
    Ok((
        available,
        Some(channels.iter().map(|c| *c as usize - 1).collect()),
    ))
}

/// The microphone a setup names: "default", an id, a name, or part of a
/// name, matched the way macOS matches its own devices (an exact id or
/// name, then the same ignoring case, then the shortest name containing
/// the query).
pub(crate) fn find_microphone(query: &str) -> Result<Microphone> {
    let sources = microphones().context("listing microphones")?;
    if query == "default" {
        if let Some(source) = sources.iter().find(|source| source.default) {
            return Ok(source.clone());
        }
        return sources
            .into_iter()
            .next()
            .context("PipeWire has no microphone at all");
    }
    let lower = query.to_lowercase();
    sources
        .iter()
        .find(|source| source.id == query || source.name == query)
        .or_else(|| {
            sources
                .iter()
                .find(|source| source.name.to_lowercase() == lower)
        })
        .or_else(|| {
            sources
                .iter()
                .filter(|source| source.name.to_lowercase().contains(&lower))
                .min_by_key(|source| source.name.len())
        })
        .cloned()
        .with_context(|| {
            let names: Vec<String> = sources
                .iter()
                .map(|source| format!("{} ({})", source.name, source.id))
                .collect();
            format!(
                "no microphone matches {query:?}; microphones: {}",
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            )
        })
}

/// One of `pactl`'s lists as JSON (`pactl -f json list <what>`).
fn pactl_list(what: &str) -> Result<Vec<u8>> {
    let output = Command::new("pactl")
        .args(["-f", "json", "list", what])
        .output()
        .with_context(|| {
            format!("running `pactl -f json list {what}` (PipeWire's PulseAudio compatibility)")
        })?;
    if !output.status.success() {
        bail!(
            "`pactl -f json list {what}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// From `pactl info`'s two-column plain output, the value of "key:".
fn pactl_default(key: &str) -> String {
    match Command::new("pactl").arg("info").output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap_or("")
            .trim()
            .to_string(),
        Err(_) => String::new(),
    }
}

/// `pactl`'s JSON arrays, each entry through `row`.
fn parsed_rows<T>(json: &[u8], row: impl Fn(&serde_json::Value) -> Option<T>) -> Vec<T> {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(json) else {
        return Vec::new();
    };
    parsed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(row)
        .collect()
}

/// A sample spec like `s16le 2ch 48000Hz`, whose middle word is the
/// channel count and whose last is the rate.
fn channels_of(spec: &str) -> u32 {
    spec.split_whitespace()
        .nth(1)
        .and_then(|word| word.trim_end_matches("ch").parse().ok())
        .unwrap_or(2)
}

fn sample_rate_of(spec: &str) -> f64 {
    spec.split_whitespace()
        .nth(2)
        .and_then(|word| word.trim_end_matches("Hz").parse().ok())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Permissions: Linux gates nothing per-app, but the screen comes from the
// desktop's portal, and whether its picker has been answered is knowable.
// ---------------------------------------------------------------------------

pub(crate) fn permissions_report() -> Result<PermissionsReport> {
    // The portal remembers a screen choice as a restore token; without
    // one, the next screen capture is what shows the picker, unless the
    // last picker failed, in which case screen sources wait to be asked.
    let refused = (!crate::portal::has_token())
        .then(crate::portal::refused)
        .flatten();
    let screen = if crate::portal::has_token() {
        Permission::Granted
    } else if refused.is_some() {
        Permission::Denied
    } else {
        Permission::NotDetermined
    };
    let choice = (screen == Permission::Granted)
        .then(crate::portal::choice)
        .flatten();
    let details = PermissionKind::ALL
        .into_iter()
        .map(|kind| {
            let (state, action, next_step) = match kind {
                PermissionKind::ScreenRecording if screen != Permission::Granted => (
                    screen,
                    PermissionAction::Request,
                    Some(match &refused {
                        Some(refused) => refused.waiting(),
                        None => "run `isocast permissions --request` and choose a screen in the \
                                 desktop's picker; every later capture follows that choice (or \
                                 let the first screen recording ask, within its 8 s open). \
                                 Window and app sources need no choice: the compositor offers \
                                 them directly"
                            .to_string(),
                    }),
                ),
                _ => (Permission::Granted, PermissionAction::None, None),
            };
            PermissionDetail {
                permission: kind,
                state,
                needed_for: kind.label().to_string(),
                action,
                settings_url: "isocast://linux/portal".to_string(),
                next_step,
                restart_after_granting: false,
                // No settings list to find an app in: the portal asks.
                app: String::new(),
                app_path: None,
                denied_without_prompt: false,
                choice: (kind == PermissionKind::ScreenRecording)
                    .then(|| choice.clone())
                    .flatten(),
            }
        })
        .collect();
    Ok(PermissionsReport {
        permissions: isocast_protocol::devices::Permissions {
            screen_recording: screen,
            camera: Permission::Granted,
            microphone: Permission::Granted,
            // Linux has no separate permission for system audio.
            system_audio: Permission::Granted,
        },
        app: "isocastd".to_string(),
        details,
    })
}

// ---------------------------------------------------------------------------
// Probe: which device a source would use, or what L.2/L.3 has to give.
// ---------------------------------------------------------------------------

pub(crate) fn probe(source: &SourceKind, canvas: &Canvas) -> Result<Option<Probe>> {
    match source {
        // The portal's picker decides which monitor is shared, so a
        // display's size here is what the desktop reports for it, not a
        // promise about the stream (see portal.rs).
        SourceKind::Display(display) => {
            let found = find_display(&display.display)?;
            Ok(Some(Probe {
                device: format!("{} (id {})", found.name, found.id),
                video: Some(CaptureFormat {
                    width: found.width,
                    height: found.height,
                    fps: canvas.fps,
                }),
                channels: None,
            }))
        }
        SourceKind::Window(window) => Ok(Some(crate::toplevel::probe(
            isocast_engine::ScreenTarget::Window {
                app: &window.app,
                title: window.title.as_deref(),
            },
            canvas.fps,
        )?)),
        SourceKind::Application(application) => Ok(Some(crate::toplevel::probe(
            isocast_engine::ScreenTarget::Application {
                app: &application.app,
                display: &application.display,
            },
            canvas.fps,
        )?)),
        SourceKind::Camera(settings) => {
            let request = CameraRequest {
                size: settings.width.zip(settings.height),
                fps: settings.fps,
            };
            let (device, format) = camera::probe(&settings.device, request, canvas)?;
            Ok(Some(Probe {
                device,
                video: Some(format),
                channels: None,
            }))
        }
        SourceKind::Microphone(microphone) => {
            let found = find_microphone(&microphone.device)?;
            Ok(Some(Probe {
                device: format!("{} (id {})", found.name, found.id),
                video: None,
                channels: Some(found.channels),
            }))
        }
        // What the machine plays comes from the default output's monitor,
        // whose channel count is what a capture of it delivers.
        SourceKind::SystemAudio(_) => {
            let output = default_output()?;
            let monitor = outputs()?
                .into_iter()
                .find(|candidate| candidate.id == output);
            Ok(Some(Probe {
                device: format!("the monitor of {output}"),
                video: None,
                channels: Some(monitor.map_or(2, |output| output.channels)),
            }))
        }
        // An app's own streams are what there is to link to: an app that
        // is playing nothing has none, so a setup naming one is told what
        // is playing instead.
        SourceKind::ApplicationAudio(application) => {
            let (name, streams) = audio::find_app(&application.app)?;
            let playing: Vec<String> = streams.iter().map(audio::AppStream::describe).collect();
            let detail = playing.join(", ");
            Ok(Some(Probe {
                device: if detail == name {
                    format!("the sound of {name}")
                } else {
                    format!("the sound of {name} ({detail})")
                },
                video: None,
                channels: Some(2),
            }))
        }
        SourceKind::Browser(_) => bail!("browser capture arrives at L.6 of docs/port-omarchy.md"),
        SourceKind::Color(_)
        | SourceKind::Image(_)
        | SourceKind::Slideshow(_)
        | SourceKind::Text(_)
        | SourceKind::Media(_)
        | SourceKind::TestPattern(_)
        | SourceKind::Tone(_) => Ok(None),
    }
}

/// The display a setup names: "main", or a name or id (`devices` lists
/// both), matched the way macOS matches them. An unknown name is an
/// error listing what exists, so a typo is a message and not a take of
/// the wrong screen.
pub(crate) fn find_display(query: &str) -> Result<Display> {
    let displays = devices()?.displays;
    let found = displays
        .iter()
        .find(|d| (query == "main" && d.main) || d.id == query || d.name == query)
        .or_else(|| displays.iter().find(|d| d.name.eq_ignore_ascii_case(query)));
    match found {
        Some(found) => Ok(found.clone()),
        None => {
            let valid: Vec<String> = displays
                .iter()
                .map(|d| format!("{:?} (id {})", d.name, d.id))
                .collect();
            bail!(
                "no display matches {query:?}; valid: \"main\", {}",
                valid.join(", ")
            )
        }
    }
}

// ---------------------------------------------------------------------------
// The recording folder.
// ---------------------------------------------------------------------------

/// Free space and the file system of the volume holding `path`.
pub(crate) fn volume(path: &Path) -> Result<Volume> {
    let stat = libc_statvfs(path)?;
    let fs = mount_type(path);
    let network = matches!(
        fs.as_str(),
        "nfs" | "nfs4" | "cifs" | "smb" | "smbfs" | "sshfs" | "fuse.rclone" | "fuse.sshfs" | "9p"
    );
    Ok(Volume {
        free_bytes: (stat.f_frsize.max(1) as u64) * stat.f_bavail as u64,
        file_system: fs,
        network,
    })
}

/// statvfs through libc, in a small safety wrapper so the UMD lines
/// stay in one spot.
#[allow(non_snake_case)]
fn libc_statvfs(path: &Path) -> Result<libc::statvfs> {
    let mut candidate = path;
    let resolved = loop {
        if let Ok(existing) = std::fs::canonicalize(candidate) {
            break existing;
        }
        candidate = candidate
            .parent()
            .context("path has no existing ancestor")?;
    };
    let c_path = std::ffi::CString::new(resolved.as_os_str().as_encoded_bytes())?;
    // SAFETY: c_path is NUL-terminated and stat is a valid writable output struct.
    let mut stat = unsafe { std::mem::zeroed::<libc::statvfs>() };
    // SAFETY: pointers refer to valid memory for the duration of this call.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error()).context("statvfs");
    }
    Ok(stat)
}

fn mount_type(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(text) = std::fs::read_to_string("/proc/self/mounts") {
        let mut best: (usize, String) = (0, String::new());
        for line in text.lines() {
            let mut columns = line.split_whitespace();
            let (Some(_device), Some(target), Some(fs_type)) =
                (columns.next(), columns.next(), columns.next())
            else {
                continue;
            };
            if resolved.starts_with(target) && target.len() > best.0 {
                best = (target.len(), fs_type.to_string());
            }
        }
        if !best.1.is_empty() {
            return best.1;
        }
    }
    String::from("unknown")
}

/// How long a small synced write next to `probe` (a recording in
/// progress) takes: written, synced, then removed.
pub(crate) fn write_latency(probe_path: &Path) -> Option<Duration> {
    let probe = probe_path.to_path_buf();
    // SAFETY: none; see the comment above.
    let probe_file = probe.join("isocast-latency-probe.tmp");
    let data = vec![0u8; 64 * 1024];
    let started = Instant::now();
    std::fs::write(&probe_file, &data).ok()?;
    let file = std::fs::OpenOptions::new()
        .append(true)
        .open(&probe_file)
        .ok()?;
    file.sync_all().ok()?;
    let latency = started.elapsed();
    drop(file);
    std::fs::remove_file(&probe_file).ok();
    Some(latency)
}

// ---------------------------------------------------------------------------
// File facts for media sources (the setup validation and layout use
// them), through this machine's ffprobe.
// ---------------------------------------------------------------------------

pub(crate) fn media_info(path: &Path) -> Result<MediaInfo> {
    let output = Command::new(ffprobe_path())
        .arg("-v")
        .arg("error")
        .arg("-print_format")
        .arg("json")
        .arg("-show_streams")
        .arg("-show_format")
        .arg(path)
        .output()
        .context("running ffprobe")?;
    if !output.status.success() {
        bail!(
            "ffprobe could not read {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing ffprobe's JSON")?;
    let mut video = None;
    let mut audio = false;
    let mut duration = 0.0f64;
    if let Some(streams) = parsed["streams"].as_array() {
        for stream in streams {
            match stream["codec_type"].as_str() {
                Some("video") => {
                    video = Some((
                        stream["width"].as_u64().unwrap_or(0) as u32,
                        stream["height"].as_u64().unwrap_or(0) as u32,
                    ));
                }
                Some("audio") => audio = true,
                _ => {}
            }
        }
    }
    if let Some(value) = parsed["format"]["duration"].as_str() {
        duration = value.parse().unwrap_or(0.0);
    }
    if duration == 0.0
        && let Some(value) = parsed["streams"]
            .as_array()
            .and_then(|streams| streams.iter().find_map(|s| s["duration"].as_str()))
    {
        duration = value.parse().unwrap_or(0.0);
    }
    Ok(MediaInfo {
        video,
        audio,
        duration: Duration::from_secs_f64(duration.max(0.0)),
    })
}

/// `ISOCAST_FFMPEG` overrides the binary; the writer, machine() and
/// ffprobe share the lookup.
pub(crate) fn ffmpeg_path() -> String {
    std::env::var("ISOCAST_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string())
}

pub(crate) fn ffprobe_path() -> String {
    std::env::var("ISOCAST_FFPROBE").unwrap_or_else(|_| "ffprobe".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real `-encoders` lines this box prints, as they are spaced:
    /// the name is the word after the six flag characters, never the
    /// flags themselves (reading `....D` as the name left the doctor
    /// reporting a machine with NVENC as CPU-only).
    #[test]
    fn only_video_hardware_encoders_are_named() {
        let named = |line: &str| encoder_name(line).map(|(name, _)| name);
        assert_eq!(
            named(" V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)"),
            Some("h264_nvenc".into())
        );
        assert_eq!(
            named(" V....D hevc_nvenc           NVIDIA NVENC hevc encoder (codec hevc)"),
            Some("hevc_nvenc".into())
        );
        assert_eq!(
            named(" V....D prores_ks_vulkan     ProRes Vulkan (codec prores)"),
            Some("prores_ks_vulkan".into())
        );
        assert_eq!(
            named(" V....D av1_vaapi            AV1 (VAAPI) (codec av1)"),
            Some("av1_vaapi".into())
        );
        // FFmpeg's software encoders are not hardware, however the
        // doctor might like them to be: a box whose ffmpeg lists only
        // libx264 has no H.264 hardware encoder to run.
        assert_eq!(
            named(" V....D libx264               libx264 H.264 (codec h264)"),
            None
        );
        assert_eq!(
            named(" V....D libx265               libx265 HEVC (codec hevc)"),
            None
        );
        assert_eq!(
            named(" V....D prores_ks            Apple ProRes (iCodec Pro)"),
            None
        );
        // Audio encoders, hardware-free codecs, and the legend and rule
        // carry no video-hardware name.
        assert_eq!(
            named(" A....D aac                  AAC (Advanced Audio Coding)"),
            None
        );
        assert_eq!(named(" V....D mpeg4                MPEG-4 part 2"), None);
        assert_eq!(named(" V..... = Video"), None);
        assert_eq!(named(" ------"), None);
        assert_eq!(named("Encoders:"), None);
    }

    /// This box's own `hyprctl clients -j` shape: a window is named by
    /// its `stableId` (else its `address`), and reading an `id` (which Hyprland doesn't send
    /// for windows) left the doctor's window list empty.
    #[test]
    fn hyprland_windows_are_listed_by_address() {
        let clients = br#"[
            {"address": "0x6242fd272870", "mapped": true, "class": "helium",
             "initialClass": "helium", "title": "A tab - Helium", "size": [2022, 1100]},
            {"address": "0x55a1b2c3d4e5", "mapped": true, "class": "",
             "initialClass": "", "title": "Isocast", "size": [1280, 800],
             "stableId": "1800000e"}
        ]"#;
        let listed = windows(clients);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].app, "helium");
        assert_eq!(listed[0].bundle_id, "helium");
        assert_eq!(listed[0].title.as_deref(), Some("A tab - Helium"));
        assert_eq!((listed[0].width, listed[0].height), (2022, 1100));
        assert_eq!(listed[0].id, 0xfd27_2870);
        assert_eq!(listed[1].id, 0x1800_000e, "stableId wins over the address");
        assert_eq!(listed[1].app, "");
        assert_ne!(
            listed[0].id, listed[1].id,
            "two windows must not share an id"
        );
        // Anything without an address or a size is not a window to offer.
        assert!(windows(br#"[{"class": "x", "title": "y"}]"#).is_empty());
        assert!(windows(b"not json").is_empty());
    }

    /// Which families can run follows the devices: FFmpeg lists every
    /// family it was built with, and this box's list has Intel's QSV and
    /// AMD's AMF beside its NVIDIA encoders.
    #[test]
    fn a_family_runs_only_with_its_hardware() {
        let nvidia = VideoHardware {
            vendors: [0x10de].into_iter().collect(),
            render_node: true,
            video_node: false,
        };
        assert!(nvidia.runs("_nvenc"));
        assert!(nvidia.runs("_vulkan"));
        assert!(!nvidia.runs("_qsv"));
        assert!(!nvidia.runs("_amf"));
        // NVIDIA's VAAPI support is a decode shim, not an encoder.
        assert!(!nvidia.runs("_vaapi"));
        assert!(!nvidia.runs("_v4l2m2m"));

        let amd = VideoHardware {
            vendors: [0x1002].into_iter().collect(),
            render_node: true,
            video_node: true,
        };
        assert!(amd.runs("_amf"));
        assert!(amd.runs("_vaapi"));
        assert!(amd.runs("_v4l2m2m"));
        assert!(!amd.runs("_nvenc"));

        let headless = VideoHardware::default();
        for suffix in HARDWARE_SUFFIXES {
            assert!(
                !headless.runs(suffix),
                "{suffix} cannot run on a box with no video hardware"
            );
        }
    }

    #[test]
    fn hardware_encoders_are_told_by_their_family_suffix() {
        for hardware in ["hevc_nvenc", "h264_vaapi", "av1_qsv", "h264_vulkan"] {
            assert!(is_hardware_encoder(hardware), "{hardware}");
        }
        for software in ["libx264", "libx265", "prores_ks", "prores_aw", "libsvtav1"] {
            assert!(!is_hardware_encoder(software), "{software}");
        }
    }
}
