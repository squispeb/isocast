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
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_engine::{CaptureFormat, MediaInfo, Probe, Volume};
use isocast_protocol::api::Machine;
use isocast_protocol::devices::{
    AudioOutput, Camera, CameraKind, Devices, Display, Microphone, Permission, PermissionAction,
    PermissionDetail, PermissionKind, PermissionsReport, VideoFormat, Window,
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

/// Audio monitoring renders through PipeWire at L.3; no instance of
/// this type is ever constructed ([`Platform::play_audio`] bails).
pub struct NoPlayback;

// ---------------------------------------------------------------------------
// The clock and the thread.
// ---------------------------------------------------------------------------

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

/// The windows Hyprland reports, from `hyprctl clients -j`. Its `address`
/// (`0x6242fd272870`) is what a window is known by — there is no small
/// integer id as on macOS — so a window's id is the low 32 bits of it,
/// which the protocol documents as valid only while the window exists
/// (setups match windows by app and title, which is what Hyprland
/// carries: `class` and `title`, with `initialClass` standing in for a
/// bundle id).
fn windows(clients: &[u8]) -> Vec<Window> {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(clients) else {
        return Vec::new();
    };
    let mut windows = Vec::new();
    for client in parsed.as_array().into_iter().flatten() {
        let Some(id) = client["address"]
            .as_str()
            .and_then(|address| u64::from_str_radix(address.trim_start_matches("0x"), 16).ok())
            .map(|address| address as u32)
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

/// Cameras over V4L2, by the stable `/dev/v4l/by-id` path when at
/// hand, and their name from sysfs. Formats come from L.3's capture.
fn cameras(out: &mut Vec<Camera>) {
    let reading = match std::fs::read_dir("/dev/v4l/by-id") {
        Ok(reading) => reading,
        Err(_) => return,
    };
    let mut by_id: Vec<(String, PathBuf)> = Vec::new();
    for entry in reading.filter_map(|entry| entry.ok()) {
        let Some(target) = std::fs::read_link(entry.path())
            .ok()
            .and_then(|link| Some(entry.path().parent()?.join(link)))
        else {
            continue;
        };
        // The -video-index0 node is the one that captures; others are
        // metadata nodes of the same device.
        if entry
            .file_name()
            .to_string_lossy()
            .ends_with("-video-index0")
        {
            by_id.push((entry.file_name().to_string_lossy().to_string(), target));
        }
    }
    for (id, target) in by_id {
        let Some(sys_name) = target.file_name() else {
            continue;
        };
        let device_number = target
            .to_string_lossy()
            .rsplit_once("/dev/video")
            .map(|(_, tail)| tail.to_string())
            .unwrap_or_default();
        let name = std::fs::read_to_string(
            PathBuf::from("/sys/class/video4linux")
                .join(&device_number)
                .join("name"),
        )
        .unwrap_or_else(|_| sys_name.to_string_lossy().to_string());
        let usb = target.to_string_lossy().contains("usb-");
        out.push(Camera {
            id,
            name: name.trim().to_string(),
            model: name.clone(),
            manufacturer: String::new(),
            kind: if usb {
                CameraKind::External
            } else {
                CameraKind::BuiltIn
            },
            formats: Vec::<VideoFormat>::new(),
            suspended: false,
        });
    }
}

/// Microphones and outputs from PipeWire's PulseAudio compatibility.
fn audio(devices: &mut Devices, notes: &mut Vec<String>) {
    let Ok(sources) = Command::new("pactl")
        .arg("list")
        .arg("short")
        .arg("sources")
        .output()
    else {
        notes.push(DESKTOP_HINT.to_string());
        return;
    };
    let Ok(sinks) = Command::new("pactl")
        .arg("list")
        .arg("short")
        .arg("sinks")
        .output()
    else {
        notes.push(DESKTOP_HINT.to_string());
        return;
    };
    let default_source = default_of(&String::from_utf8_lossy(&sources.stdout), "Default Source:");
    let default_sink = default_of(&String::from_utf8_lossy(&sinks.stdout), "Default Sink:");
    devices.microphones =
        microphone_rows(&String::from_utf8_lossy(&sources.stdout), &default_source);
    devices.audio_outputs = output_rows(&String::from_utf8_lossy(&sinks.stdout), &default_sink);
}

/// `pactl list short` line form: index \t name \t driver \t spec \t
/// state (the spec column holds the sample spec, like
/// `s32le 2ch 48000Hz`).
fn microphone_rows(text: &str, default_id: &str) -> Vec<Microphone> {
    let mut out = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        let Some(name) = parts.get(1).map(|part| part.trim()) else {
            continue;
        };
        let spec = parts.get(3).copied().unwrap_or_default();
        let channels = spec.split_whitespace().nth(1).unwrap_or("2");
        let channels = channels.trim_start_matches("ch").parse().unwrap_or(2);
        let sample_rate = spec
            .split_whitespace()
            .nth(2)
            .and_then(|rate| rate.trim_end_matches("Hz").parse().ok())
            .unwrap_or(0.0);
        out.push(Microphone {
            id: name.to_string(),
            name: name.to_string(),
            model: String::new(),
            manufacturer: String::new(),
            default: name == default_id.trim(),
            sample_rates: vec![sample_rate],
            channels,
        });
    }
    out
}

fn output_rows(text: &str, default_id: &str) -> Vec<AudioOutput> {
    text.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            let name = parts.get(1)?.trim();
            let spec = parts.get(3).copied().unwrap_or_default();
            let channels = spec
                .split_whitespace()
                .nth(1)
                .unwrap_or("2")
                .trim_end_matches("ch")
                .parse()
                .unwrap_or(2);
            let sample_rate = spec
                .split_whitespace()
                .nth(2)
                .and_then(|rate| rate.trim_end_matches("Hz").parse().ok())
                .unwrap_or(0.0);
            Some(AudioOutput {
                id: name.to_string(),
                name: name.to_string(),
                default: name == default_id.trim(),
                sample_rate,
                channels,
            })
        })
        .collect()
}

/// From `pactl info`'s two-column plain output, find "key: value".
fn default_of(_pactl_short_text: &str, key: &str) -> String {
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

// ---------------------------------------------------------------------------
// Permissions: Linux gates nothing per-app, but the screen comes from the
// desktop's portal, and whether its picker has been answered is knowable.
// ---------------------------------------------------------------------------

pub(crate) fn permissions_report() -> Result<PermissionsReport> {
    // The portal remembers a screen choice as a restore token; without
    // one, the next screen capture is what shows the picker.
    let screen = if crate::portal::has_token() {
        Permission::Granted
    } else {
        Permission::NotDetermined
    };
    let details = PermissionKind::ALL
        .into_iter()
        .map(|kind| {
            let (state, action, next_step) = match kind {
                PermissionKind::ScreenRecording if screen != Permission::Granted => (
                    screen,
                    PermissionAction::Request,
                    Some(
                        "run `isocast permissions --request` and choose a screen in the \
                         desktop's picker; every later capture follows that choice (or let the \
                         first screen recording ask, within its 8 s open)"
                            .to_string(),
                    ),
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
            }
        })
        .collect();
    Ok(PermissionsReport {
        permissions: isocast_protocol::devices::Permissions {
            screen_recording: screen,
            camera: Permission::Granted,
            microphone: Permission::Granted,
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
        SourceKind::Window(_) | SourceKind::Application(_) => bail!(
            "window and app capture arrive at L.2's follow-up (a monitor through the portal is \
             what L.2 delivers)"
        ),
        SourceKind::Camera(_)
        | SourceKind::Microphone(_)
        | SourceKind::SystemAudio(_)
        | SourceKind::ApplicationAudio(_) => {
            bail!("camera and audio capture arrive at L.3 of docs/port-omarchy.md")
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
    /// its `address`, and reading an `id` (which Hyprland doesn't send
    /// for windows) left the doctor's window list empty.
    #[test]
    fn hyprland_windows_are_listed_by_address() {
        let clients = br#"[
            {"address": "0x6242fd272870", "mapped": true, "class": "helium",
             "initialClass": "helium", "title": "A tab - Helium", "size": [2022, 1100]},
            {"address": "0x55a1b2c3d4e5", "mapped": true, "class": "",
             "initialClass": "", "title": "Isocast", "size": [1280, 800]}
        ]"#;
        let listed = windows(clients);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].app, "helium");
        assert_eq!(listed[0].bundle_id, "helium");
        assert_eq!(listed[0].title.as_deref(), Some("A tab - Helium"));
        assert_eq!((listed[0].width, listed[0].height), (2022, 1100));
        assert_eq!(listed[0].id, 0xfd27_2870);
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
}
