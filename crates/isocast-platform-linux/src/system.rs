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
use isocast_engine::{MediaInfo, MediaReader, Probe, Volume};
use isocast_protocol::api::Machine;
use isocast_protocol::devices::{
    AudioOutput, Camera, CameraKind, Devices, Display, Microphone, Permission, PermissionAction,
    PermissionDetail, PermissionKind, PermissionsReport, VideoFormat, Window,
};
use isocast_protocol::{Canvas, SourceKind};

use crate::frames::LinuxFrame;

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

/// Media readers decode through this machine's FFmpeg at L.1; never
/// constructed ([`Platform::open_media`] bails), but the associated
/// type still exists.
pub enum PendingMedia {}

impl MediaReader for PendingMedia {
    type Frame = LinuxFrame;

    fn next_video(&mut self) -> Result<Option<(Self::Frame, Duration)>> {
        match *self {}
    }

    fn next_audio(&mut self) -> Result<Option<(std::vec::Vec<f32>, Duration)>> {
        match *self {}
    }
}

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
/// the device suffix each carries.
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
        let mut names = BTreeSet::new();
        for line in text.lines() {
            let Some(name) = encoder_name(line) else {
                continue;
            };
            // The name itself is what setups see in the engine's
            // encoder lists; nothing else about the line matters.
            names.insert(name);
        }
        names.into_iter().collect()
    })
    .clone()
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

/// The encoder's name from one `-encoders` line, when it runs on video
/// hardware, like
/// ` V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)`:
/// the first word is the six flag characters, the second the name. (The
/// name is the word *after* the flags — reading the flags' tail `....D`
/// as the name once left this list empty on a box with NVENC, and the
/// doctor said encoding would fall back to the CPU.)
pub(crate) fn encoder_name(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    let flags = words.next()?;
    if flags.len() != 6 || !flags.starts_with('V') {
        return None;
    }
    let name = words.next()?;
    HARDWARE_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
        .then(|| name.to_string())
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

    if let Ok(clients) = Command::new("hyprctl").arg("clients").arg("-j").output()
        && let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&clients.stdout)
    {
        let mut windows = Vec::new();
        for client in parsed.as_array().into_iter().flatten() {
            let Some(id) = client["id"].as_u64() else {
                continue;
            };
            let (Some(width), Some(height)) =
                (client["size"][0].as_u64(), client["size"][1].as_u64())
            else {
                continue;
            };
            windows.push(Window {
                id: id as u32,
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
        devices.windows = windows;
    }
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
// Permissions: Linux gates nothing per-app here (portals arrive L.2).
// ---------------------------------------------------------------------------

pub(crate) fn permissions_report() -> Result<PermissionsReport> {
    let mut details = Vec::new();
    for kind in PermissionKind::ALL {
        details.push(PermissionDetail {
            permission: kind,
            state: Permission::Granted,
            needed_for: kind.label().to_string(),
            action: PermissionAction::None,
            settings_url: "isocast://linux/portal".to_string(),
            next_step: None,
            restart_after_granting: false,
        });
    }
    Ok(PermissionsReport {
        permissions: isocast_protocol::devices::Permissions {
            screen_recording: Permission::Granted,
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

pub(crate) fn probe(source: &SourceKind, _canvas: &Canvas) -> Result<Option<Probe>> {
    match source {
        SourceKind::Display(_) | SourceKind::Window(_) | SourceKind::Application(_) => {
            bail!("screen, window and app capture arrive at L.2 of docs/port-omarchy.md")
        }
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
        assert_eq!(
            encoder_name(" V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)"),
            Some("h264_nvenc".into())
        );
        assert_eq!(
            encoder_name(" V....D hevc_nvenc           NVIDIA NVENC hevc encoder (codec hevc)"),
            Some("hevc_nvenc".into())
        );
        assert_eq!(
            encoder_name(" V....D prores_ks_vulkan     ProRes Vulkan (codec prores)"),
            Some("prores_ks_vulkan".into())
        );
        assert_eq!(
            encoder_name(" V....D av1_vaapi            AV1 (VAAPI) (codec av1)"),
            Some("av1_vaapi".into())
        );
        // FFmpeg's software encoders are not hardware, however the
        // doctor might like them to be: a box whose ffmpeg lists only
        // libx264 has no H.264 hardware encoder to run.
        assert_eq!(
            encoder_name(" V....D libx264               libx264 H.264 (codec h264)"),
            None
        );
        assert_eq!(
            encoder_name(" V....D libx265               libx265 HEVC (codec hevc)"),
            None
        );
        assert_eq!(
            encoder_name(" V....D prores_ks            Apple ProRes (iCodec Pro)"),
            None
        );
        // Audio encoders, hardware-free codecs, and the legend and rule
        // carry no video-hardware name.
        assert_eq!(
            encoder_name(" A....D aac                  AAC (Advanced Audio Coding)"),
            None
        );
        assert_eq!(
            encoder_name(" V....D mpeg4                MPEG-4 part 2"),
            None
        );
        assert_eq!(encoder_name(" V..... = Video"), None);
        assert_eq!(encoder_name(" ------"), None);
        assert_eq!(encoder_name("Encoders:"), None);
    }
}
