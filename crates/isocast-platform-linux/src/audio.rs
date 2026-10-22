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
//! an app by the streams it plays through ([`Target::Application`]).
//!
//! An app is the odd one: the graph has no node for "what Spotify
//! plays". Its *streams* are what there is, they come and go as the app
//! starts and stops playing, and the session manager will not link a
//! capture to a playback stream (asked to, it links the capture to its
//! default *source* — the microphone). So a capture of an app watches
//! the graph's registry itself and links every stream that matches to
//! its own input ports ([`Links`]), at open and for each one that
//! appears later in the take.
//!
//! PipeWire hands a capture buffers of the graph's quantum (10-25 ms of
//! samples by default) with no capture timestamp of their own, so a
//! buffer's samples are dated from the stream's own sample position — they
//! run back to back — with the host time that position goes with (see
//! [`Clock`]).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
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
/// The loudest sample an audio graph produces: full scale is ±1, a mix may
/// pass it, and a thousand times it is not sound but a value the graph or
/// an app's own data put there (see [`silence_memory`]).
const MAX_SAMPLE: f32 = 1.0e3;

/// Where an audio capture takes its samples from.
#[derive(Clone, Debug)]
pub(crate) enum Target {
    /// A microphone: the source node itself, by PipeWire node name.
    Microphone(String),
    /// What the machine plays on a sink, from that sink's monitor ports.
    Output(String),
    /// One app's own playback streams, the ones `query` matches (`name`
    /// is what the app is called in messages).
    Application { query: String, name: String },
}

impl Target {
    /// What this target's samples are, for a debug line.
    fn describe(&self) -> String {
        match self {
            Self::Microphone(name) => format!("microphone {name}"),
            Self::Output(name) => format!("the monitor of {name}"),
            Self::Application { name, .. } => format!("the sound of {name}"),
        }
    }

    /// The node the stream asks for, and whether it is a sink's monitor.
    /// An app has no one node: its streams are linked to the capture
    /// instead ([`Links`]).
    fn node(&self) -> Option<(&str, bool)> {
        match self {
            Self::Microphone(name) => Some((name, false)),
            Self::Output(name) => Some((name, true)),
            Self::Application { .. } => None,
        }
    }

    /// The app whose streams this capture links to, if it is one of those.
    fn app(&self) -> Option<&str> {
        match self {
            Self::Application { query, .. } => Some(query),
            _ => None,
        }
    }
}

/// One app's own playback stream, as the graph has it.
///
/// An app is capturable only by its streams, so these are what a setup
/// naming an app is told about, and what a capture matches against.
#[derive(Clone, Debug)]
pub(crate) struct AppStream {
    /// What the app calls itself (`application.name`), or the stream
    /// node's own name when the app does not say.
    app: String,
    /// The app's binary (`application.process.binary`), which a setup can
    /// name instead of the display name.
    binary: String,
    /// The node's own name (`node.name`).
    node: String,
}

impl AppStream {
    /// The app stream a graph node is, if it is one at all: what an app
    /// plays is a *playback* stream, and an app that is not playing has
    /// none to link to.
    fn from_node(props: &spa::utils::dict::DictRef) -> Option<Self> {
        if props.get(*pw::keys::MEDIA_CLASS)? != "Stream/Output/Audio" {
            return None;
        }
        let node = props.get(*pw::keys::NODE_NAME)?.to_string();
        Some(Self {
            app: props.get(*pw::keys::APP_NAME).unwrap_or(&node).to_string(),
            binary: props
                .get(*pw::keys::APP_PROCESS_BINARY)
                .unwrap_or_default()
                .to_string(),
            node,
        })
    }

    /// Whether `query` names this stream's app: its own name, its binary
    /// or the stream node's name, exactly — or, as macOS matches a
    /// running app, as part of the app's name.
    fn matches(&self, query: &str) -> bool {
        let query = query.to_lowercase();
        let app = self.app.to_lowercase();
        app == query
            || self.binary.to_lowercase() == query
            || self.node.to_lowercase() == query
            || app.contains(&query)
    }

    /// How this stream is said in a message: the app, and the node it
    /// plays through when that says anything more.
    pub(crate) fn describe(&self) -> String {
        if self.app == self.node {
            self.app.clone()
        } else {
            format!("{} (node {})", self.app, self.node)
        }
    }
}

/// The app a setup names, and the streams it has right now, matched by
/// name, binary or node name. A miss is an error that says what *is*
/// playing, because an app that is playing nothing cannot be captured at
/// all — there is no stream to link to.
pub(crate) fn find_app(query: &str) -> Result<(String, Vec<AppStream>)> {
    let streams = app_streams().context("asking PipeWire what is playing")?;
    let matched: Vec<AppStream> = streams
        .iter()
        .filter(|it| it.matches(query))
        .cloned()
        .collect();
    let Some(first) = matched.first() else {
        let playing: Vec<String> = if streams.is_empty() {
            vec!["none".to_string()]
        } else {
            streams.iter().map(AppStream::describe).collect()
        };
        bail!(
            "no app is playing sound that matches {query:?}; apps playing now: {}",
            playing.join(", ")
        );
    };
    Ok((first.app.clone(), matched))
}

/// Every app stream the graph has right now. This is what makes an app
/// capturable, so the doctor and the error messages are built from it.
pub(crate) fn app_streams() -> Result<Vec<AppStream>> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("creating a PipeWire loop")?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).context("creating a PipeWire context")?;
    let core = context
        .connect_rc(None)
        .context("connecting to PipeWire (is the session's PipeWire running?)")?;
    let registry = core
        .get_registry()
        .context("asking PipeWire for the graph's objects")?;
    let found = Rc::new(RefCell::new(Vec::new()));
    let _objects = registry
        .add_listener_local()
        .global({
            let found = found.clone();
            move |object| {
                if object.type_ != pw::types::ObjectType::Node {
                    return;
                }
                if let Some(stream) = object.props.and_then(AppStream::from_node) {
                    found.borrow_mut().push(stream);
                }
            }
        })
        .register();
    // The registry's first burst of objects ends with the answer to a
    // sync: there is nothing to wait for after that.
    let _done = core
        .add_listener_local()
        .done({
            let mainloop = mainloop.clone();
            move |_, _| mainloop.quit()
        })
        .register();
    core.sync(0).context("asking PipeWire which nodes exist")?;
    mainloop.run();
    let streams = found.borrow().clone();
    Ok(streams)
}

/// The id the server knows our own stream's node by, or `None` until it
/// has told us: `pw_stream_get_node_id` is the *server's* id for the node,
/// which arrives as the proxy's `bound` event after the stream is
/// connected, not when the stream is built.
fn our_node(node: &pw::stream::Stream) -> Option<u32> {
    let id = node.node_id();
    (id != spa::sys::SPA_ID_INVALID).then_some(id)
}

/// What a per-app capture drains: its own input ports, the ports of every
/// stream that matches the app, and the links between them.
///
/// The graph is watched rather than asked once, because an app's streams
/// appear and disappear as it plays; a stream that opens mid-take is
/// linked like the ones already there. Ports are matched by their channel
/// (`audio.channel`), so a stereo app's left channel feeds ours.
#[derive(Default)]
struct Links {
    /// Our own node's id, which the graph knows it by; the app's streams
    /// are linked to its ports.
    ours: Option<u32>,
    /// Every port the graph has told us about, by the port's own id. Which
    /// side a port is on is worked out when a link is made rather than
    /// when the port is seen: the id our node is known by arrives after
    /// ports can, too ([`our_node`]).
    ports: BTreeMap<u32, Port>,
    /// The app's streams, by node id: the nodes `query` has matched so
    /// far, including ones that appear mid-take.
    app: BTreeSet<u32>,
    /// Output and input port pairs already linked, so a second `global`
    /// event for one of them does not link it twice.
    linked: BTreeSet<(u32, u32)>,
    /// The links themselves, kept so they live: dropping one destroys it
    /// (as does the app's stream going away, which ends the take's sound
    /// with it).
    links: Vec<pw::link::Link>,
    /// Set once the app's last stream has gone and the log has said so,
    /// cleared when the app plays again (a second stop is worth saying
    /// too).
    silent: bool,
}

/// A port the graph has, and the node it hangs off.
struct Port {
    node: u32,
    /// The channel it carries (`audio.channel`: "FL", "FR", "MONO"), or
    /// the port's own name when it carries no channel.
    channel: String,
    input: bool,
}

impl Links {
    /// A `global` event: note what it is, then link whatever is now on
    /// both sides of a pair of ports.
    fn event(
        core: &pw::core::CoreRc,
        links: &Rc<RefCell<Self>>,
        node: &pw::stream::Stream,
        query: &str,
        object: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    ) {
        let mut known = links.borrow_mut();
        // Our own node's id is not known when the stream is connected: it
        // is what the server calls it, which arrives with the proxy's
        // `bound` event, so it is read again on every event until it has.
        known.ours = known.ours.or_else(|| our_node(node));
        match object.type_ {
            pw::types::ObjectType::Node => {
                if let Some(stream) = object.props.and_then(AppStream::from_node)
                    && stream.matches(query)
                    && known.app.insert(object.id)
                {
                    // Playing again: a later stop is worth saying, too.
                    known.silent = false;
                }
            }
            pw::types::ObjectType::Port => {
                if let Some(props) = object.props {
                    known.port(object.id, props);
                }
            }
            _ => {}
        }
        known.relink(core);
    }

    /// An object is gone: forget its ports (PipeWire has already dropped
    /// the links between them).
    ///
    /// An app's stream is gone when it stops playing — or quits. What is
    /// already recorded is the take's, so the capture goes on and delivers
    /// silence where that stream's sound was; the last stream going away
    /// is said once, because otherwise a take quietly has no sound from
    /// that moment.
    fn removed(links: &Rc<RefCell<Self>>, described: &str, id: u32) {
        let mut known = links.borrow_mut();
        let was_playing = known.app.remove(&id);
        known.ports.remove(&id);
        // A link with a port that is gone went with it; the pair is
        // forgotten, so a port that does come back is linked again.
        known
            .linked
            .retain(|(output, input)| *output != id && *input != id);
        if was_playing && known.app.is_empty() && !known.silent {
            known.silent = true;
            isocast_diagnostics::diagnostic!(
                "warn"; "audio: {described}: stopped playing; the take goes on without its sound"
            );
        }
    }

    /// A port the graph has, ours or the app's, kept until a link can be
    /// made.
    fn port(&mut self, id: u32, props: &spa::utils::dict::DictRef) {
        let Some(node) = props
            .get(*pw::keys::NODE_ID)
            .and_then(|node| node.parse::<u32>().ok())
        else {
            return;
        };
        let channel = props
            .get(*pw::keys::AUDIO_CHANNEL)
            .or_else(|| props.get(*pw::keys::PORT_NAME))
            .unwrap_or_default()
            .to_string();
        let input = props.get(*pw::keys::PORT_DIRECTION) == Some("in");
        self.ports.insert(
            id,
            Port {
                node,
                channel,
                input,
            },
        );
    }

    /// Link every pair of ports that has no link yet.
    ///
    /// Channels are matched by name where the graph names them; a stream
    /// that says nothing about its channels feeds ours in order, and one
    /// with a single port feeds every channel we have (what the graph
    /// itself does with a mono stream).
    fn relink(&mut self, core: &pw::core::CoreRc) {
        let Some(ours) = self.ours else {
            return;
        };
        let inputs: Vec<(String, u32)> = self
            .ports
            .iter()
            .filter(|(_, port)| port.node == ours && port.input)
            .map(|(id, port)| (port.channel.clone(), *id))
            .collect();
        // The pairs to make, before any are made: a link is an object of
        // its own, and making one must not disturb the reading of the
        // ports it goes between.
        let mut pairs: Vec<(u32, u32, u32)> = Vec::new();
        for node in &self.app {
            let ports: Vec<(String, u32)> = self
                .ports
                .iter()
                .filter(|(_, port)| port.node == *node && !port.input)
                .map(|(id, port)| (port.channel.clone(), *id))
                .collect();
            for (index, (channel, output)) in ports.iter().enumerate() {
                for input in feeds(&inputs, &ports, channel, index) {
                    pairs.push((*node, *output, input));
                }
            }
        }
        for (node, output, input) in pairs {
            if !self.linked.insert((output, input)) {
                continue;
            }
            if let Some(link) = Self::link(core, node, output, ours, input) {
                isocast_diagnostics::debug!(
                    "audio: linked port {output} of node {node} to port {input} of node {ours}"
                );
                self.links.push(link);
            }
        }
    }

    /// One link, from an output port of the app's stream to an input port
    /// of ours. A link that cannot be made is said once (the pair stays in
    /// `linked`, so a busy graph is not asked over and over).
    fn link(
        core: &pw::core::CoreRc,
        output_node: u32,
        output_port: u32,
        input_node: u32,
        input_port: u32,
    ) -> Option<pw::link::Link> {
        let props = pw::properties::properties! {
            *pw::keys::LINK_OUTPUT_NODE => output_node.to_string(),
            *pw::keys::LINK_OUTPUT_PORT => output_port.to_string(),
            *pw::keys::LINK_INPUT_NODE => input_node.to_string(),
            *pw::keys::LINK_INPUT_PORT => input_port.to_string(),
        };
        match core.create_object::<pw::link::Link>("link-factory", &props) {
            Ok(link) => Some(link),
            Err(err) => {
                isocast_diagnostics::diagnostic!(
                    "warn"; "audio: could not link port {output_port} of node {output_node} to \
                    port {input_port}: {err}"
                );
                None
            }
        }
    }
}

/// Silence a buffer that is not sound, answering its loudest value (`None`
/// when the buffer is sound).
///
/// A capture's buffer is whatever the graph handed over, and values no
/// audio graph produces do turn up in one: a buffer nobody has written
/// yet (a stream whose inputs were just relinked, a graph renegotiated
/// when another stream was linked to it) holds whatever its memory did,
/// and an app's own data is not sound either when a decoder or a file it
/// plays is not — the first buffer caught here held 3.777893e22 and
/// 8.355842e8, which turned out to be the first six *samples* of the file
/// being played. It is not only a wrong sample: `aac` refuses the whole
/// take over one value that is not a number ("Input contains (near)
/// NaN/+-Inf"), so a recording is lost to it. Anything a thousand times
/// full scale is not sound, and a buffer holding one of them is not
/// half-right, so the whole buffer goes (20 ms of silence is not heard; a
/// value in the middle of a quantum is).
fn silence_memory(samples: &mut [f32]) -> Option<f32> {
    let worst = samples.iter().fold(0.0f32, |worst, sample| {
        let magnitude = if sample.is_finite() {
            sample.abs()
        } else {
            f32::INFINITY
        };
        worst.max(magnitude)
    });
    if worst <= MAX_SAMPLE {
        return None;
    }
    samples.fill(0.0);
    Some(worst)
}

/// Which of our input ports an app stream's output port feeds.
///
/// Channels are matched by name where the graph names them, so a stereo
/// app's left channel feeds ours; a stream with a single port feeds every
/// channel we have (what the graph itself does with a mono stream); and
/// one whose channels are not named feeds ours in order. Ports the app has
/// and we do not (a 5.1 stream's centre, say) are left alone: the capture
/// is stereo, like macOS's.
fn feeds(
    inputs: &[(String, u32)],
    ports: &[(String, u32)],
    channel: &str,
    index: usize,
) -> Vec<u32> {
    if let Some((_, input)) = inputs.iter().find(|(ours, _)| ours == channel) {
        return vec![*input];
    }
    if ports.len() == 1 {
        return inputs.iter().map(|(_, input)| *input).collect();
    }
    inputs
        .get(index)
        .map(|(_, input)| vec![*input])
        .unwrap_or_default()
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

/// `sink`, taking samples interleaved in `channels` channels and passing
/// on only the ones `pick` names (from 0), in its order.
pub(crate) fn picking(sink: AudioSink, channels: usize, pick: Vec<usize>) -> AudioSink {
    let samples = sink.samples;
    AudioSink {
        samples: Box::new(move |interleaved: &[f32], at: Time| {
            let mut out = Vec::with_capacity(interleaved.len() / channels.max(1) * pick.len());
            for frame in interleaved.chunks_exact(channels.max(1)) {
                out.extend(pick.iter().map(|&c| frame[c]));
            }
            samples(&out, at)
        }),
        stopped: sink.stopped,
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
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::APP_NAME => "Isocast",
        *pw::keys::NODE_NAME => "isocast capture",
    };
    match target.node() {
        Some((node_name, monitor)) => {
            props.insert(*pw::keys::TARGET_OBJECT, node_name);
            if monitor {
                // A sink's *monitor* ports are what its name alone would
                // not give: without this the stream would link to the
                // sink's inputs.
                props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
            }
        }
        // An app's streams are linked to us by `Links` and never by the
        // session manager: asked for a playback stream it links the
        // capture to its default *source* (the microphone) instead, which
        // is the opposite of the app's sound. "0" is how a stream says it
        // has no target of its own.
        None => {
            props.insert(*pw::keys::TARGET_OBJECT, "0");
        }
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
        repaired: false,
        clock: Clock::default(),
    };
    let _listener = node
        .add_local_listener_with_user_data(samples)
        .param_changed(param_changed)
        .state_changed(state_changed)
        .process(process)
        .register()
        .context("watching the audio stream")?;

    // An app's sound is linked rather than asked for, so the capture
    // follows the graph: the streams the app plays through now, and the
    // ones it opens later in the take. The registry reports both (and our
    // own ports, which are what they are linked to).
    let links = Rc::new(RefCell::new(Links::default()));
    let _registry_watch = target
        .app()
        .map(|query| {
            let query = query.to_string();
            let registry = core
                .get_registry()
                .context("asking PipeWire for the graph's objects")?;
            let listener = registry
                .add_listener_local()
                .global({
                    let core = core.clone();
                    let links = links.clone();
                    let node = node.clone();
                    move |object| Links::event(&core, &links, &node, &query, object)
                })
                .global_remove({
                    let links = links.clone();
                    let described = target.describe();
                    move |id| Links::removed(&links, &described, id)
                })
                .register();
            Ok::<_, anyhow::Error>((registry, listener))
        })
        .transpose()?;

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
    if target.app().is_some() {
        // The app's streams are linked *to* our node, so the graph's id
        // for it is what a link needs. It is read here and on every
        // registry event (see [`Links::event`]) because the server's id
        // for the node arrives as the proxy's `bound` event, not with
        // `connect`.
        links.borrow_mut().ours = our_node(&node);
    }
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
    /// Set once a buffer's unsound samples have been said, so a
    /// renegotiation does not fill the log.
    repaired: bool,
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
    let mut samples: Vec<f32> = raw
        .as_chunks::<4>()
        .0
        .iter()
        .map(|four| f32::from_le_bytes(*four))
        .collect();
    data.sound(&mut samples);
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
    /// Say a buffer that is not sound, once, and record it as silence
    /// (see [`silence_memory`]).
    fn sound(&mut self, samples: &mut [f32]) {
        let Some(worst) = silence_memory(samples) else {
            return;
        };
        if !self.repaired {
            self.repaired = true;
            isocast_diagnostics::diagnostic!(
                "warn"; "audio: {}: a buffer held {worst:.3e}, which no audio graph produces; it \
                was recorded as silence",
                self.target.describe()
            );
        }
    }

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
pub(crate) fn format_params(channels: u32) -> Result<Vec<u8>> {
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

    /// An app is named the way macOS names one: what it calls itself, its
    /// binary or its stream's node, exactly or as part of the name.
    #[test]
    fn an_app_is_matched_by_name_binary_or_node() {
        let stream = |app: &str, binary: &str, node: &str| AppStream {
            app: app.to_string(),
            binary: binary.to_string(),
            node: node.to_string(),
        };
        let spotify = stream("Spotify", "spotify", "Spotify");
        assert!(spotify.matches("Spotify"));
        assert!(spotify.matches("spotify")); // case does not matter
        assert!(spotify.matches("pot")); // part of the name, as macOS does
        assert!(spotify.matches("spotify")); // the binary
        assert!(!spotify.matches("Chromium"));
        // An app that names no binary is still matched by its node name.
        let limusic = stream("Limusic", "", "Limusic");
        assert!(limusic.matches("Limusic"));
        assert!(!limusic.matches("Limusic2"));
    }

    /// A capture is stereo: an app's own channels line up, a mono stream
    /// feeds both, unnamed ones go in order, and channels we do not have
    /// (a 5.1 stream's centre) are not linked at all.
    #[test]
    fn app_ports_feed_ours_by_channel() {
        let ours = vec![("FL".to_string(), 10u32), ("FR".to_string(), 11)];
        // Stereo, named: left to left.
        let stereo = vec![("FL".to_string(), 20u32), ("FR".to_string(), 21)];
        assert_eq!(feeds(&ours, &stereo, "FL", 0), vec![10]);
        assert_eq!(feeds(&ours, &stereo, "FR", 1), vec![11]);
        // Mono: every channel of ours, as the graph does with a mono
        // stream.
        let mono = vec![("MONO".to_string(), 30u32)];
        assert_eq!(feeds(&ours, &mono, "MONO", 0), vec![10, 11]);
        // Unnamed: ours in order.
        let unnamed = vec![
            ("playback_1".to_string(), 40u32),
            ("playback_2".to_string(), 41),
        ];
        assert_eq!(feeds(&ours, &unnamed, "playback_1", 0), vec![10]);
        assert_eq!(feeds(&ours, &unnamed, "playback_2", 1), vec![11]);
        // More than we have: nothing to link the rest to.
        let surround = vec![
            ("FL".to_string(), 50u32),
            ("FR".to_string(), 51),
            ("FC".to_string(), 52),
            ("LFE".to_string(), 53),
        ];
        assert_eq!(feeds(&ours, &surround, "FC", 2), Vec::<u32>::new());
        assert_eq!(feeds(&ours, &surround, "LFE", 3), Vec::<u32>::new());
    }

    /// A buffer that is not sound becomes silence whole (a value in the
    /// middle of a quantum is heard), while ordinary samples — including
    /// a mix that went past full scale — are left alone.
    #[test]
    fn buffers_that_are_not_sound_become_silence() {
        let mut tone = vec![0.05f32; 4096];
        assert_eq!(silence_memory(&mut tone), None);
        assert!(tone.iter().all(|sample| *sample == 0.05));
        // Silence is sound: an unlinked stream's buffers are zeros, which
        // is exactly what a take wants while an app is not playing.
        let mut quiet = vec![0.0f32; 4096];
        assert_eq!(silence_memory(&mut quiet), None);
        let mut loud = vec![1.5f32; 4096];
        assert_eq!(silence_memory(&mut loud), None);
        // The values a played file's own corrupt start put in a buffer,
        // and what a buffer nobody wrote holds.
        for garbage in [
            f32::NAN,
            f32::INFINITY,
            -3.777893e22,
            8.355842e8,
            -8.430551e8,
        ] {
            let mut memory = vec![0.05f32; 4096];
            memory[1] = garbage;
            let worst = silence_memory(&mut memory).expect("garbage was taken for sound");
            assert!(worst.is_infinite() || worst > MAX_SAMPLE);
            assert!(
                memory.iter().all(|sample| *sample == 0.0),
                "{garbage} left samples behind"
            );
        }
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
