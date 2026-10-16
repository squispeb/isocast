//! Microphones and what the machine plays, captured through PipeWire.
//!
//! There is one way to hear a microphone, a sink's monitor or one app's
//! own output on Linux: PipeWire's graph. The engine's audio is 48 kHz
//! float, so a capture stream asks for exactly that — the device's own
//! channel count for a microphone, stereo for an output or an app — and
//! copies each chunk of mapped buffer into a plain `Vec<f32>`, handed to
//! the sink stamped with the host clock, the shape the camera and the
//! screen already have. Nothing is encoded here.
//!
//! A capture names its source the way `isocast devices` lists it: a
//! microphone by the `node.name` `pactl` shows (which is the name
//! PipeWire knows the node by), the machine's output by its sink, whose
//! *monitor* ports carry what is played on it ([`Target::Output`]), and
//! an app by its own output streams (L.3's per-app step).
//!
//! PipeWire hands a capture buffers of the graph's quantum (10-25 ms of
//! samples by default) with no capture timestamp of their own, so a
//! buffer's samples are dated from the stream's own sample position — they
//! run back to back — with the host time that position goes with (see
//! [`Clock`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_engine::{AudioSink, Platform, SAMPLE_RATE, Time};
use pipewire as pw;
use pw::spa;

use crate::LinuxPlatform;

/// How long a stream has to describe its format: a node answers in
/// milliseconds, and this bounds one that never links (a microphone that
/// was unplugged, a sink that is gone).
const FORMAT_WAIT: Duration = Duration::from_secs(5);
/// Buffers the sink's queue was too busy to take are reported at most
/// this often, so a stalled consumer shows in the log without flooding
/// it.
const DROP_REPORT: Duration = Duration::from_secs(5);

/// Where an audio capture takes its samples from.
#[derive(Clone, Debug)]
pub(crate) enum Target {
    /// A microphone: the source node itself, by PipeWire node name.
    Microphone(String),
    /// What the machine plays on a sink, from that sink's monitor ports.
    Output(String),
}

impl Target {
    /// What this target's samples are, for a debug line.
    fn describe(&self) -> String {
        match self {
            Self::Microphone(name) => format!("microphone {name}"),
            Self::Output(name) => format!("the monitor of {name}"),
        }
    }

    /// The node the stream asks for, and whether it is a sink's monitor.
    fn node(&self) -> (&str, bool) {
        match self {
            Self::Microphone(name) => (name, false),
            Self::Output(name) => (name, true),
        }
    }
}

/// A running audio capture. Dropping it quits the stream's loop and
/// joins its thread.
pub(crate) struct AudioCapture {
    /// Tells the capture's thread to leave its loop.
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    /// Set while dropping, so the end that follows is a stop rather than
    /// something to report through the sink.
    stopping: Arc<AtomicBool>,
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start capturing `target` in `channels` (the device's own for a
/// microphone, 2 for what the machine plays), returning the channel
/// count the stream settled on: the samples are interleaved in that, and
/// the format is always [`SAMPLE_RATE`].
pub(crate) fn capture(
    platform: LinuxPlatform,
    target: Target,
    channels: u32,
    sink: AudioSink,
) -> Result<(AudioCapture, u32)> {
    let described = target.describe();
    let (stop, quit) = pw::channel::channel();
    let (report, reported) = mpsc::channel();
    let stopping = Arc::new(AtomicBool::new(false));
    let thread = isocast_diagnostics::spawn(
        "audio capture",
        {
            let stopping = stopping.clone();
            move || {
                run(
                    platform,
                    target,
                    channels.max(1),
                    sink,
                    quit,
                    stopping,
                    report,
                )
            }
        },
        |why| isocast_diagnostics::diagnostic!("error"; "the audio capture thread stopped: {why}"),
    )
    .context("starting the audio capture thread")?;
    let capture = AudioCapture {
        stop,
        thread: Some(thread),
        stopping,
    };
    match reported.recv_timeout(FORMAT_WAIT) {
        Ok(Ok(channels)) => Ok((capture, channels)),
        Ok(Err(err)) => {
            drop(capture);
            Err(err)
        }
        Err(RecvTimeoutError::Timeout) => {
            drop(capture);
            bail!(
                "{described} did not describe its format within {} s; is the device still there?",
                FORMAT_WAIT.as_secs()
            )
        }
        Err(RecvTimeoutError::Disconnected) => {
            drop(capture);
            bail!("the audio capture's thread stopped before {described} described its format")
        }
    }
}

/// The capture thread: build the stream, run its loop, and report why it
/// ended when it was not a stop.
fn run(
    platform: LinuxPlatform,
    target: Target,
    channels: u32,
    sink: AudioSink,
    stop: pw::channel::Receiver<()>,
    stopping: Arc<AtomicBool>,
    report: Sender<Result<u32>>,
) {
    if let Err(err) = watch(platform, target, channels, sink, stop, stopping, &report) {
        // The caller is waiting for the format: a failure before the
        // stream described itself has to reach it. Afterwards `Samples`
        // has already reported through the sink.
        let _ = report.send(Err(err));
    }
}

/// Run the stream until stopped. Every callback the loop runs works
/// through `Samples`, which owns the sink.
#[allow(clippy::too_many_arguments)]
fn watch(
    platform: LinuxPlatform,
    target: Target,
    channels: u32,
    sink: AudioSink,
    stop: pw::channel::Receiver<()>,
    stopping: Arc<AtomicBool>,
    report: &Sender<Result<u32>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("creating a PipeWire loop")?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).context("creating a PipeWire context")?;
    // The session's own PipeWire, where the devices live (`pactl`'s names
    // are this graph's `node.name`).
    let core = context
        .connect_rc(None)
        .context("connecting to PipeWire (is the session's PipeWire running?)")?;
    let (node_name, monitor) = target.node();
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::APP_NAME => "Isocast",
        *pw::keys::NODE_NAME => "isocast capture",
    };
    props.insert(*pw::keys::TARGET_OBJECT, node_name);
    if monitor {
        // A sink's *monitor* ports are what its name alone would not
        // give: without this the stream would link to the sink's inputs.
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }
    let node = pw::stream::StreamRc::new(core.clone(), "isocast capture", props)
        .context("creating the audio stream")?;

    let started = Arc::new(AtomicBool::new(false));
    let samples = Samples {
        platform,
        sink,
        stopping: stopping.clone(),
        report: report.clone(),
        started: started.clone(),
        mainloop: mainloop.clone(),
        format: Default::default(),
        target: target.clone(),
        drops: DropReport::default(),
        clock: Clock::default(),
    };
    let _listener = node
        .add_local_listener_with_user_data(samples)
        .param_changed(param_changed)
        .state_changed(state_changed)
        .process(process)
        .register()
        .context("watching the audio stream")?;

    // Drop tells this loop to quit; the channel is the only way into a
    // PipeWire loop from another thread.
    let _quit = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });

    let format = format_params(channels)?;
    let mut params =
        [spa::pod::Pod::from_bytes(&format).context("building the audio stream's format")?];
    node.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )
    .context("connecting to the audio node")?;
    mainloop.run();
    Ok(())
}

/// What the stream's callbacks work with: the negotiated format, where
/// the samples go, and how to end the loop.
struct Samples {
    platform: LinuxPlatform,
    sink: AudioSink,
    stopping: Arc<AtomicBool>,
    report: Sender<Result<u32>>,
    /// Set once the format has reached the caller: a failure after this
    /// is the capture ending, not the capture failing to start.
    started: Arc<AtomicBool>,
    /// Quits the loop when the stream ends on its own.
    mainloop: pw::main_loop::MainLoopRc,
    format: spa::param::audio::AudioInfoRaw,
    target: Target,
    drops: DropReport,
    /// Dates each buffer's samples on the host clock.
    clock: Clock,
}

/// Buffers the sink's queue was too busy to take, reported rarely: a
/// stalled consumer shows in the log without flooding it.
#[derive(Default)]
struct DropReport {
    dropped: u64,
    since: Option<std::time::Instant>,
}

impl DropReport {
    fn count(&mut self, target: &Target, why: &str) {
        self.dropped += 1;
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        let elapsed = since.elapsed();
        if elapsed >= DROP_REPORT {
            isocast_diagnostics::diagnostic!(
                "warn"; "audio: {}: dropped {} buffer(s) in {:.0} s ({why})",
                target.describe(),
                self.dropped,
                elapsed.as_secs_f64()
            );
            self.since = Some(std::time::Instant::now());
            self.dropped = 0;
        }
    }

    /// Samples got through, so a drop run ended.
    fn reset(&mut self) {
        self.since = None;
        self.dropped = 0;
    }
}

/// The device described the format it will deliver.
fn param_changed(
    _stream: &pw::stream::Stream,
    data: &mut Samples,
    id: u32,
    param: Option<&spa::pod::Pod>,
) {
    if id != spa::param::ParamType::Format.as_raw() {
        return;
    }
    // None clears the format: the stream is being torn down, and a new
    // description follows if it is renegotiated.
    let Some(param) = param else {
        return;
    };
    let (media_type, media_subtype) = match spa::param::format_utils::parse_format(param) {
        Ok(parts) => parts,
        Err(err) => {
            return data.ended(format!(
                "{} described a format we could not read: {err}",
                data.target.describe()
            ));
        }
    };
    if media_type != spa::param::format::MediaType::Audio
        || media_subtype != spa::param::format::MediaSubtype::Raw
    {
        return data.ended(format!(
            "{} offers {media_type:?}/{media_subtype:?}, where Isocast captures raw audio",
            data.target.describe()
        ));
    }
    let before = data.format.channels();
    if let Err(err) = data.format.parse(param) {
        return data.ended(format!(
            "{} described a format we could not read: {err}",
            data.target.describe()
        ));
    }
    let rate = data.format.rate();
    let channels = data.format.channels();
    if channels == 0 {
        return data.ended(format!(
            "{} described zero channels",
            data.target.describe()
        ));
    }
    if data.started.load(Ordering::SeqCst) {
        // A second description is a renegotiation mid-take (the device
        // changed its layout). The engine mixes with the count the first
        // one settled, so a change is worth saying and not worth acting
        // on: the samples stay what the device now hands over.
        if channels != before {
            isocast_diagnostics::diagnostic!(
                "warn"; "audio: {}: the device changed from {before} to {channels} channel(s) \
                mid-take; the take keeps mixing {before}",
                data.target.describe()
            );
        }
        return;
    }
    if rate != SAMPLE_RATE {
        // The stream asks for the engine's rate and PipeWire converts, so
        // this means something in between refused to; the samples would be
        // at the wrong speed until it is converted.
        return data.ended(format!(
            "{} delivers {rate} Hz, where Isocast records {SAMPLE_RATE} Hz",
            data.target.describe()
        ));
    }
    isocast_diagnostics::debug!(
        "audio: {}: {rate} Hz, {channels} channel(s), {:?}",
        data.target.describe(),
        data.format.format()
    );
    data.started.store(true, Ordering::SeqCst);
    let _ = data.report.send(Ok(channels));
}

/// The stream's state changed; an error is what ended it.
fn state_changed(
    _stream: &pw::stream::Stream,
    data: &mut Samples,
    _old: pw::stream::StreamState,
    new: pw::stream::StreamState,
) {
    if let pw::stream::StreamState::Error(message) = new {
        data.ended(format!("{} failed: {message}", data.target.describe()));
    }
}

/// A buffer of samples arrived.
fn process(stream: &pw::stream::Stream, data: &mut Samples) {
    if data.stopping.load(Ordering::SeqCst) {
        return;
    }
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let Some(block) = buffer.datas_mut().first_mut() else {
        return;
    };
    let (offset, size) = {
        let chunk = block.chunk();
        (chunk.offset() as usize, chunk.size() as usize)
    };
    let Some(bytes) = block.data() else {
        // A capture buffer is mapped (the stream asked for MAP_BUFFERS);
        // one that is not cannot be read, so it is a dropped buffer, not
        // a silent one.
        data.dropped("a buffer was not mapped");
        return;
    };
    let end = offset.saturating_add(size).min(bytes.len());
    let Some(raw) = bytes.get(offset..end) else {
        data.dropped("a buffer's chunk was outside its own memory");
        return;
    };
    let samples: Vec<f32> = raw
        .as_chunks::<4>()
        .0
        .iter()
        .map(|four| f32::from_le_bytes(*four))
        .collect();
    let channels = data.format.channels().max(1) as usize;
    let frames = samples.len() / channels;
    if frames == 0 {
        return;
    }
    // The stream's own clock: `now` is the host time of the sample it has
    // delivered up to. A stream that cannot say (it never does in practice)
    // leaves the samples dated by the callback alone.
    let stream_clock = stream.time().ok().map(|t| t.now());
    let at = data.clock.at(
        frames as u64,
        stream_clock,
        data.platform.now().as_nanos(),
        &data.target,
    );
    data.drops.reset();
    (data.sink.samples)(&samples, at);
}

/// Nanoseconds in `frames` samples of [`SAMPLE_RATE`].
const fn nanos(frames: i64) -> i64 {
    (frames as i128 * 1_000_000_000 / SAMPLE_RATE as i128) as i64
}

/// How far the stream's own clock may differ from the count before a *step*
/// between buffers means samples never arrived, rather than a device whose
/// clock is a little fast or slow.
const CLOCK_STEP: i64 = 100_000_000;
/// A creep this far from the count is a device whose clock is off its
/// nominal rate by enough to say so: a long take will drift that much
/// against the picture.
const CLOCK_DRIFT: i64 = 250_000_000;

/// Where the samples that arrive are placed on the host clock.
///
/// A capture's samples run back to back — they are what the device handed
/// over, with nothing missing between them — so the count is what dates
/// them: the first buffer's place comes from the stream's own clock, and
/// every buffer after it is exactly its own samples later. Dating each
/// buffer by the wall clock read in its callback instead would leave holes
/// and overlaps in the engine's track, which fills a gap of a couple of
/// milliseconds with silence and calls it a dropped buffer; PipeWire's own
/// clock also runs a little fast while its graph settles. What the stream's
/// clock is watched for is the sound it says went missing (a step between
/// buffers) and a device whose rate is far enough off that the take will
/// drift against the picture.
#[derive(Default)]
struct Clock {
    /// The host time the count starts from, and the sample it was at.
    anchor: Option<(Time, u64)>,
    /// Samples handed over so far.
    delivered: u64,
    /// How far the stream's own clock was from the count at the last
    /// buffer, so a step can be told from a creep.
    creep: Option<i64>,
    /// Set once the creep is worth saying.
    warned: bool,
}

impl Clock {
    /// The host time for the next `frames` samples: `stream` is the stream's
    /// own reading of the host time at the sample that ends this buffer,
    /// or `None` when the stream had none to give and `now` (read at the
    /// callback) stands in for it.
    fn at(&mut self, frames: u64, stream: Option<i64>, now: i64, target: &Target) -> Time {
        let position = self.delivered;
        // The stream's report is the host time of the sample that ends this
        // buffer, so the first of them sat one buffer before it.
        let first = stream.unwrap_or(now) - nanos(frames as i64);
        let Some((anchor, anchor_position)) = self.anchor else {
            self.anchor = Some((Time::from_nanos(first), position));
            self.delivered += frames;
            return Time::from_nanos(first);
        };
        let mut at = anchor.add_ticks(position - anchor_position, SAMPLE_RATE);
        let mut jumped = false;
        if let Some(reported) = stream {
            let creep = reported - (at.as_nanos() + nanos(frames as i64));
            if self
                .creep
                .is_some_and(|last| (creep - last).abs() > CLOCK_STEP)
            {
                // The stream's clock does not jump on its own: samples the
                // graph never handed over went missing, and the gap belongs
                // where it happened.
                isocast_diagnostics::diagnostic!(
                    "warn"; "audio: {}: {:.0} ms of sound never arrived (the stream went on \
                    without it); the take's audio keeps the gap",
                    target.describe(),
                    creep.abs() as f64 / 1e6
                );
                at = Time::from_nanos(first);
                self.anchor = Some((at, position));
                jumped = true;
            } else if !self.warned && creep.abs() > CLOCK_DRIFT {
                self.warned = true;
                isocast_diagnostics::diagnostic!(
                    "warn"; "audio: {}: the device's own clock is {} {:.0} ms from the host's \
                    after {:.0} s (about {:.0} ppm); over a long take the sound drifts that far \
                    against the picture",
                    target.describe(),
                    if creep > 0 { "ahead of" } else { "behind" },
                    creep.abs() as f64 / 1e6,
                    position as f64 / SAMPLE_RATE as f64,
                    creep.abs() as f64 * 1e6 / (position as f64 / SAMPLE_RATE as f64 * 1e9)
                );
            }
            // The count starts again where a jump put it, so there is no
            // creep left to compare the next buffer against.
            self.creep = (!jumped).then_some(creep);
        }
        self.delivered += frames;
        at
    }
}

impl Samples {
    /// A buffer the sink's queue was too busy to take.
    fn dropped(&mut self, why: &str) {
        self.drops.count(&self.target, why);
    }

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
        self.mainloop.quit();
    }
}

/// The format the stream asks for: the engine's own (48 kHz float) in
/// `channels`, with the channel positions a converter needs to downmix
/// or upmix correctly.
fn format_params(channels: u32) -> Result<Vec<u8>> {
    use spa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
    use spa::pod::serialize::PodSerializer;
    use spa::pod::{Object, Value};

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(channels);
    let mut position = [0; MAX_CHANNELS];
    for (index, slot) in position.iter_mut().take(channels as usize).enumerate() {
        // The plain layouts: front left/right, then the rest by position,
        // which is what a device's own order is.
        *slot = match index {
            0 => spa::sys::SPA_AUDIO_CHANNEL_FL,
            1 => spa::sys::SPA_AUDIO_CHANNEL_FR,
            2 => spa::sys::SPA_AUDIO_CHANNEL_FC,
            3 => spa::sys::SPA_AUDIO_CHANNEL_LFE,
            4 => spa::sys::SPA_AUDIO_CHANNEL_RL,
            5 => spa::sys::SPA_AUDIO_CHANNEL_RR,
            6 => spa::sys::SPA_AUDIO_CHANNEL_SL,
            7 => spa::sys::SPA_AUDIO_CHANNEL_SR,
            _ => spa::sys::SPA_AUDIO_CHANNEL_UNKNOWN,
        };
    }
    info.set_position(position);
    let (bytes, _) = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|err| anyhow::anyhow!("serializing an audio format: {err}"))?;
    Ok(bytes.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_is_asked_for_as_the_engines_own_format() {
        // The pod is what the device is asked for: 48 kHz float stereo,
        // front left and right, nothing else. `parse` only succeeds for a
        // raw-audio EnumFormat object, which is what this has to be.
        let bytes = format_params(2).unwrap();
        let pod = spa::pod::Pod::from_bytes(&bytes).unwrap();
        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.parse(pod).unwrap();
        assert_eq!(info.rate(), SAMPLE_RATE);
        assert_eq!(info.channels(), 2);
        assert_eq!(info.format(), spa::param::audio::AudioFormat::F32LE);
        let position = info.position();
        assert_eq!(position[0], spa::sys::SPA_AUDIO_CHANNEL_FL);
        assert_eq!(position[1], spa::sys::SPA_AUDIO_CHANNEL_FR);
        // A mono ask is one channel, not two with the right one dropped.
        let bytes = format_params(1).unwrap();
        let pod = spa::pod::Pod::from_bytes(&bytes).unwrap();
        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.parse(pod).unwrap();
        assert_eq!(info.channels(), 1);
        assert_eq!(info.position()[0], spa::sys::SPA_AUDIO_CHANNEL_FL);
    }

    /// The host time `frames` samples after `base`, exactly: rounding to
    /// whole nanoseconds per buffer would show up as a wrong count.
    fn counted(base: i64, frames: i64) -> i64 {
        base + (frames as i128 * 1_000_000_000 / SAMPLE_RATE as i128) as i64
    }

    /// A stream delivers 1024-frame buffers whose own clock runs `ppm`
    /// fast, jitters by a couple of milliseconds and opens with the few
    /// milliseconds of ramp PipeWire's graph settles by: the stamps stay
    /// exactly one buffer apart (a hole would be a dropped buffer to the
    /// engine) and the sound is not pulled apart over the take.
    #[test]
    fn samples_are_dated_by_their_count_not_the_callback() {
        const FRAMES: u64 = 1024;
        const PPM: f64 = 200.0;
        const RAMP_BUFFERS: i64 = 40;
        const RAMP: i64 = 8_000_000;
        let nominal = nanos(FRAMES as i64);
        let step = (nominal as f64 * (1.0 + PPM * 1e-6)) as i64;
        let start = 271_000_000_000_000i64;
        let target = Target::Microphone("test".into());
        let mut clock = Clock::default();
        for buffer in 0..2_000i64 {
            // The callback's own reading, which jitters; the stream's clock
            // is the same thing without the jitter.
            let jitter = if buffer % 2 == 0 {
                2_000_000
            } else {
                -2_000_000
            };
            let settled = buffer * step + (buffer * RAMP / RAMP_BUFFERS).min(RAMP);
            let reported = start + settled;
            let at = clock.at(FRAMES, Some(reported), reported + jitter, &target);
            // The first buffer's place is the stream's own reading of its
            // end minus its length; every one after it is a buffer later.
            let expected = counted(start - nominal, buffer * FRAMES as i64);
            assert_eq!(
                at.as_nanos(),
                expected,
                "buffer {buffer} was not its count into the take"
            );
        }
        // A device 200 ppm off says so well before a quarter of a second of
        // drift, and never breaks the track over it.
        assert!(!clock.warned, "200 ppm is not worth a warning by itself");
    }

    /// A stream that stalls loses the samples that went with it, and the
    /// track should show that as a gap rather than a shorter take.
    #[test]
    fn samples_a_stalled_stream_lost_are_a_gap() {
        const FRAMES: u64 = 1024;
        let nominal = nanos(FRAMES as i64);
        let start = 271_000_000_000_000i64;
        let target = Target::Microphone("test".into());
        let mut clock = Clock::default();
        let mut previous = 0;
        for buffer in 1..6i64 {
            let reported = start + buffer * nominal;
            previous = clock
                .at(FRAMES, Some(reported), reported, &target)
                .as_nanos();
        }
        // Two seconds of samples never arrived.
        let missed = nanos(2 * SAMPLE_RATE as i64);
        let reported = start + 6 * nominal + missed;
        let after = clock.at(FRAMES, Some(reported), reported, &target);
        assert!(after.as_nanos() - previous > missed, "the gap was hidden");
        // And the one after that is a buffer later, not another gap: the
        // count started again where the jump put it.
        let next = clock.at(
            FRAMES,
            Some(reported + nominal),
            reported + nominal,
            &target,
        );
        assert_eq!(next.as_nanos() - after.as_nanos(), nominal);
    }

    /// A device whose clock is percent-off is not a stall: the samples stay
    /// back to back, and the drift is said once rather than silently
    /// pulling the take apart.
    #[test]
    fn a_far_off_device_clock_is_said_rather_than_a_gap() {
        const FRAMES: u64 = 1024;
        const PPM: f64 = 10_000.0;
        let nominal = nanos(FRAMES as i64);
        let step = (nominal as f64 * (1.0 + PPM * 1e-6)) as i64;
        let start = 271_000_000_000_000i64;
        let target = Target::Microphone("test".into());
        let mut clock = Clock::default();
        // A percent of drift is 213 ns a buffer, so it takes a while to add
        // up to the quarter second worth saying.
        for buffer in 0..2_000i64 {
            let reported = start + buffer * step;
            let at = clock.at(FRAMES, Some(reported), reported, &target);
            assert_eq!(
                at.as_nanos(),
                counted(start - nominal, buffer * FRAMES as i64),
                "buffer {buffer} broke the track"
            );
        }
        assert!(clock.warned, "a percent of drift is worth saying");
    }
}
