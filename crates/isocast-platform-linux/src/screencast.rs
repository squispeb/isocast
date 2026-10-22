//! The desktop's screen stream, consumed in this process.
//!
//! The portal hands back a PipeWire node; this module is the node's
//! client. It negotiates one of the 4-byte BGRA orders the pools use,
//! maps the buffers the compositor writes, copies each picture into a
//! pool frame and hands it to the sink stamped with the host clock —
//! the shape the camera already has, minus ffmpeg: nothing is encoded
//! here, and the writer's ffmpeg takes BGRA as it does from a camera.
//!
//! Pictures arrive when the screen changes, not on a clock, so the
//! stream's own rate is a ceiling; the ask (`fps`) is enforced by
//! dropping whatever arrives inside its interval. DMA-BUF *import* (the
//! zero-copy path) is L.4: this reads the mapped bytes, which needs the
//! buffer to be mappable (Hyprland's screencopy exports linear ones).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_engine::{CaptureFormat, FramePool, Platform, VideoSink};
use pipewire as pw;
use pw::spa;

use crate::frames::{CpuPool, LinuxFrame};
use crate::{LinuxPlatform, portal};

/// How long the stream has to describe its picture once it is connected:
/// the compositor answers in milliseconds, and this bounds a node that
/// never links.
const FORMAT_WAIT: Duration = Duration::from_secs(5);
/// Frames the pool was too busy to take are reported at most this often.
const DROP_REPORT: Duration = Duration::from_secs(5);

/// A running screen capture. Dropping it quits the stream's loop and
/// joins its thread.
pub(crate) struct ScreenCapture {
    /// Tells the capture's thread to leave its loop.
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    /// Set while dropping, so the end that follows is a stop rather than
    /// something to report through the sink.
    stopping: Arc<AtomicBool>,
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start capturing a monitor through the desktop's portal: the picker on
/// a machine that has not answered one, and the choice it remembers
/// afterwards.
pub(crate) fn capture(
    platform: LinuxPlatform,
    fps: u32,
    cursor: bool,
    sink: VideoSink<LinuxFrame>,
) -> Result<(ScreenCapture, CaptureFormat)> {
    let stream = portal::open(cursor).context("sharing a screen through the desktop's portal")?;
    start(platform, stream, fps, sink)
}

/// Consume a stream the portal handed back, and wait for the compositor
/// to describe its picture: the format a take records at is the
/// compositor's to settle, not ours.
fn start(
    platform: LinuxPlatform,
    stream: portal::Stream,
    fps: u32,
    sink: VideoSink<LinuxFrame>,
) -> Result<(ScreenCapture, CaptureFormat)> {
    let (stop, quit) = pw::channel::channel();
    let (report, reported) = mpsc::channel();
    let stopping = Arc::new(AtomicBool::new(false));
    let thread = isocast_diagnostics::spawn(
        "screen capture",
        {
            let stopping = stopping.clone();
            move || run(platform, stream, fps, sink, quit, stopping, report)
        },
        |why| isocast_diagnostics::diagnostic!("error"; "the screen capture thread stopped: {why}"),
    )
    .context("starting the screen capture thread")?;
    let capture = ScreenCapture {
        stop,
        thread: Some(thread),
        stopping,
    };
    match reported.recv_timeout(FORMAT_WAIT) {
        Ok(Ok(format)) => Ok((capture, format)),
        Ok(Err(err)) => {
            drop(capture);
            Err(err)
        }
        Err(RecvTimeoutError::Timeout) => {
            drop(capture);
            bail!(
                "the desktop's screen stream did not describe its picture within {} s",
                FORMAT_WAIT.as_secs()
            )
        }
        Err(RecvTimeoutError::Disconnected) => {
            drop(capture);
            bail!("the screen capture's thread stopped before the stream described its picture")
        }
    }
}

/// The capture thread: build the stream, run its loop, and report why it
/// ended when it was not a stop.
fn run(
    platform: LinuxPlatform,
    stream: portal::Stream,
    fps: u32,
    sink: VideoSink<LinuxFrame>,
    stop: pw::channel::Receiver<()>,
    stopping: Arc<AtomicBool>,
    report: Sender<Result<CaptureFormat>>,
) {
    if let Err(err) = watch(platform, stream, fps, sink, stop, stopping, &report) {
        // The caller is waiting for the format: a failure before the
        // stream described itself has to reach it. Afterwards `Pictures`
        // has already reported through the sink.
        let _ = report.send(Err(err));
    }
}

/// Run the portal node's stream until stopped. Every callback the loop
/// runs works through `Pictures`, which owns the sink.
#[allow(clippy::too_many_arguments)]
fn watch(
    platform: LinuxPlatform,
    stream: portal::Stream,
    fps: u32,
    sink: VideoSink<LinuxFrame>,
    stop: pw::channel::Receiver<()>,
    stopping: Arc<AtomicBool>,
    report: &Sender<Result<CaptureFormat>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("creating a PipeWire loop")?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).context("creating a PipeWire context")?;
    // The fd the portal handed back is this stream's whole world: the
    // node lives on the portal's own remote, not this session's daemon.
    let core = context
        .connect_fd_rc(stream.fd, None)
        .context("connecting to the desktop's screen remote")?;
    let node = pw::stream::StreamRc::new(
        core.clone(),
        "isocast screen",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("creating the screen stream")?;

    let started = Arc::new(AtomicBool::new(false));
    let pictures = Pictures {
        platform,
        sink,
        stopping: stopping.clone(),
        report: report.clone(),
        started: started.clone(),
        mainloop: mainloop.clone(),
        format: Default::default(),
        pool: None,
        size: (0, 0),
        interval: Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1))),
        last: None,
        drops: DropReport::default(),
        unmapped: false,
    };
    let _listener = node
        .add_local_listener_with_user_data(pictures)
        .param_changed(param_changed)
        .state_changed(state_changed)
        .process(process)
        .register()
        .context("watching the screen stream")?;

    // Drop tells this loop to quit; the channel is the only way into a
    // PipeWire loop from another thread.
    let _quit = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });

    let picture = format_params(fps)?;
    let buffers = buffer_params()?;
    let mut params = [
        spa::pod::Pod::from_bytes(&picture).context("building the screen stream's format")?,
        spa::pod::Pod::from_bytes(&buffers).context("building the screen stream's buffers")?,
    ];
    node.connect(
        spa::utils::Direction::Input,
        Some(stream.node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )
    .context("connecting to the desktop's screen node")?;
    mainloop.run();
    Ok(())
}

/// What one stream's callbacks work with: the negotiating format, the
/// pool the pictures are copied into, and where they go.
struct Pictures {
    platform: LinuxPlatform,
    sink: VideoSink<LinuxFrame>,
    stopping: Arc<AtomicBool>,
    report: Sender<Result<CaptureFormat>>,
    /// Set once the format has reached the caller: a failure after this
    /// is the capture ending, not the capture failing to start.
    started: Arc<AtomicBool>,
    /// Quits the loop from inside a callback.
    mainloop: pw::main_loop::MainLoopRc,
    /// The format the compositor settled on.
    format: spa::param::video::VideoInfoRaw,
    /// The pool the pictures are copied into, once the size is known.
    pool: Option<CpuPool>,
    size: (u32, u32),
    /// The shortest gap between two pictures the ask allows.
    interval: Duration,
    last: Option<Instant>,
    drops: DropReport,
    /// Complained about an unreadable buffer already.
    unmapped: bool,
}

impl Pictures {
    /// The stream failed or ended: told to the caller while it waits for
    /// the format, and to the sink's owner afterwards. A deliberate stop
    /// says nothing.
    fn ended(&mut self, reason: String) {
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        if self.started.load(Ordering::SeqCst) {
            (self.sink.stopped)(reason);
        } else {
            let _ = self.report.send(Err(anyhow::anyhow!(reason)));
        }
    }
}

/// The compositor settled the picture's shape: build the pool that frame
/// size needs, and tell the caller the format it is recording at.
fn param_changed(
    _stream: &pw::stream::Stream,
    data: &mut Pictures,
    id: u32,
    param: Option<&spa::pod::Pod>,
) {
    use spa::param::ParamType;
    if id != ParamType::Format.as_raw() {
        return;
    }
    let Some(param) = param else {
        return;
    };
    let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param) else {
        return;
    };
    if media_type != spa::param::format::MediaType::Video
        || media_subtype != spa::param::format::MediaSubtype::Raw
    {
        return;
    }
    if data.format.parse(param).is_err() {
        data.ended("the desktop's screen stream described a picture we could not read".into());
        data.mainloop.quit();
        return;
    }
    let pixel = data.format.format();
    let size = data.format.size();
    let (width, height) = (size.width, size.height);
    if width == 0 || height == 0 {
        return;
    }
    // The pools hold BGRA bytes, so the stream's order has to be one of
    // the two whose bytes are already blue, green, red, alpha.
    if !matches!(
        pixel,
        spa::param::video::VideoFormat::BGRx | spa::param::video::VideoFormat::BGRA
    ) {
        data.ended(format!(
            "the desktop's screen stream delivers {pixel:?}, which is not a BGRA order this \
             build copies from"
        ));
        data.mainloop.quit();
        return;
    }
    let rate = data.format.framerate();
    // A stream that names no rate leaves the take's own; a named one is
    // rounded down to whole frames per second.
    let fps = rate.num / rate.denom.max(1);
    data.size = (width, height);
    data.pool = Some(CpuPool::bgra(width, height));
    data.started.store(true, Ordering::SeqCst);
    isocast_diagnostics::debug!(
        "screen: the desktop shares {width}x{height} {pixel:?} ({} Hz)",
        if fps == 0 {
            "its own".to_string()
        } else {
            fps.to_string()
        }
    );
    // A variable-rate stream (0/1) is delivered at the take's own rate:
    // pictures closer than `interval` are dropped. The engine paces by
    // the rate it is told, so it is told that one, never 0.
    let asked = (1_000_000_000 / data.interval.as_nanos().max(1)) as u32;
    let _ = data.report.send(Ok(CaptureFormat {
        width,
        height,
        fps: if fps == 0 { asked.max(1) } else { fps },
    }));
}

/// The stream's own state, for the failures that never reach a frame.
fn state_changed(
    _stream: &pw::stream::Stream,
    data: &mut Pictures,
    _old: pw::stream::StreamState,
    new: pw::stream::StreamState,
) {
    if let pw::stream::StreamState::Error(why) = new {
        data.ended(format!("the desktop's screen stream failed: {why}"));
        data.mainloop.quit();
    }
}

/// One picture: copy it into a pool frame and hand it over.
fn process(stream: &pw::stream::Stream, data: &mut Pictures) {
    if data.stopping.load(Ordering::SeqCst) {
        return;
    }
    // Pictures arrive when the screen changes; the ask caps how many we
    // keep, and a screen that repaints faster than that is the only case
    // this drops.
    let now = Instant::now();
    if let Some(last) = data.last
        && now.duration_since(last) < data.interval
    {
        return;
    }
    let (width, height) = (data.size.0 as usize, data.size.1 as usize);
    let row_bytes = width * 4;
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let Some(first) = buffer.datas_mut().first_mut() else {
        return;
    };
    let (offset, stride) = {
        let chunk = first.chunk();
        let stride = match chunk.stride() {
            stride if stride > 0 => stride as usize,
            _ => row_bytes,
        };
        (chunk.offset() as usize, stride)
    };
    let Some(bytes) = first.data() else {
        if !data.unmapped {
            data.unmapped = true;
            data.ended(
                "the desktop shared the screen as a DMA-BUF this build cannot read (its import \
                 is L.4): put `force_shm = true` in the screencopy section of \
                 ~/.config/hypr/xdph.conf to share pictures this way"
                    .into(),
            );
            data.mainloop.quit();
        }
        return;
    };
    let Some(pool) = data.pool.as_mut() else {
        return;
    };
    let taken = pool.draw(&mut |pixels, rows| {
        for row in 0..height {
            let (from, to) = (offset + row * stride, row * rows);
            if from + row_bytes <= bytes.len() && to + row_bytes <= pixels.len() {
                pixels[to..to + row_bytes].copy_from_slice(&bytes[from..from + row_bytes]);
            }
        }
    });
    data.last = Some(now);
    match taken {
        Ok(Some(frame)) => {
            data.drops.reset();
            (data.sink.frame)(frame, data.platform.now());
        }
        // Every frame still downstream: this one goes, and the screen's
        // next change brings another.
        Ok(None) => data.drops.count(),
        Err(err) => {
            data.ended(format!("the screen capture stopped: {err:#}"));
            data.mainloop.quit();
        }
    }
}

/// How many pictures the pool was too full to take, reported rarely: a
/// stalled consumer shows in the log without flooding it.
#[derive(Default)]
struct DropReport {
    dropped: u64,
    since: Option<Instant>,
}

impl DropReport {
    fn count(&mut self) {
        self.dropped += 1;
        let since = *self.since.get_or_insert_with(Instant::now);
        let elapsed = since.elapsed();
        if elapsed >= DROP_REPORT {
            isocast_diagnostics::diagnostic!(
                "warn"; "screen: dropped {} picture(s) in {:.0} s: every frame in the pool is \
                still downstream",
                self.dropped,
                elapsed.as_secs_f64()
            );
            self.since = Some(Instant::now());
            self.dropped = 0;
        }
    }

    /// A picture got through, so a drop run ended.
    fn reset(&mut self) {
        self.since = None;
        self.dropped = 0;
    }
}

/// The picture this stream will take: 4-byte BGRA orders only, at the
/// compositor's own size, and asking for the take's frame rate.
fn format_params(fps: u32) -> Result<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::pod::{Value, object, property};
    use spa::utils::{Fraction, Rectangle, SpaTypes};
    let object = object!(
        SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA
        ),
        property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 16,
                height: 16
            },
            Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction {
                num: fps.max(1),
                denom: 1
            },
            // From 0/1: a compositor's screen cast is variable-rate and
            // says so as 0/1 (XDPH, with a max-framerate beside it); a
            // range from 1/1 never meets it ("no more input formats").
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: fps.max(1) * 4,
                denom: 1
            }
        ),
    );
    serialize(&Value::Object(object)).context("serializing the screen stream's format")
}

/// The buffers this consumer can read: mapped memory (a memfd, or plain
/// shared memory). A DMA-BUF is an fd whose bytes want importing into the
/// GPU (L.4) rather than copying, so the compositor is asked for the
/// shapes [`process`] copies from; a node that hands one over anyway ends
/// the capture with a message naming it.
fn buffer_params() -> Result<Vec<u8>> {
    use spa::pod::{ChoiceValue, Object, Property, PropertyFlags, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, SpaTypes};
    let memfd = 1 << spa::sys::SPA_DATA_MemFd;
    let memptr = 1 << spa::sys::SPA_DATA_MemPtr;
    let object = Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![Property {
            key: spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: PropertyFlags::empty(),
            value: Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: memfd,
                    flags: vec![memfd, memptr],
                },
            ))),
        }],
    };
    serialize(&Value::Object(object)).context("serializing the screen stream's buffers")
}

/// A pod, as the bytes `Stream::connect` takes.
fn serialize(value: &spa::pod::Value) -> Result<Vec<u8>> {
    let bytes =
        spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), value)
            .context("serializing a pod")?
            .0
            .into_inner();
    Ok(bytes)
}
