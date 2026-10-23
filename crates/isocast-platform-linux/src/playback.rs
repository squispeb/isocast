//! Audio played on an output device through PipeWire: what the engine's
//! monitoring (and the editor's sound) hears.
//!
//! One output stream on a thread of its own, in the engine's format (48 kHz
//! float stereo). PipeWire asks for each buffer from `process`, which has
//! the engine's `render` fill it, the pull model Core Audio's output unit
//! has on macOS. "default" leaves the stream without a target, so the
//! session manager links it to the default output and moves it when that
//! changes; a named output is the stream's `target.object`.
//!
//! The stream is a playback node like any app's, so a capture of the
//! output's monitor hears it too. It carries [`crate::audio::OWNER_KEY`],
//! which is how a system-audio capture that links streams (one with an
//! `exclude`, or with `exclude_own`) leaves Isocast's own monitoring out.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_engine::AudioRender;
use pipewire as pw;
use pw::spa;

use crate::system;

/// How long the stream may take to reach the device.
const OPEN_WAIT: Duration = Duration::from_secs(5);

/// Bytes in one frame: two 32-bit float samples.
const FRAME_BYTES: usize = 8;

/// Audio playing on an output. Dropping it stops the stream and joins its
/// thread.
pub struct Playback {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start playing what `render` fills on `device` ("default", or an output
/// from [`system::outputs`] by id, name or part of a name). Returns the
/// playback and the output's name.
pub(crate) fn play(device: &str, render: AudioRender) -> Result<(Playback, String)> {
    let (target, name) = match device {
        "default" => {
            let default = system::default_output()?;
            let name = system::outputs()
                .ok()
                .and_then(|outputs| outputs.into_iter().find(|output| output.id == default))
                .map_or(default, |output| output.name);
            (None, name)
        }
        query => {
            let output = system::find_output(query)?;
            (Some(output.id), output.name)
        }
    };
    let (stop, quit) = pw::channel::channel();
    let (report, reported) = mpsc::channel();
    let stopping = Arc::new(AtomicBool::new(false));
    let thread = isocast_diagnostics::spawn(
        "audio playback",
        {
            let stopping = stopping.clone();
            move || {
                if let Err(err) = run(target, render, quit, stopping, &report) {
                    let _ = report.send(Err(err));
                }
            }
        },
        |why| isocast_diagnostics::diagnostic!("error"; "the audio playback thread stopped: {why}"),
    )
    .context("starting the audio playback thread")?;
    let playback = Playback {
        stop,
        thread: Some(thread),
        stopping,
    };
    match reported.recv_timeout(OPEN_WAIT) {
        Ok(Ok(())) => Ok((playback, name)),
        Ok(Err(err)) => Err(err.context(format!("could not play through {name:?}"))),
        Err(RecvTimeoutError::Timeout) => bail!(
            "{name:?} did not start playing within {} s; is the output still there?",
            OPEN_WAIT.as_secs()
        ),
        Err(RecvTimeoutError::Disconnected) => {
            bail!("the audio playback's thread stopped before {name:?} started playing")
        }
    }
}

/// What the stream's callbacks work with.
struct Output {
    render: AudioRender,
    /// Says once that the stream reached the device (or why it can't).
    report: Option<Sender<Result<()>>>,
    stopping: Arc<AtomicBool>,
    /// The engine's samples for one buffer, kept to reuse its memory.
    samples: Vec<f32>,
}

impl Output {
    fn answer(&mut self, result: Result<()>) {
        if let Some(report) = self.report.take() {
            let _ = report.send(result);
        }
    }
}

/// The playback thread: build the stream and run its loop until stopped.
fn run(
    target: Option<String>,
    render: AudioRender,
    stop: pw::channel::Receiver<()>,
    stopping: Arc<AtomicBool>,
    report: &Sender<Result<()>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("creating a PipeWire loop")?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).context("creating a PipeWire context")?;
    let core = context
        .connect_rc(None)
        .context("connecting to PipeWire (is the session's PipeWire running?)")?;
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::APP_NAME => "Isocast",
        *pw::keys::NODE_NAME => "isocast monitor",
    };
    props.insert(crate::audio::OWNER_KEY, std::process::id().to_string());
    if let Some(target) = &target {
        props.insert(*pw::keys::TARGET_OBJECT, target.as_str());
    }
    let stream = pw::stream::StreamRc::new(core.clone(), "isocast monitor", props)
        .context("creating the playback stream")?;
    let output = Output {
        render,
        report: Some(report.clone()),
        stopping,
        samples: Vec::new(),
    };
    let _listener = stream
        .add_local_listener_with_user_data(output)
        .state_changed(|_, output, _old, new| match new {
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                output.answer(Ok(()))
            }
            pw::stream::StreamState::Error(message) => {
                if output.report.is_some() {
                    output.answer(Err(anyhow::anyhow!("the stream failed: {message}")));
                } else if !output.stopping.load(Ordering::SeqCst) {
                    isocast_diagnostics::diagnostic!("warn"; "audio playback stopped: {message}");
                }
            }
            _ => {}
        })
        .process(fill)
        .register()
        .context("watching the playback stream")?;

    let _quit = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });

    let format = crate::audio::format_params(2)?;
    let mut params =
        [spa::pod::Pod::from_bytes(&format).context("building the playback's format")?];
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("connecting to the audio output")?;
    mainloop.run();
    Ok(())
}

/// PipeWire wants a buffer: the engine fills as many frames as it asked
/// for (or as fit), and the chunk says how many there are.
fn fill(stream: &pw::stream::Stream, output: &mut Output) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let requested = buffer.requested() as usize;
    let Some(block) = buffer.datas_mut().first_mut() else {
        return;
    };
    let Some(bytes) = block.data() else {
        return;
    };
    let room = bytes.len() / FRAME_BYTES;
    let frames = if requested > 0 {
        requested.min(room)
    } else {
        room
    };
    output.samples.clear();
    output.samples.resize(frames * 2, 0.0);
    if !output.stopping.load(Ordering::SeqCst) {
        (output.render)(&mut output.samples);
    }
    for (four, sample) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(&output.samples) {
        *four = sample.to_le_bytes();
    }
    let chunk = block.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = FRAME_BYTES as i32;
    *chunk.size_mut() = (frames * FRAME_BYTES) as u32;
}
