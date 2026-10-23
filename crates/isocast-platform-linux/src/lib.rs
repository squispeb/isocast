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

pub(crate) mod audio;
pub(crate) mod browser;
pub(crate) mod camera;
mod fragments;
pub(crate) mod frames;
pub(crate) mod gpu;
pub mod hotkeys;
pub(crate) mod join;
mod jpeg;
mod keychain;
pub(crate) mod media;
pub(crate) mod pacing;
mod playback;
mod portal;
mod recode;
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
    /// A microphone, an output's monitor or an app's own streams
    /// (audio.rs).
    Audio { _audio: audio::AudioCapture },
    /// A web page rendered by WebKitGTK (browser.rs, L.6a).
    Browser { _page: browser::BrowserCapture },
}

impl Platform for LinuxPlatform {
    type Frame = LinuxFrame;
    type FramePool = CpuPool;
    type Writer = FfmpegFileWriter;
    type Capture = Capture;
    type Importer = VulkanImporter;
    type Media = crate::media::MediaReader;
    type Playback = playback::Playback;
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

    /// L.6e: the virtual camera will be a v4l2loopback device; until
    /// then it is refused by name rather than as an unknown backend.
    fn virtual_camera_sink(
        &self,
        _width: u32,
        _height: u32,
        _fps: u32,
    ) -> Result<Box<dyn isocast_engine::record::CameraSink<LinuxFrame>>> {
        bail!(
            "virtual_camera: not available on Linux yet; it arrives as a v4l2loopback camera \
             (L.6e in PLAN.md), which needs the v4l2loopback kernel module installed"
        )
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
    fn request_permissions(&self, only: Option<PermissionKind>) -> Result<PermissionsReport> {
        // Only the screen has a prompt (the portal's picker); asking for
        // anything else has nothing to show.
        if only.is_some_and(|kind| kind != PermissionKind::ScreenRecording) {
            return system::permissions_report();
        }
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

    /// Same device number, same file system: what `volume`'s free space
    /// is counted on.
    fn same_volume(&self, left: &Path, right: &Path) -> Result<bool> {
        use std::os::unix::fs::MetadataExt;
        Ok(std::fs::metadata(left)?.dev() == std::fs::metadata(right)?.dev())
    }

    fn process_cpu_seconds(&self) -> Option<f64> {
        system::process_cpu_seconds()
    }

    fn system_cpu_seconds(&self) -> Option<f64> {
        system::system_cpu_seconds()
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

    /// L.4: a monitor's picture for clients on this machine, through a
    /// ring of shared surfaces. The surface is shared memory the client
    /// maps; the texture is what the compositor draws into, and
    /// [`Self::publish_shared_surface`] copies it back into that memory
    /// once the frame is drawn — Linux has no way to let another
    /// process's GPU sample this texture in place yet (that is DMA-BUF,
    /// L.4.6 in docs/port-omarchy.md), so the first version copies.
    fn shared_surface(
        &self,
        gpu: &isocast_engine::gpu::Gpu,
        width: u32,
        height: u32,
    ) -> Result<(Self::SharedSurface, wgpu::Texture)> {
        let surface = isocast_surfaces::Surface::rgba(width, height)?;
        let texture = surface.texture(
            &gpu.device,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        )?;
        Ok((surface, texture))
    }

    /// L.4: the drawn picture out to the shared memory the client reads.
    /// The rows come back with the padding a 256-byte-aligned read-back
    /// needs, which is exactly the surface's own stride, so the picture
    /// goes into it without repacking a row.
    fn publish_shared_surface(
        &self,
        gpu: &isocast_engine::gpu::Gpu,
        texture: &wgpu::Texture,
        surface: &isocast_surfaces::Surface,
    ) -> Result<()> {
        let size = wgpu::Extent3d {
            width: surface.width(),
            height: surface.height(),
            depth_or_array_layers: 1,
        };
        let (pixels, _) = gpu::copy_out(
            &gpu.device,
            &gpu.queue,
            texture,
            u64::from(surface.width()) * 4,
            size,
        )?;
        surface.publish(&pixels)
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

    /// L.S1f: a fragmented file an interruption tore (a fragment's `moof`
    /// without its whole `mdat`, as a power loss leaves) is cut back to
    /// its last whole fragment, which every player reads. A file whose
    /// fragments are all whole is a movie already (`None`), as is one
    /// that isn't fragmented. Only a regular file is touched, never a
    /// link, and only once its size has stopped changing: a writer that
    /// outlived its daemon may still be flushing its tail.
    fn recover_file(
        &self,
        folder: &Path,
        name: &std::ffi::OsStr,
    ) -> Result<Option<isocast_engine::RecoveredFile>> {
        let path = folder.join(name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        if !metadata.file_type().is_file() {
            bail!(
                "{} is not a regular file; it is left as it is",
                path.display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        let length = std::fs::metadata(&path)?.len();
        if length != metadata.len() {
            return Ok(None);
        }
        let mut scan = fragments::Scan::default();
        scan.advance(&path)?;
        if !scan.fragmented() || scan.whole_end >= length {
            return Ok(None);
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {} to cut its torn fragment", path.display()))?;
        file.set_len(scan.whole_end)?;
        file.sync_all()?;
        Ok(Some(isocast_engine::RecoveredFile {
            fragments: scan.fragments,
            frames: scan.frames(),
            seconds: Some(scan.covered().as_secs_f64()),
            summary: format!(
                "{}; left out: {} bytes of a fragment that didn't reach the disk whole",
                scan.summary(),
                length - scan.whole_end
            ),
        }))
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

    /// L.3: a microphone through PipeWire (see audio.rs). A one- or
    /// two-channel device is captured as it is; a larger one, or one
    /// whose `channels` are picked, is captured whole and only the picked
    /// inputs (by default 1 + 2) go on, as on macOS.
    fn capture_microphone(
        &self,
        device: &str,
        channels: Option<&[u32]>,
        sink: AudioSink,
    ) -> Result<(Self::Capture, u32)> {
        let microphone = system::find_microphone(device)?;
        let (request, pick) = system::microphone_channels(&microphone, channels)?;
        let sink = match &pick {
            Some(pick) => audio::picking(sink, request as usize, pick.clone()),
            None => sink,
        };
        let (audio, delivered) = audio::capture(
            *self,
            audio::Target::Microphone(microphone.id.clone()),
            request,
            sink,
        )
        .with_context(|| {
            format!(
                "capturing microphone {} (id {})",
                microphone.name, microphone.id
            )
        })?;
        let channels = match &pick {
            Some(_) if delivered != request => bail!(
                "microphone {} (id {}) delivered {delivered} channel(s), not the {request} its \
                 picked inputs are counted in",
                microphone.name,
                microphone.id
            ),
            Some(pick) => pick.len() as u32,
            None => delivered,
        };
        isocast_diagnostics::debug!(
            "microphone: {} ({}): {channels} channel(s)",
            microphone.name,
            microphone.id
        );
        Ok((
            Capture {
                _running: Running::Audio { _audio: audio },
            },
            channels,
        ))
    }

    /// L.3: what the machine plays, from the default output's *monitor*
    /// ports (audio.rs), as stereo.
    fn capture_system_audio(
        &self,
        exclude: &[String],
        sink: AudioSink,
        exclude_own: bool,
    ) -> Result<(Self::Capture, u32)> {
        // The default output's monitor carries every app's sound mixed,
        // so leaving apps out needs per-stream links (the per-app step's
        // way); recording what the setup said to leave out would be worse
        // than not recording.
        if !exclude.is_empty() {
            bail!(
                "exclude: leaving apps out of system audio is not available on Linux yet (the \
                 output's monitor carries every app mixed); remove {exclude:?} from the source's \
                 exclude, or record the apps you want as application audio sources"
            );
        }
        let output = system::default_output()?;
        if exclude_own {
            // What this process plays (monitoring) goes through the same
            // sink, and a sink's monitor carries everything played on it.
            // Exclusion is what the per-app step (linking a capture to
            // every stream but ours) can do and this cannot; macOS gets
            // it from ScreenCaptureKit.
            isocast_diagnostics::diagnostic!(
                "warn"; "system audio: monitoring is on, and what the machine plays is captured \
                from {output}'s monitor, which carries Isocast's own monitoring too; the monitor \
                is not filtered (docs/notes.md)"
            );
        }
        let (audio, channels) =
            audio::capture(*self, audio::Target::Output(output.clone()), 2, sink)
                .with_context(|| format!("capturing what {output} plays"))?;
        isocast_diagnostics::debug!("system audio: {output}: {channels} channel(s)");
        Ok((
            Capture {
                _running: Running::Audio { _audio: audio },
            },
            channels,
        ))
    }

    /// L.S1c: monitoring and the editor's sound, through a PipeWire
    /// output stream (playback.rs).
    fn play_audio(&self, device: &str, render: AudioRender) -> Result<(Self::Playback, String)> {
        playback::play(device, render)
    }

    /// L.6a: a web page rendered by WebKitGTK in an offscreen window
    /// and snapshotted into BGRA frames (browser/); L.6b: with `audio`,
    /// the page's sound goes there instead of playing (audio.js).
    fn capture_browser(
        &self,
        page: &isocast_engine::BrowserPage<'_>,
        video: VideoSink<Self::Frame>,
        audio: Option<AudioSink>,
    ) -> Result<(
        Self::Capture,
        CaptureFormat,
        Arc<dyn isocast_engine::BrowserControl>,
    )> {
        let (capture, format, control) = browser::capture_browser(page, video, audio)?;
        Ok((
            Capture {
                _running: Running::Browser { _page: capture },
            },
            format,
            control,
        ))
    }

    /// L.3: one app's own sound, by linking a capture to the streams the
    /// app plays through (audio.rs) — an app has no node of its own, and
    /// the session manager will not link a capture to a playback stream.
    /// An app is captured as stereo, the way macOS captures one.
    fn capture_application_audio(
        &self,
        app: &str,
        sink: AudioSink,
    ) -> Result<(Self::Capture, u32)> {
        // Resolved before the capture opens, so a setup naming an app that
        // is playing nothing gets the list of what is (the capture itself
        // would only sit there with no format, which is a worse message).
        let (name, streams) = audio::find_app(app)?;
        let (audio, channels) = audio::capture(
            *self,
            audio::Target::Application {
                query: app.to_string(),
                name: name.clone(),
            },
            2,
            sink,
        )
        .with_context(|| {
            format!(
                "capturing the sound of {name} ({} stream(s) playing: {})",
                streams.len(),
                streams
                    .iter()
                    .map(audio::AppStream::describe)
                    .collect::<Vec<String>>()
                    .join(", ")
            )
        })?;
        isocast_diagnostics::debug!(
            "app audio: {name}: {channels} channel(s), {} stream(s)",
            streams.len()
        );
        Ok((
            Capture {
                _running: Running::Audio { _audio: audio },
            },
            channels,
        ))
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
    fn join_files(&self, spec: &JoinSpec, progress: OnJoinProgress<'_>) -> Result<Joined> {
        join::join(spec, progress)
    }
}
