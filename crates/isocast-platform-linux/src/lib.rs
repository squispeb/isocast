//! Linux back end for Isocast (Wayland, PipeWire, V4L2, FFmpeg encoders).
//!
//! The master clock is `CLOCK_MONOTONIC` in nanoseconds: the same
//! monotonic master clock macOS's Mach absolute time provides. Frames
//! live in heap pools the GPU uploads from and the writer reads back
//! into; recording is encoded by the machine's own encoder processes
//! (NVENC hardware first, software fallbacks), fed over pipes.
//!
//! This is docs/port-omarchy.md's L.0 + L.1, and L.3's camera: synthetic
//! sources, the encode/write/pacing path, devices, secrets, volumes, the
//! doctor/tune inputs and V4L2 cameras, so `isocast record`, `doctor` and
//! the CLI/MCP surface work on Linux today with real files and a real
//! webcam. Screen and audio capture, media decoding, shared monitor
//! surfaces, browser pages and joins bail with a message naming the
//! milestone that delivers them.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use isocast_engine::{
    AudioRender, AudioSink, CameraRequest, CaptureFormat, EncoderSessions, JoinSpec, Joined,
    LiveAudioSpec, LiveVideoSpec, MediaInfo, MediaPart, OnJoinProgress, Platform, Probe, Time,
    VideoSink, Volume, WriterSpec,
};
use isocast_protocol::api::Machine;
use isocast_protocol::devices::{Devices, PermissionKind, PermissionsReport};
use isocast_protocol::{Canvas, Codec, SourceKind};

pub(crate) mod camera;
pub(crate) mod frames;
pub(crate) mod gpu;
mod jpeg;
mod keychain;
pub(crate) mod media;
pub(crate) mod pacing;
mod portal;
mod screencast;
pub(crate) mod stream;
pub(crate) mod system;
pub(crate) mod writer;

pub use frames::LinuxFrame;
pub use gpu::VulkanImporter;
pub use stream::{LiveAacEncoder, LiveH264Encoder};
pub use writer::FfmpegFileWriter;

use crate::frames::CpuPool;

#[derive(Clone, Copy)]
pub struct LinuxPlatform;

/// A running capture. Dropping it stops the capture and joins its
/// threads: a camera's ffmpeg child (L.3), or the portal's PipeWire
/// stream (L.2).
pub struct Capture {
    /// Kept alive for the capture's lifetime: its `Drop` stops it.
    _running: Running,
}

/// What a handle is holding open. One variant per kind of capture a
/// source can have.
enum Running {
    /// A camera read through this machine's ffmpeg (camera.rs).
    Camera { _camera: camera::CameraCapture },
    /// A screen shared through the desktop's portal (screencast.rs).
    Screen { _screen: screencast::ScreenCapture },
}

impl Platform for LinuxPlatform {
    type Frame = LinuxFrame;
    type FramePool = CpuPool;
    type Writer = FfmpegFileWriter;
    type Capture = Capture;
    type Importer = VulkanImporter;
    type Media = crate::media::MediaReader;
    type Playback = system::NoPlayback;
    /// Real-time streaming encoders are L.5's in-process NVENC/libav:
    /// the types are uninhabited and their `new` bails naming the
    /// milestone, so the engine's streaming paths compile today.
    type VideoEncoder = LiveH264Encoder;
    type AudioEncoder = LiveAacEncoder;
    /// L.4: DMA-BUF rings shared with the Slint app.
    type SharedSurface = isocast_surfaces::Surface;

    /// FFmpeg converts its inputs itself, like VideoToolbox does on
    /// macOS, so the engine hands BGRA frames to the writer as they
    /// were captured and skips the per-file GPU conversion stage.
    fn converts_own_inputs(&self) -> bool {
        true
    }

    fn now(&self) -> Time {
        system::host_time()
    }

    /// Absolute waits, so timer coalescing can't stretch them; see
    /// pacing.rs for the measured-stretching story macOS needed.
    fn sleep_until(&self, at: Time) {
        pacing::sleep_until(at);
    }

    fn devices(&self) -> Result<Devices> {
        system::devices()
    }

    fn permissions(&self) -> Result<PermissionsReport> {
        system::permissions_report()
    }

    /// The screen comes from the desktop's portal, and its picker is what
    /// a screen source's open waits on until this machine has answered one
    /// (L.2). A capture that runs past the engine's 8 s bound is
    /// described by that, not by "check its file/device".
    fn source_permission(&self, source: &SourceKind) -> Option<String> {
        // Window and app capture follow the same picker when they land.
        let screen = matches!(
            source,
            SourceKind::Display(_) | SourceKind::Window(_) | SourceKind::Application(_)
        );
        (screen && !portal::has_token()).then(|| {
            "the desktop's screen-sharing picker is waiting for a choice: run `isocast \
             permissions --request` and pick a screen (the choice is remembered, so later \
             captures open silently)"
                .to_string()
        })
    }

    /// On Linux there is no TCC; a source gets its access when it is
    /// first captured, through the desktop's portals (L.2). The
    /// screen-capture portal is the one that asks a person, so this
    /// answers its picker now and keeps the restore token: a take's own
    /// open is bounded to 8 s, which is not long enough to choose a
    /// screen in.
    fn request_permissions(&self) -> Result<PermissionsReport> {
        portal::prepare(true).context(
            "asking the desktop to share a screen (its picker chooses the monitor once; later \
             captures need no prompt)",
        )?;
        system::permissions_report()
    }

    fn open_permission_settings(&self, permission: PermissionKind) -> Result<String> {
        Ok(format!(
            "Linux has no per-app permission settings: {permission:?} access is granted by the \
             desktop portal the first time a source is captured (capture arrives at L.2 of \
             docs/port-omarchy.md)."
        ))
    }

    fn media_info(&self, path: &Path) -> Result<MediaInfo> {
        system::media_info(path)
    }

    /// Media-file sources decode through this machine's FFmpeg, one
    /// child process per part.
    fn open_media(&self, path: &Path, part: MediaPart) -> Result<Self::Media> {
        crate::media::open_media(path, part)
    }

    fn frame_importer(&self, gpu: &isocast_engine::gpu::Gpu) -> Result<VulkanImporter> {
        VulkanImporter::new(gpu)
    }

    fn machine(&self) -> Machine {
        system::machine()
    }

    /// NVENC is the one family here that caps how many encodes run at
    /// once, so it is the one worth asking: the probe starts the encodes
    /// and counts the sessions that open.
    fn concurrent_encodes(&self, codec: Codec, files: u32) -> Option<EncoderSessions> {
        system::concurrent_encodes(codec, files)
    }

    /// Which device a source would record, or an error naming the valid
    /// devices. Synthetic sources need no device.
    fn probe(&self, source: &SourceKind, canvas: &Canvas) -> Result<Option<Probe>> {
        system::probe(source, canvas)
    }

    fn volume(&self, path: &Path) -> Result<Volume> {
        system::volume(path)
    }

    fn write_latency(&self, probe: &Path) -> Option<std::time::Duration> {
        system::write_latency(probe)
    }

    fn frame_pool(&self, width: u32, height: u32) -> Result<CpuPool> {
        Ok(CpuPool::bgra(width, height))
    }

    fn ycbcr_frame_pool(&self, width: u32, height: u32) -> Result<CpuPool> {
        Ok(CpuPool::ycbcr(width, height))
    }

    /// Run the calling thread ahead of background work. Best effort:
    /// without privilege nothing happens, and Linux's timer slack is
    /// small enough that pacing doesn't depend on it.
    fn real_time_thread(&self) {
        pacing::raise_thread();
    }

    /// The calling thread's own CPU time, for the compositor's costs.
    fn thread_cpu_time(&self) -> Option<std::time::Duration> {
        system::thread_cpu_time()
    }

    fn frame_size(&self, frame: &LinuxFrame) -> (u32, u32) {
        frame.size()
    }

    /// L.0 records through BGRA and 8-bit 4:2:0 pools, matching the
    /// macOS pools; a 10-bit frame can't exist until capture asks.
    fn frame_is_ycbcr(&self, frame: &LinuxFrame) -> bool {
        frame.is_ycbcr()
    }

    /// L.4: DMA-BUF rings and the client side of native monitors.
    fn shared_surface(
        &self,
        _gpu: &isocast_engine::gpu::Gpu,
        _width: u32,
        _height: u32,
    ) -> Result<(Self::SharedSurface, wgpu::Texture)> {
        bail!(
            "shared GPU surfaces are only available on macOS so far; the native app's live \
             monitors arrive on Linux at L.4 of docs/port-omarchy.md"
        )
    }

    fn encode_jpeg(
        &self,
        frame: &Self::Frame,
        max_width: Option<u32>,
        quality: f32,
    ) -> Result<(Vec<u8>, u32, u32)> {
        jpeg::encode(frame, max_width, quality)
    }

    /// One media file, encoded by this machine's encoders (NVENC first),
    /// written fragment by fragment so a killed process keeps what it
    /// stored.
    fn create_writer(&self, spec: &WriterSpec) -> Result<FfmpegFileWriter> {
        FfmpegFileWriter::spawn(spec)
    }

    /// L.2: a monitor, shared by the desktop's ScreenCast portal. The
    /// portal's picker chooses which monitor once per machine; the
    /// restore token it hands back makes every later capture silent (see
    /// portal.rs and docs/notes.md).
    fn capture_screen(
        &self,
        target: isocast_engine::ScreenTarget<'_>,
        fps: u32,
        ten_bit: bool,
        cursor: bool,
        sink: VideoSink<Self::Frame>,
    ) -> Result<(Self::Capture, CaptureFormat)> {
        if ten_bit {
            bail!(
                "10-bit screen capture is not available on Linux yet: the portal's ScreenCast \
                 shares 8-bit pictures, so a 10-bit encoder has nothing to take here (L.5)"
            );
        }
        let display = match target {
            isocast_engine::ScreenTarget::Display(display) => display,
            isocast_engine::ScreenTarget::Window { .. }
            | isocast_engine::ScreenTarget::Application { .. } => bail!(
                "capturing a window or an app is not available on Linux yet: L.2 captures a \
                 monitor through the desktop's portal, so point the source at a display"
            ),
        };
        // The portal's own picker decides which monitor is shared, so the
        // name is a check, not an instruction: a name that matches no
        // display is a typo worth failing on (macOS fails on one too, and
        // this way the picker is never shown for it), and a picture whose
        // size disagrees with the named display is worth a word.
        let wanted = system::find_display(display)?;
        let (screen, format) = screencast::capture(*self, fps, cursor, sink)?;
        if (wanted.width, wanted.height) != (format.width, format.height) {
            isocast_diagnostics::diagnostic!(
                "warn"; "screen: the desktop shared a {}x{} picture, but {display:?} is \
                {}x{}; the portal's picker decides which monitor is shared (docs/notes.md)",
                format.width,
                format.height,
                wanted.width,
                wanted.height
            );
        }
        Ok((
            Capture {
                _running: Running::Screen { _screen: screen },
            },
            format,
        ))
    }

    /// L.3: V4L2 cameras, read through this machine's `ffmpeg` (see
    /// camera.rs).
    fn capture_camera(
        &self,
        device: &str,
        request: CameraRequest,
        canvas: &Canvas,
        sink: VideoSink<Self::Frame>,
    ) -> Result<(Self::Capture, CaptureFormat)> {
        let (camera, format) = camera::capture(*self, device, request, canvas, sink)?;
        Ok((
            Capture {
                _running: Running::Camera { _camera: camera },
            },
            format,
        ))
    }

    /// L.3: PipeWire microphone capture.
    fn capture_microphone(&self, _device: &str, _sink: AudioSink) -> Result<(Self::Capture, u32)> {
        bail!(
            "microphone capture is not implemented on Linux yet: it arrives at L.3 of \
             docs/port-omarchy.md (PipeWire)."
        )
    }

    /// L.3: PipeWire's monitor of the default sink for system audio.
    fn capture_system_audio(
        &self,
        _sink: AudioSink,
        _exclude_own: bool,
    ) -> Result<(Self::Capture, u32)> {
        bail!(
            "system-audio capture is not implemented on Linux yet: it arrives at L.3 of \
             docs/port-omarchy.md (PipeWire monitor of the default sink)."
        )
    }

    /// Audio playback on an output device, L.3.
    fn play_audio(&self, _device: &str, _render: AudioRender) -> Result<(Self::Playback, String)> {
        bail!(
            "audio monitoring is not implemented on Linux yet: it arrives at L.3 of \
             docs/port-omarchy.md (PipeWire playback)."
        )
    }

    /// Browser pages are L.6 (WebKitGTK or WPE).
    fn capture_browser(
        &self,
        _page: &isocast_engine::BrowserPage<'_>,
        _video: VideoSink<Self::Frame>,
        _audio: Option<AudioSink>,
    ) -> Result<(
        Self::Capture,
        CaptureFormat,
        Arc<dyn isocast_engine::BrowserControl>,
    )> {
        bail!(
            "browser sources are not implemented on Linux yet: they arrive at L.6 of \
             docs/port-omarchy.md (WebKitGTK or WPE)."
        )
    }

    /// Per-app audio by linking to the app's stream node, L.3.
    fn capture_application_audio(
        &self,
        _app: &str,
        _sink: AudioSink,
    ) -> Result<(Self::Capture, u32)> {
        bail!(
            "per-application audio capture is not implemented on Linux yet: it arrives at \
             L.3 of docs/port-omarchy.md"
        )
    }

    fn create_video_encoder(&self, spec: &LiveVideoSpec) -> Result<LiveH264Encoder> {
        LiveH264Encoder::new(spec)
    }

    fn create_audio_encoder(&self, spec: &LiveAudioSpec) -> Result<LiveAacEncoder> {
        LiveAacEncoder::new(spec)
    }

    // ---- Secrets ------------------------------------------------------

    /// The Keychain stand-in is a 0600 file in the support folder; the
    /// OS store (secret-service / kwallet) is a later task.
    fn has_secret(&self, service: &str, account: &str) -> Result<bool> {
        keychain::contains(service, account)
    }

    fn secret(&self, service: &str, account: &str) -> Result<Option<String>> {
        keychain::get(service, account)
    }

    fn set_secret(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        keychain::set(service, account, secret)
    }

    fn delete_secret(&self, service: &str, account: &str) -> Result<bool> {
        keychain::remove(service, account)
    }

    // ---- Joins --------------------------------------------------------

    /// Timeline exports are L.5.
    fn join_files(&self, _spec: &JoinSpec, _progress: OnJoinProgress<'_>) -> Result<Joined> {
        bail!(
            "joining files isn't supported on this platform yet; timeline exports arrive at \
             L.5 of docs/port-omarchy.md"
        )
    }
}
