//! Camera capture: V4L2 (UVC webcams, capture cards) read through this
//! machine's own `ffmpeg`, the way writer.rs encodes — the repo's
//! decision is system ffmpeg until L.5's in-process libav/NVENC.
//!
//! What a camera offers comes from the device's own enumeration
//! (VIDIOC_ENUM_FMT / ENUM_FRAMESIZES / ENUM_FRAMEINTERVALS), so a
//! request the device can't meet is answered with the nearest format it
//! has and the format actually captured is the one reported to the
//! engine. MJPEG is preferred over YUYV at the same size and rate: it is
//! what makes 1280×720@30 and 1080p possible over USB 2 on a UVC webcam,
//! and ffmpeg decodes it for us either way; YUYV is the fallback for
//! cameras without it.
//!
//! ffmpeg decodes the camera into raw BGRA on its stdout, and this module
//! reads those bytes straight into the engine's own pool frames. That
//! costs the decoder process and one copy per frame (the decode), where
//! dequeueing V4L2's mmap'd buffers into pool frames in-process would
//! cost none: that is the L.3 follow-up if this copy ever shows in the
//! cost numbers, not a decision to make before then.
//!
//! Raw pipes carry no timestamps, so every frame is stamped with the
//! platform's host clock as its bytes arrive, the rule the writer's raw
//! input and the audio captures run on too.

use std::ffi::CString;
use std::io::{ErrorKind, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{CameraRequest, CaptureFormat, FramePool, Platform, VideoSink};
use isocast_protocol::Canvas;

use crate::LinuxPlatform;
use crate::frames::{CpuPool, LinuxFrame};

/// How long the camera has to deliver its first picture once ffmpeg has
/// started: a UVC camera answers in well under a second, and the engine
/// gives a whole source open 8 s.
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(4);
/// How much of ffmpeg's stderr is kept: its last words, for the reason a
/// capture ended and for the error when it never started.
const STDERR_TAIL: usize = 8 * 1024;
/// Frames the pool was too busy to take are reported at most this often,
/// so a stalled consumer shows in the log without flooding it.
const DROP_REPORT: Duration = Duration::from_secs(5);
/// How long the wait for the first picture sleeps between checks.
const START_POLL: Duration = Duration::from_millis(5);

/// A running camera capture. Dropping it stops capture for real: the
/// ffmpeg child is killed and waited for, and the threads reading it are
/// joined.
pub struct CameraCapture {
    /// Set when this handle is dropped, so a child that ends afterwards
    /// is a stop rather than something to report through `sink.stopped`.
    stopping: Arc<AtomicBool>,
    /// The ffmpeg child. Shared with the reading thread, which reaps it
    /// when it ends on its own; whoever takes it first waits for it.
    child: Arc<Mutex<Option<Child>>>,
    threads: Vec<JoinHandle<()>>,
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        kill_and_wait(&self.child);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// Start capturing the camera `query` names: an id, a name or part of a
/// name from `Platform::devices`, or a `/dev/videoN` node.
pub fn capture(
    platform: LinuxPlatform,
    query: &str,
    request: CameraRequest,
    canvas: &Canvas,
    sink: VideoSink<LinuxFrame>,
) -> Result<(CameraCapture, CaptureFormat)> {
    let camera = Camera::resolve(query)?;
    let offers = camera
        .offers()
        .with_context(|| format!("camera {} ({})", camera.name, camera.node.display()))?;
    let chosen = choose(&offers, request, canvas)
        .with_context(|| format!("camera {} ({})", camera.name, camera.node.display()))?;
    isocast_diagnostics::debug!(
        "camera: {} ({}): capturing {}x{} @ {} fps as {:?} (canvas {}x{} @ {} fps)",
        camera.name,
        camera.node.display(),
        chosen.width,
        chosen.height,
        chosen.rate.ffmpeg(),
        chosen.pixel,
        canvas.width,
        canvas.height,
        canvas.fps
    );
    let mut child = spawn_ffmpeg(&camera, &chosen)?;
    let stdout = child.stdout.take().expect("ffmpeg's stdout was piped");
    let stderr = child.stderr.take().expect("ffmpeg's stderr was piped");
    let stopping = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let child = Arc::new(Mutex::new(Some(child)));
    let tail = Arc::new(Mutex::new(Vec::new()));
    // What ffmpeg's exit turned out to be, recorded by the reader: it is
    // the only thread that reaps the child, and the wait below must still
    // be able to say *why* a camera that cannot be captured said no.
    let reaped: Arc<Mutex<Option<String>>> = Arc::default();

    // A capture that fails from here on must not leave ffmpeg holding
    // the camera: every error path below drops this handle.
    let mut capture = CameraCapture {
        stopping: stopping.clone(),
        child: child.clone(),
        threads: Vec::new(),
    };
    let name = camera.name.clone();
    let stderr_thread = isocast_diagnostics::spawn(
        "camera ffmpeg stderr",
        {
            let tail = tail.clone();
            move || read_stderr(stderr, tail)
        },
        |why| {
            isocast_diagnostics::diagnostic!(
                "error"; "the camera's ffmpeg stderr reader stopped: {why}"
            );
        },
    );
    match stderr_thread {
        Ok(thread) => capture.threads.push(thread),
        Err(err) => {
            drop(capture);
            return Err(err).context("the camera's ffmpeg stderr reader could not be started");
        }
    }
    let reader = isocast_diagnostics::spawn(
        "camera capture",
        {
            let (mut stdout, child, tail, name) =
                (stdout, child.clone(), tail.clone(), name.clone());
            let (started, stopping) = (started.clone(), stopping.clone());
            let (platform, sink) = (platform, sink);
            let ended_of = reaped.clone();
            move || {
                let mut pool = CpuPool::bgra(chosen.width, chosen.height);
                let end = pump(
                    &mut stdout,
                    platform,
                    (chosen.width, chosen.height),
                    &mut pool,
                    &sink,
                    &started,
                    &stopping,
                );
                ended(end, &sink, &stopping, &child, &tail, &name, &ended_of);
            }
        },
        |why| {
            isocast_diagnostics::diagnostic!("error"; "the camera capture reader stopped: {why}");
        },
    );
    match reader {
        Ok(thread) => capture.threads.push(thread),
        Err(err) => {
            drop(capture);
            return Err(err).context("the camera capture reader could not be started");
        }
    }
    // The camera's first picture, so a camera that can't be captured (in
    // use by another program, unplugged, a format the driver refuses)
    // fails the source's open with ffmpeg's own words rather than a
    // recording that never gets a frame.
    let deadline = Instant::now() + FIRST_FRAME_WAIT;
    loop {
        if started.load(Ordering::SeqCst) {
            return Ok((
                capture,
                CaptureFormat {
                    width: chosen.width,
                    height: chosen.height,
                    fps: chosen.rate.rounded(),
                },
            ));
        }
        if let Some(status) = exited(&child).or_else(|| end_status(&reaped)) {
            let tail = tail_text(&tail);
            let hint = if tail.contains("busy") {
                "; another program is using the camera (PipeWire's camera node for OBS or a \
                 browser): close that capture and try again"
            } else {
                ""
            };
            drop(capture);
            bail!(
                "camera {} ({}) couldn't be captured: ffmpeg {status}{hint}{}",
                name,
                camera.node.display(),
                said(&tail)
            );
        }
        if Instant::now() >= deadline {
            let tail = tail_text(&tail);
            drop(capture);
            bail!(
                "camera {} ({}) sent no picture within {} s, so it was stopped{}",
                name,
                camera.node.display(),
                FIRST_FRAME_WAIT.as_secs(),
                said(&tail)
            );
        }
        thread::sleep(START_POLL);
    }
}

/// The ffmpeg that reads the camera and writes BGRA frames to its
/// stdout: the device's own pictures at the rate it gives them, not
/// ffmpeg's idea of the rate (a raw pipe carries no timestamps, so
/// ffmpeg's default conversion to a constant rate would repeat and drop
/// frames the capture never sees).
fn spawn_ffmpeg(camera: &Camera, chosen: &Chosen) -> Result<Child> {
    let program = crate::writer::ffmpeg_binary();
    let mut command = Command::new(&program);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .arg("-f")
        .arg("v4l2")
        .arg("-input_format")
        .arg(chosen.pixel.input_format())
        .arg("-video_size")
        .arg(format!("{}x{}", chosen.width, chosen.height))
        .arg("-framerate")
        .arg(chosen.rate.ffmpeg())
        .arg("-i")
        .arg(&camera.node)
        .arg("-fps_mode")
        .arg("passthrough")
        .arg("-pix_fmt")
        .arg("bgra")
        .arg("-f")
        .arg("rawvideo")
        .arg("-");
    command.spawn().with_context(|| {
        format!(
            "could not start ffmpeg ({}), which captures cameras on Linux: is it installed, or \
             does ISOCAST_FFMPEG name one?",
            program.to_string_lossy()
        )
    })
}

/// Why a frame pump returned: the capture is being dropped, the camera
/// ran out, or reading failed.
enum End {
    /// The handle is being dropped: stop, and report nothing.
    Stopped,
    /// The stream ended: the camera or ffmpeg went away.
    Ended,
    /// Reading a frame failed.
    Failed(String),
}

/// Read `width`×`height` BGRA frames from `read` into pool frames and
/// hand each to `sink`, stamped with the host clock as its bytes arrive.
fn pump(
    read: &mut impl Read,
    platform: LinuxPlatform,
    size: (u32, u32),
    pool: &mut CpuPool,
    sink: &VideoSink<LinuxFrame>,
    started: &AtomicBool,
    stopping: &AtomicBool,
) -> End {
    let (width, height) = size;
    let row = width as usize * 4;
    let frame_bytes = row * height as usize;
    // Every frame in the pool still downstream: this frame is dropped,
    // but its bytes must still leave the pipe, so they land here.
    let mut scratch: Vec<u8> = Vec::new();
    let mut dropped = DropReport::default();
    loop {
        if stopping.load(Ordering::SeqCst) {
            return End::Stopped;
        }
        let mut read_error = None;
        let taken = pool.draw(&mut |pixels, stride| {
            if let Err(err) = read_frame(read, pixels, stride, width, height) {
                read_error = Some(err);
            }
        });
        match taken {
            Ok(Some(frame)) => {
                if let Some(err) = read_error {
                    return unread(err);
                }
                started.store(true, Ordering::SeqCst);
                // The host clock, stamped where the bytes arrived: the
                // camera's own timestamps are gone by now.
                (sink.frame)(frame, platform.now());
            }
            Ok(None) => {
                if scratch.is_empty() {
                    scratch.resize(frame_bytes, 0);
                }
                if let Err(err) = read_frame(read, &mut scratch, row, width, height) {
                    return unread(err);
                }
                dropped.count();
            }
            Err(err) => return End::Failed(format!("{err:#}")),
        }
    }
}

/// How a stream that ended before its frame did reads: a stream that ran
/// out is an ended capture, anything else is a failure.
fn unread(err: std::io::Error) -> End {
    if err.kind() == ErrorKind::UnexpectedEof {
        End::Ended
    } else {
        End::Failed(format!("{err}"))
    }
}

/// One `width`×`height` BGRA frame from `read` into `pixels`, whose rows
/// are `stride` bytes apart (the pool's own rows, which may be padded).
fn read_frame(
    read: &mut impl Read,
    pixels: &mut [u8],
    stride: usize,
    width: u32,
    height: u32,
) -> std::io::Result<()> {
    let row = width as usize * 4;
    let height = height as usize;
    let needed = height.saturating_sub(1) * stride + row;
    if pixels.len() < needed {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "a {width}×{height} BGRA picture at stride {stride} needs {needed} bytes and the \
                 frame's plane holds {}",
                pixels.len()
            ),
        ));
    }
    if stride == row {
        return read.read_exact(&mut pixels[..row * height]);
    }
    for y in 0..height {
        read.read_exact(&mut pixels[y * stride..y * stride + row])?;
    }
    Ok(())
}

/// Frames the pool was too busy to take: counted, and reported at most
/// every [`DROP_REPORT`], the way macOS's capture output reports the
/// frames it discarded.
#[derive(Default)]
struct DropReport {
    since: Option<Instant>,
    count: u64,
}

impl DropReport {
    fn count(&mut self) {
        let since = *self.since.get_or_insert_with(Instant::now);
        self.count += 1;
        let elapsed = since.elapsed();
        if elapsed >= DROP_REPORT {
            isocast_diagnostics::diagnostic!(
                "warn"; "camera: dropped {} frame(s) in {:.0} s: every frame in the pool is still \
                downstream",
                self.count,
                elapsed.as_secs_f64()
            );
            self.since = Some(Instant::now());
            self.count = 0;
        }
    }
}

/// Report a capture that ended on its own, with what ffmpeg said: its
/// exit, and its stderr's last words.
fn ended(
    end: End,
    sink: &VideoSink<LinuxFrame>,
    stopping: &AtomicBool,
    child: &Mutex<Option<Child>>,
    tail: &Mutex<Vec<u8>>,
    name: &str,
    reaped: &Mutex<Option<String>>,
) {
    if stopping.load(Ordering::SeqCst) {
        // A deliberate stop: nothing stopped on its own.
        return;
    }
    let reason = match end {
        End::Stopped => return,
        End::Failed(why) => format!("camera {name:?} stopped: {why}"),
        End::Ended => {
            let status = match reap(child) {
                Some(status) => format!("{status}"),
                None => "ended".to_string(),
            };
            // The wait for the first picture reads this: without it, a
            // camera that died at once looked like one that never spoke.
            *reaped.lock().expect("the camera's end is poisoned") = Some(status.clone());
            format!("camera {name:?} stopped: ffmpeg {status}")
        }
    };
    let tail = tail_text(tail);
    (sink.stopped)(format!("{reason}{}", said(&tail)));
}

/// `; it said: <ffmpeg's last words>`, for a reason that carries some.
fn said(tail: &str) -> String {
    if tail.is_empty() {
        String::new()
    } else {
        format!("; ffmpeg said: {tail}")
    }
}

/// Drain ffmpeg's stderr into `tail`, keeping its last [`STDERR_TAIL`]
/// bytes: a chatty ffmpeg must not deadlock on a pipe nobody reads.
/// Shared with the media-file decoding, which runs the same children.
pub(crate) fn read_stderr(mut stderr: ChildStderr, tail: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0u8; 4096];
    loop {
        let read = match stderr.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => read,
            Err(_) => return,
        };
        let mut tail = tail.lock().expect("the camera's ffmpeg stderr is poisoned");
        tail.extend_from_slice(&chunk[..read]);
        let excess = tail.len().saturating_sub(STDERR_TAIL);
        if excess > 0 {
            tail.drain(..excess);
        }
    }
}

/// What ffmpeg said last, trimmed.
fn tail_text(tail: &Mutex<Vec<u8>>) -> String {
    let tail = tail.lock().expect("the camera's ffmpeg stderr is poisoned");
    String::from_utf8_lossy(&tail).trim().to_string()
}

/// Whether the child has ended already, without waiting for it.
fn exited(child: &Mutex<Option<Child>>) -> Option<String> {
    let mut guard = child.lock().expect("the camera's ffmpeg child is poisoned");
    let child = guard.as_mut()?;
    match child.try_wait() {
        Ok(Some(status)) => Some(format!("{status}")),
        _ => None,
    }
}

/// The status the reader reaped the child with, if it got there first.
fn end_status(ended: &Mutex<Option<String>>) -> Option<String> {
    ended.lock().expect("the camera's end is poisoned").clone()
}

/// Wait for the child and take it, so nothing kills it afterwards.
fn reap(child: &Mutex<Option<Child>>) -> Option<std::process::ExitStatus> {
    let mut child = child
        .lock()
        .expect("the camera's ffmpeg child is poisoned")
        .take()?;
    child.wait().ok()
}

/// Kill the child and wait for it, so no ffmpeg keeps reading the camera
/// after its capture is dropped. A reading thread that got there first (a
/// camera that ended by itself) leaves nothing to kill.
fn kill_and_wait(child: &Mutex<Option<Child>>) {
    let mut guard = child.lock().expect("the camera's ffmpeg child is poisoned");
    if let Some(child) = guard.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    *guard = None;
}

/// A camera: the stable id `/dev/v4l/by-id` gives it, the node that
/// captures, and the name the kernel gives it.
#[derive(Clone)]
struct Camera {
    /// The id a setup names it by, like
    /// `usb-046d_HD_Pro_Webcam_C920_31CFD3EF-video-index0`.
    id: String,
    /// The node the id points at: `/dev/videoN`.
    node: PathBuf,
    /// The kernel's name for it, like `HD Pro Webcam C920`.
    name: String,
}

impl Camera {
    /// The camera `query` names: an id, a name or part of one, or a
    /// `/dev/videoN` node. An error names the cameras this machine has.
    fn resolve(query: &str) -> Result<Self> {
        let cameras = attached();
        if let Some(camera) = cameras.iter().find(|camera| camera.id == query) {
            return Ok(camera.clone());
        }
        let wanted = query.to_lowercase();
        if let Some(camera) = cameras
            .iter()
            .find(|camera| camera.name.to_lowercase() == wanted)
        {
            return Ok(camera.clone());
        }
        // A node the caller named itself, with or without a by-id link.
        let node = Path::new(query);
        if node.starts_with("/dev/video") && node.exists() {
            return Ok(Self {
                id: query.to_string(),
                name: name_of(node),
                node: node.to_path_buf(),
            });
        }
        let matching: Vec<&Camera> = cameras
            .iter()
            .filter(|camera| camera.matches(&wanted))
            .collect();
        match matching.as_slice() {
            [camera] => Ok((*camera).clone()),
            [] => bail!(
                "no camera matches {query:?}; cameras: {}",
                describe(&cameras)
            ),
            several => bail!(
                "{query:?} matches {} cameras: {}",
                several.len(),
                several
                    .iter()
                    .map(|camera| format!("{} ({})", camera.id, camera.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Whether `wanted` (lower case) names this camera: its id, its name
    /// or part of either, or its node's own name (`video0`).
    fn matches(&self, wanted: &str) -> bool {
        self.id.to_lowercase().contains(wanted)
            || self.name.to_lowercase().contains(wanted)
            || self.node.to_string_lossy().to_lowercase().contains(wanted)
    }

    /// Every format the device offers, with the sizes and rates of the
    /// ones Isocast captures, and the names of all of them for an error
    /// that lists the valid options.
    fn offers(&self) -> Result<Offers> {
        let node = open_capture_node(&self.node)?;
        let fd = node.as_raw_fd();
        let capabilities = capability(fd)?;
        let capabilities = if capabilities.device_caps != 0 {
            capabilities.device_caps
        } else {
            capabilities.capabilities
        };
        if capabilities & (CAP_VIDEO_CAPTURE | CAP_VIDEO_CAPTURE_MPLANE) == 0 {
            bail!(
                "{} is not a video capture device (its capabilities are {capabilities:#010x}); \
                 the by-id id ending in -video-index0 is the one that captures",
                self.node.display()
            );
        }
        let mut offers = Offers {
            list: Vec::new(),
            formats: Vec::new(),
        };
        for index in 0.. {
            let mut format = Fmtdesc {
                index,
                kind: BUF_TYPE_VIDEO_CAPTURE,
                flags: 0,
                description: [0; 32],
                pixelformat: 0,
                mbus_code: 0,
                reserved: [0; 3],
            };
            match ioctl(fd, VIDIOC_ENUM_FMT, &mut format) {
                Ok(()) => {}
                Err(err) if list_ended(&err) => break,
                Err(err) => return Err(err).context("VIDIOC_ENUM_FMT"),
            }
            let described = cstr(&format.description);
            if !offers.formats.contains(&described) {
                offers.formats.push(described);
            }
            let Some(pixel) = Pixel::from_fourcc(format.pixelformat) else {
                continue;
            };
            for size_index in 0.. {
                let mut sizes = Frmsizeenum {
                    index: size_index,
                    pixel_format: format.pixelformat,
                    kind: 0,
                    sizes: [0; 6],
                    reserved: [0; 2],
                };
                match ioctl(fd, VIDIOC_ENUM_FRAMESIZES, &mut sizes) {
                    Ok(()) => {}
                    Err(err) if list_ended(&err) => break,
                    Err(err) => return Err(err).context("VIDIOC_ENUM_FRAMESIZES"),
                }
                let Some((width, height)) = sizes_dims(&sizes) else {
                    continue;
                };
                let mut rates = Vec::new();
                for interval_index in 0.. {
                    let mut intervals = Frmivalenum {
                        index: interval_index,
                        pixel_format: format.pixelformat,
                        width,
                        height,
                        kind: 0,
                        intervals: [0; 6],
                        reserved: [0; 2],
                    };
                    match ioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS, &mut intervals) {
                        Ok(()) => {}
                        Err(err) if list_ended(&err) => break,
                        Err(err) => return Err(err).context("VIDIOC_ENUM_FRAMEINTERVALS"),
                    }
                    for rate in interval_rates(&intervals) {
                        if !rates.contains(&rate) {
                            rates.push(rate);
                        }
                    }
                }
                if !rates.is_empty() {
                    offers.list.push(Offer {
                        pixel,
                        width,
                        height,
                        rates,
                    });
                }
            }
        }
        Ok(offers)
    }
}

/// The cameras this machine has, from `/dev/v4l/by-id`: each device's
/// `-video-index0` link is the node that captures (its other index is
/// that device's metadata node), and its name comes from sysfs.
fn attached() -> Vec<Camera> {
    let mut cameras = Vec::new();
    let Ok(entries) = std::fs::read_dir("/dev/v4l/by-id") else {
        return cameras;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let id = entry.file_name().to_string_lossy().to_string();
        if !id.ends_with("-video-index0") {
            continue;
        }
        // The by-id name is relative and unnormalized: resolve it to the
        // node it really is.
        let Ok(node) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        cameras.push(Camera {
            id,
            name: name_of(&node),
            node,
        });
    }
    cameras
}

/// The kernel's name for a video node (`/dev/video0` → `HD Pro Webcam
/// C920`): `/sys/class/video4linux/video0/name`, else the node's own
/// name.
fn name_of(node: &Path) -> String {
    let Some(file) = node.file_name() else {
        return node.display().to_string();
    };
    std::fs::read_to_string(Path::new("/sys/class/video4linux").join(file).join("name"))
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| file.to_string_lossy().to_string())
}

/// The cameras as one line for an error, so a wrong id can be corrected.
fn describe(cameras: &[Camera]) -> String {
    if cameras.is_empty() {
        return "none are attached (no /dev/v4l/by-id/*-video-index0 nodes); plug a camera in and \
                run `isocast devices`"
            .to_string();
    }
    cameras
        .iter()
        .map(|camera| format!("{} ({})", camera.id, camera.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a camera offers: the pixel formats Isocast captures, at the sizes
/// and rates the device gives them, and every format the device named.
struct Offers {
    list: Vec<Offer>,
    formats: Vec<String>,
}

/// One size a camera offers in one pixel format, and the rates there.
struct Offer {
    pixel: Pixel,
    width: u32,
    height: u32,
    rates: Vec<Rate>,
}

/// What to capture, exactly as the device will be asked for it.
#[derive(Debug)]
struct Chosen {
    pixel: Pixel,
    width: u32,
    height: u32,
    rate: Rate,
}

/// The format to capture in, from the device's own offers. A request
/// that names a size or rate the device doesn't have gets the nearest it
/// does, and `CaptureFormat` reports what was really chosen; with no
/// request the choice follows the canvas the way macOS's does: the
/// highest rate up to the canvas rate, then a size that fits on the
/// canvas, then the largest such size, then the canvas's own shape.
fn choose(offers: &Offers, request: CameraRequest, canvas: &Canvas) -> Result<Chosen> {
    if offers.list.is_empty() {
        bail!(
            "it offers no format Isocast can capture (the device names {}); Isocast captures \
             Motion-JPEG and YUYV cameras",
            if offers.formats.is_empty() {
                "no formats at all".to_string()
            } else {
                offers.formats.join(", ")
            }
        );
    }
    // The sizes to choose between: the requested one where the device has
    // it, else every size it has, which the score below then sorts by
    // closeness to the request.
    let sizes: Vec<&Offer> = match request.size {
        Some(wanted) => {
            let exact: Vec<&Offer> = offers
                .list
                .iter()
                .filter(|offer| (offer.width, offer.height) == wanted)
                .collect();
            if exact.is_empty() {
                offers.list.iter().collect()
            } else {
                exact
            }
        }
        None => offers.list.iter().collect(),
    };
    let mut best: Option<(Score, Chosen)> = None;
    for offer in sizes {
        for &rate in &offer.rates {
            let score = score_of(offer, rate, request, canvas);
            if best.as_ref().is_none_or(|(best, _)| score > *best) {
                best = Some((
                    score,
                    Chosen {
                        pixel: offer.pixel,
                        width: offer.width,
                        height: offer.height,
                        rate,
                    },
                ));
            }
        }
    }
    best.map(|(_, chosen)| chosen)
        .ok_or_else(|| anyhow!("it offers no size and rate Isocast can capture"))
}

/// How good a size, rate and pixel format is for the request and the
/// canvas: bigger is better, in the order the keys are compared (rate,
/// fits, size, shape, pixel format).
type Score = (u32, bool, i64, bool, u8);

/// Score one candidate, in the keys [`Score`] compares in order.
fn score_of(offer: &Offer, rate: Rate, request: CameraRequest, canvas: &Canvas) -> Score {
    let fps = rate.rounded();
    // A requested rate first (the nearest the device has when it has no
    // exact one), else the highest rate up to the canvas's: a 1080p60
    // facecam beats 4K30.
    let rate_key = match request.fps {
        Some(wanted) => u32::MAX - fps.abs_diff(wanted),
        None => fps.min(canvas.fps),
    };
    let fits = offer.width.max(offer.height) <= canvas.width.max(canvas.height)
        && offer.width.min(offer.height) <= canvas.width.min(canvas.height);
    // The size: the closest to the request where there is one, else the
    // largest that fits (and among sizes that don't, the smallest
    // overflow).
    let area = i64::from(offer.width) * i64::from(offer.height);
    let size_key = match request.size {
        Some(wanted) => {
            -i64::from(offer.width.abs_diff(wanted.0) + offer.height.abs_diff(wanted.1))
        }
        None if fits => area,
        None => i64::MIN + area,
    };
    let shape = (offer.width >= offer.height) == (canvas.width >= canvas.height);
    (rate_key, fits, size_key, shape, offer.pixel.score())
}

/// A frame rate as an exact ratio: `num` frames every `den` seconds
/// (30/1, and 15/2 for the 7.5 fps a camera reports as a 2/15-second
/// interval).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Rate {
    num: u32,
    den: u32,
}

impl Rate {
    /// A whole frames-per-second rate.
    #[cfg(test)]
    fn whole(fps: u32) -> Self {
        Self::of_interval([1, fps]).expect("a whole rate's interval is non-zero")
    }

    /// The rate a `v4l2_fract` frame interval describes (`1/30` is 30
    /// fps), inverted from the interval the device reports.
    fn of_interval([numerator, denominator]: [u32; 2]) -> Option<Self> {
        (numerator > 0 && denominator > 0).then_some(Self {
            num: denominator,
            den: numerator,
        })
    }

    fn fps(self) -> f64 {
        f64::from(self.num) / f64::from(self.den)
    }

    /// The rate as the engine's whole frames per second, which its file
    /// timelines count in.
    fn rounded(self) -> u32 {
        let fps = self.fps().round();
        if fps < 1.0 { 1 } else { fps as u32 }
    }

    /// The rate as ffmpeg's `-framerate`, which takes a number or a
    /// ratio (`30`, `15/2`).
    fn ffmpeg(self) -> String {
        if self.den == 1 {
            self.num.to_string()
        } else {
            format!("{}/{}", self.num, self.den)
        }
    }
}

/// The pixel formats Isocast captures from a V4L2 camera: MJPEG, which
/// this machine's ffmpeg decodes, and YUYV, which needs no decoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pixel {
    Mjpeg,
    Yuyv,
}

impl Pixel {
    /// The fourcc the kernel knows the format by.
    fn from_fourcc(fourcc: u32) -> Option<Self> {
        match fourcc {
            PIX_FMT_MJPEG => Some(Self::Mjpeg),
            PIX_FMT_YUYV => Some(Self::Yuyv),
            _ => None,
        }
    }

    /// What ffmpeg's v4l2 input calls the format.
    fn input_format(self) -> &'static str {
        match self {
            Self::Mjpeg => "mjpeg",
            Self::Yuyv => "yuyv422",
        }
    }

    /// Which to prefer at one size and rate: MJPEG carries less over the
    /// USB wire (and is the only way to 720p30 and 1080p on a USB 2
    /// webcam), while ffmpeg decodes it either way.
    fn score(self) -> u8 {
        match self {
            Self::Mjpeg => 1,
            Self::Yuyv => 0,
        }
    }
}

/// The size one framesize entry names: a discrete entry's own, or the
/// largest of a stepwise or continuous range.
fn sizes_dims(entry: &Frmsizeenum) -> Option<(u32, u32)> {
    let (width, height) = match entry.kind {
        SIZE_DISCRETE => (entry.sizes[0], entry.sizes[1]),
        // A stepwise range is min, max and step, width first: capture
        // its largest size, the one a canvas wants.
        SIZE_STEPWISE | SIZE_CONTINUOUS => (entry.sizes[1], entry.sizes[4]),
        _ => return None,
    };
    (width > 0 && height > 0).then_some((width, height))
}

/// The rates one frameinterval entry names: a discrete entry's own, or
/// the ends of a stepwise or continuous range.
fn interval_rates(entry: &Frmivalenum) -> Vec<Rate> {
    let intervals: Vec<[u32; 2]> = match entry.kind {
        INTERVAL_DISCRETE => vec![[entry.intervals[0], entry.intervals[1]]],
        INTERVAL_STEPWISE | INTERVAL_CONTINUOUS => vec![
            [entry.intervals[0], entry.intervals[1]],
            [entry.intervals[2], entry.intervals[3]],
        ],
        _ => Vec::new(),
    };
    intervals
        .into_iter()
        .filter_map(Rate::of_interval)
        .collect()
}

/// Whether an enumerating ioctl's error means the list has ended: the
/// kernel answers EINVAL past the last entry, and ENOTTY where a driver
/// has no such list at all.
fn list_ended(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::EINVAL) | Some(libc::ENOTTY))
}

/// The node's capabilities, QUERYCAP's answer.
fn capability(fd: RawFd) -> Result<Capability> {
    let mut capabilities = Capability {
        driver: [0; 16],
        card: [0; 32],
        bus_info: [0; 32],
        version: 0,
        capabilities: 0,
        device_caps: 0,
        reserved: [0; 3],
    };
    ioctl(fd, VIDIOC_QUERYCAP, &mut capabilities).context("VIDIOC_QUERYCAP")?;
    Ok(capabilities)
}

/// Open a camera node for the enumeration ioctls: the way a capture opens
/// it (read-write, non-blocking, closed on exec), and closed when the
/// returned descriptor goes out of scope.
fn open_capture_node(path: &Path) -> Result<OwnedFd> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes())
        .with_context(|| format!("{} has a NUL byte in its name", path.display()))?;
    // SAFETY: `c_path` is NUL-terminated and the flags are constants, so
    // the call reads only what it was given and returns a new descriptor
    // or -1.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return Err(anyhow!("could not open {}: {err}", path.display()));
    }
    // SAFETY: `fd` is a descriptor this call just made, owned by nothing
    // else, and never closed by hand.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// One ioctl whose argument is a pointer to `value`, the way V4L2's
/// enumerating calls work.
fn ioctl<T>(fd: RawFd, request: libc::c_ulong, value: &mut T) -> std::io::Result<()> {
    // SAFETY: the request's argument is a pointer to the struct it
    // names — `value`, live and borrowed for the call — which V4L2 only
    // reads and writes through.
    if unsafe { libc::ioctl(fd, request as _, value as *mut T) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The NUL-terminated string in a kernel byte array.
fn cstr(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

// ---------------------------------------------------------------------------
// The kernel's own types and request numbers, from `linux/videodev2.h`.
// ---------------------------------------------------------------------------

/// `_IOC_READ` and `_IOC_WRITE` from `asm-generic/ioctl.h`.
const IOC_READ: u32 = 2;
const IOC_WRITE: u32 = 1;

/// `_IOWR('V', nr, type)`: how the kernel builds the request number of
/// every enumerating V4L2 ioctl.
const fn v4l2_iowr(nr: u32, size: usize) -> libc::c_ulong {
    (((IOC_READ | IOC_WRITE) << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr)
        as libc::c_ulong
}

/// `_IOR('V', nr, type)`: QUERYCAP's request number, which only has the
/// kernel write its answer back.
const fn v4l2_ior(nr: u32, size: usize) -> libc::c_ulong {
    (((IOC_READ) << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr) as libc::c_ulong
}

const VIDIOC_QUERYCAP: libc::c_ulong = v4l2_ior(0, size_of::<Capability>());
const VIDIOC_ENUM_FMT: libc::c_ulong = v4l2_iowr(2, size_of::<Fmtdesc>());
const VIDIOC_ENUM_FRAMESIZES: libc::c_ulong = v4l2_iowr(74, size_of::<Frmsizeenum>());
const VIDIOC_ENUM_FRAMEINTERVALS: libc::c_ulong = v4l2_iowr(75, size_of::<Frmivalenum>());

/// `V4L2_BUF_TYPE_VIDEO_CAPTURE`.
const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
/// `V4L2_CAP_VIDEO_CAPTURE` and its multiplanar twin: a node that takes
/// pictures.
const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const CAP_VIDEO_CAPTURE_MPLANE: u32 = 0x0000_1000;
/// The pixel formats Isocast captures, by the kernel's fourcc
/// (`v4l2_fourcc('M', 'J', 'P', 'G')` and `('Y', 'U', 'Y', 'V')`).
const PIX_FMT_MJPEG: u32 = 0x4750_4a4d;
const PIX_FMT_YUYV: u32 = 0x5659_5559;
/// The framesize and frameinterval enumeration types: discrete, stepwise
/// (min, max, step) and continuous (stepwise with a step of one).
const SIZE_DISCRETE: u32 = 1;
const SIZE_CONTINUOUS: u32 = 2;
const SIZE_STEPWISE: u32 = 3;
const INTERVAL_DISCRETE: u32 = 1;
const INTERVAL_CONTINUOUS: u32 = 2;
const INTERVAL_STEPWISE: u32 = 3;

/// `struct v4l2_capability`.
#[repr(C)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_fmtdesc`.
#[repr(C)]
struct Fmtdesc {
    index: u32,
    kind: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    /// Since 5.0, where `reserved[0]` used to be.
    mbus_code: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_frmsizeenum`: `sizes` is its union, six words wide
/// (`{width, height}` for a discrete size, `{min_w, max_w, step_w, min_h,
/// max_h, step_h}` for a stepwise one).
#[repr(C)]
struct Frmsizeenum {
    index: u32,
    pixel_format: u32,
    kind: u32,
    sizes: [u32; 6],
    reserved: [u32; 2],
}

/// `struct v4l2_frmivalenum`: `intervals` is its union, six words wide
/// (`{numerator, denominator}` for a discrete interval, `{min, max,
/// step}` — each a `v4l2_fract` — for a stepwise one).
#[repr(C)]
struct Frmivalenum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    kind: u32,
    intervals: [u32; 6],
    reserved: [u32; 2],
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1920×1080 canvas at 60 fps, the shape of the setups Isocast
    /// records.
    fn canvas() -> Canvas {
        Canvas {
            width: 1920,
            height: 1080,
            fps: 60,
        }
    }

    /// The C920's offers, as the device on the machine this was written
    /// on gives them (its MJPEG sizes and rates; the real table is
    /// longer).
    fn offers() -> Offers {
        let mjpeg = |width, height, rates: &[u32]| Offer {
            pixel: Pixel::Mjpeg,
            width,
            height,
            rates: rates.iter().copied().map(Rate::whole).collect(),
        };
        Offers {
            list: vec![
                mjpeg(640, 480, &[30, 24, 15]),
                mjpeg(1280, 720, &[30, 24, 15]),
                mjpeg(1920, 1080, &[30]),
                Offer {
                    pixel: Pixel::Yuyv,
                    width: 1280,
                    height: 720,
                    rates: vec![Rate::whole(10)],
                },
            ],
            formats: vec!["YUYV 4:2:2".to_string(), "Motion-JPEG".to_string()],
        }
    }

    #[test]
    fn ioctl_requests_match_the_kernel_header() {
        // The numbers linux/videodev2.h's own macros make on x86-64 (and
        // the struct sizes that go into them), so a mistyped field would
        // not silently change what the kernel is asked.
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xc034_564b);
        assert_eq!(size_of::<Capability>(), 104);
        assert_eq!(size_of::<Fmtdesc>(), 64);
        assert_eq!(size_of::<Frmsizeenum>(), 44);
        assert_eq!(size_of::<Frmivalenum>(), 52);
        // The unions start where the kernel puts them.
        assert_eq!(std::mem::offset_of!(Frmsizeenum, kind), 8);
        assert_eq!(std::mem::offset_of!(Frmsizeenum, sizes), 12);
        assert_eq!(std::mem::offset_of!(Frmivalenum, kind), 16);
        assert_eq!(std::mem::offset_of!(Frmivalenum, intervals), 20);
        assert_eq!(std::mem::offset_of!(Fmtdesc, pixelformat), 44);
    }

    #[test]
    fn an_open_request_takes_the_canvas_shape() {
        // No request: the highest rate up to the canvas rate (30 here),
        // then the largest size that fits a 1920×1080 canvas.
        let chosen = choose(&offers(), CameraRequest::default(), &canvas()).unwrap();
        assert_eq!((chosen.width, chosen.height), (1920, 1080));
        assert_eq!(chosen.rate, Rate::whole(30));
        assert_eq!(chosen.pixel, Pixel::Mjpeg);
    }

    #[test]
    fn a_request_the_device_has_is_taken_exactly() {
        let request = CameraRequest {
            size: Some((1280, 720)),
            fps: Some(24),
        };
        let chosen = choose(&offers(), request, &canvas()).unwrap();
        assert_eq!((chosen.width, chosen.height), (1280, 720));
        assert_eq!(chosen.rate, Rate::whole(24));
        // MJPEG over YUYV at the same size, since only YUYV has 10 fps
        // there and the request asked for 24.
        assert_eq!(chosen.pixel, Pixel::Mjpeg);
    }

    #[test]
    fn a_request_the_device_lacks_gets_the_nearest() {
        // 60 fps at 640×480: the device's highest there is 30, so that is
        // what CaptureFormat then reports.
        let request = CameraRequest {
            size: Some((640, 480)),
            fps: Some(60),
        };
        let chosen = choose(&offers(), request, &canvas()).unwrap();
        assert_eq!((chosen.width, chosen.height), (640, 480));
        assert_eq!(chosen.rate, Rate::whole(30));
        // A size the device has nowhere near: the closest it does have.
        let request = CameraRequest {
            size: Some((618, 470)),
            fps: None,
        };
        let chosen = choose(&offers(), request, &canvas()).unwrap();
        assert_eq!((chosen.width, chosen.height), (640, 480));
    }

    #[test]
    fn a_camera_with_no_capturable_format_is_an_error_naming_them() {
        let offers = Offers {
            list: Vec::new(),
            formats: vec!["H.264".to_string()],
        };
        let error = choose(&offers, CameraRequest::default(), &canvas()).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("H.264"), "{message}");
    }

    #[test]
    fn rates_come_from_frame_intervals() {
        // `1/30` is 30 fps, `2/15` is 7.5, and ffmpeg takes either as a
        // number or a ratio.
        assert_eq!(Rate::of_interval([1, 30]), Some(Rate::whole(30)));
        let seven_and_a_half = Rate::of_interval([2, 15]).unwrap();
        assert_eq!(seven_and_a_half.ffmpeg(), "15/2");
        assert_eq!(seven_and_a_half.rounded(), 8);
        assert_eq!(Rate::of_interval([0, 30]), None);
    }

    #[test]
    fn frames_follow_the_rows_stride() {
        // A 4×2 picture whose rows are 8 bytes apart: the padding between
        // them must stay untouched.
        let bytes: Vec<u8> = (0..32u8).collect();
        let mut pixels = vec![0u8; 2 * 24];
        let mut read = std::io::Cursor::new(bytes.clone());
        read_frame(&mut read, &mut pixels, 24, 4, 2).unwrap();
        assert_eq!(&pixels[..16], &bytes[..16]);
        assert_eq!(&pixels[24..40], &bytes[16..]);
        assert!(pixels[16..24].iter().all(|byte| *byte == 0));
        // A plane that can't hold the picture is an error, not a panic.
        let mut small = vec![0u8; 16];
        let mut read = std::io::Cursor::new(bytes);
        assert!(read_frame(&mut read, &mut small, 24, 4, 2).is_err());
    }

    #[test]
    fn a_pump_hands_over_whole_frames_and_ends_at_the_end() {
        let (width, height) = (4u32, 2u32);
        let frame_bytes = (width * height * 4) as usize;
        let mut bytes = vec![0x11u8; frame_bytes];
        bytes.extend(vec![0x22u8; frame_bytes]);
        let mut read = std::io::Cursor::new(bytes);
        let seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sink = VideoSink {
            frame: Box::new({
                let seen = seen.clone();
                move |frame: LinuxFrame, _at| {
                    seen.lock()
                        .expect("frames")
                        .push(frame.plane(0).expect("a BGRA plane").0);
                }
            }),
            stopped: Box::new(|_reason| {}),
        };
        let started = AtomicBool::new(false);
        let stopping = AtomicBool::new(false);
        let mut pool = CpuPool::bgra(width, height);
        let end = pump(
            &mut read,
            crate::LinuxPlatform,
            (width, height),
            &mut pool,
            &sink,
            &started,
            &stopping,
        );
        assert!(matches!(end, End::Ended));
        assert!(started.load(Ordering::SeqCst));
        // No frame was handed over twice or half-read: the second frame
        // starts where the first ended.
        let seen = seen.lock().expect("frames");
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].len(), frame_bytes);
        assert!(seen[0].iter().all(|byte| *byte == 0x11));
        assert!(seen[1].iter().all(|byte| *byte == 0x22));
    }

    #[test]
    fn a_stop_before_the_first_frame_hands_nothing_over() {
        let mut read = std::io::Cursor::new(vec![0u8; 64]);
        let seen = Arc::new(Mutex::new(0usize));
        let sink = VideoSink {
            frame: Box::new({
                let seen = seen.clone();
                move |_frame: LinuxFrame, _at| *seen.lock().expect("frames") += 1
            }),
            stopped: Box::new(|_reason| {}),
        };
        let started = AtomicBool::new(false);
        let stopping = AtomicBool::new(true);
        let mut pool = CpuPool::bgra(4, 2);
        let end = pump(
            &mut read,
            crate::LinuxPlatform,
            (4, 2),
            &mut pool,
            &sink,
            &started,
            &stopping,
        );
        assert!(matches!(end, End::Stopped));
        assert_eq!(*seen.lock().expect("frames"), 0);
    }
}
