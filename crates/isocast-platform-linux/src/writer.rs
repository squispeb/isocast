//! One .mov or .mp4 written by this machine's `ffmpeg`, fed over
//! pipes: the raw pictures into its stdin, one f32 pipe per audio
//! track, each read end moved onto a fixed fd of the child
//! (`pipe:3`, `pipe:4`, …) before the exec. Everything that can fail
//! is checked before a take starts (the binary, the encoders it
//! lists, the output's folder): once a take is running, a failed
//! encoder is a lost recording. The child itself starts lazily, at
//! the first frame, when the frame's pixel layout (BGRA or NV12) is
//! known; if it dies mid-take, its stderr's last words are carried
//! in the error, once, and every append after that says dropped, so
//! the engine counts frames instead of the same dead-child error.
//!
//! Raw pipes carry no timestamps, so the file's timeline is the order
//! the bytes went in: a dropped frame would compress it, so the
//! previous picture is written again for the missing ticks (and for
//! the tail at Stop). Fragmented muxing — a keyframe every two
//! seconds, the engine's `MOVIE_FRAGMENT_SECONDS` — keeps a killed
//! process's file playable up to its last fragment, the way
//! AVAssetWriter's fragments do on macOS.
//!
//! Every stream has a writer thread of its own: the pictures queued to
//! one, each audio track's samples to another. The child picks between
//! its inputs by timestamp, so it stops draining the video pipe until
//! it has taken an audio packet, and a picture (megabytes) is larger
//! than a pipe (64 KiB): written from the engine's own packet loop, a
//! picture in flight would hold the samples back, the samples would be
//! what the child waits for, and neither would ever move. The loop
//! only queues, the threads do the waiting, and the streams stay
//! independent — as AVAssetWriter's own buffering makes them on macOS.
//! A child that takes nothing at all for [`PIPE_STALL`] is killed
//! instead, so the writes blocked inside it fail and the file reports
//! why: a take must never hang on an encoder.

use std::ffi::OsString;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{FileWriter, Finished, SAMPLE_RATE, Time, WriterSpec};
use isocast_protocol::{Codec, Container, Encoder};

use crate::frames::{FrameKind, LinuxFrame};

/// How long Stop waits for the child to drain what it was fed. A
/// disk-bound backlog takes 7–15 s to reach the disk
/// (docs/port-omarchy.md S.2b), so the wait is long enough for that,
/// then the file fails with what its fragments hold.
const FINISH_WAIT: Duration = Duration::from_secs(60);
/// How many missing ticks one append may cover with the previous
/// picture: five seconds at 60 fps, so a long-frozen source can't
/// write minutes of repeats in one call.
const REPEATS_PER_CALL: u64 = 300;
/// How much of the child's stderr is kept: its last words, for the
/// error that says why it died.
const STDERR_TAIL: usize = 8 * 1024;
/// How many sample blocks one track's writer thread may hold before its
/// queue drops the new ones: about two and a half seconds at the
/// engine's 10 ms chunk, so an encoder's hiccup costs samples rather
/// than video.
const AUDIO_QUEUE_DEPTH: usize = 256;
/// How many bytes of pictures the video thread's queue may hold. The
/// engine's packet loop must never wait for the child (the samples the
/// child waits for arrive through that loop), so the queue takes what
/// a program mix delivers late — it computes audio a beat behind live,
/// which is a few pictures — and drops the rest whole: the previous
/// picture repeats for their ticks, so the file keeps its timeline.
const VIDEO_QUEUE_BYTES: usize = 64 * 1024 * 1024;
/// The queue's picture count, whatever their size: at least two, and no
/// more than this, so a small picture can't hold a minute of video.
const VIDEO_QUEUE_FRAMES: std::ops::RangeInclusive<usize> = 2..=64;
/// How long a write already in flight may take nothing at all before
/// the child is taken to be wedged and killed. Writes that are merely
/// slow (a disk-bound encoder draining its backlog) keep putting bytes
/// in, which resets this.
const PIPE_STALL: Duration = Duration::from_secs(20);
/// How long the stderr reader is given to reach a child's end once that
/// child is gone: it is reading a pipe the child's exit closes, so this
/// is a formality, not a wait.
const STDERR_WAIT: Duration = Duration::from_secs(2);
/// How long a child that is still alive is given to say its last words
/// in an error message: it has said what it has said by then, and a
/// take must not wait on a child that is still running.
const STDERR_GLANCE: Duration = Duration::from_millis(250);
/// How often ffmpeg reports its progress, in seconds (`-stats_period`).
const PROGRESS_PERIOD: &str = "0.1";
/// How often the watchdog looks.
const WATCHDOG_TICK: Duration = Duration::from_millis(100);

/// One media file being written by a child `ffmpeg`.
pub struct FfmpegFileWriter {
    /// The binary preflight verified: `ISOCAST_FFMPEG`, else `ffmpeg`
    /// from the PATH.
    program: OsString,
    spec: WriterSpec,
    /// The ffmpeg encoder chosen for `spec`'s codec, with the
    /// arguments that carry its quality.
    encoder: String,
    encoder_args: Vec<String>,
    /// The timeline's start, recorded by `start`.
    start: Option<Time>,
    child: Option<Child>,
    /// The pictures queued to the video pipe's own thread; `None` until
    /// the child starts.
    video: Option<SyncSender<Vec<u8>>>,
    video_thread: Option<thread::JoinHandle<()>>,
    /// One pipe per audio track, each with its own writer thread.
    audio: Vec<AudioPipe>,
    /// What the watchdog reads: how the pipes' writes are going.
    progress: Arc<Progress>,
    /// Kills the child if its writes stop moving (a wedged encoder).
    watchdog: Option<thread::JoinHandle<()>>,
    /// Set when the watchdog is no longer wanted, so it can't outlive
    /// the child it watches (whose pid is reaped by `finish`/`cancel`).
    watchdog_stop: Arc<AtomicBool>,
    /// The child's stderr, drained by its own thread (a chatty encoder
    /// must not deadlock on a pipe nobody reads) and kept to its last
    /// words.
    stderr: Arc<Mutex<Vec<u8>>>,
    /// Set when that thread has reached the child's end, so a reader can
    /// be waited for without waiting for a child that is still running.
    stderr_ended: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
    /// Pictures the encoder has returned, as ffmpeg's `-progress` reports
    /// them (its own `frame=`), kept by a thread of its own. This is what
    /// calibration counts: completed pictures, not what the pipe accepted.
    completed: Arc<AtomicU64>,
    /// The progress reader started, so `completed` is a real count.
    counting: bool,
    progress_reader: Option<thread::JoinHandle<()>>,
    /// The last picture written: its time and its bytes, what a later
    /// frame's gaps repeat and the tail repeats at Stop.
    last_pts: Option<Time>,
    last_bytes: Option<Vec<u8>>,
    /// Pictures written again to cover missing ticks.
    repeats: u64,
    /// The encoder ended early: appends say dropped instead of raising
    /// the same dead-child error again.
    broken: bool,
    /// Why a stream thread's write failed, for the append or `finish`
    /// that reports it (the child's own exit status is the other half
    /// of the story).
    failure: Arc<Mutex<Option<String>>>,
    /// The raw stream's frame size, fixed where the child started.
    pixels: Option<(u32, u32)>,
    /// How far the file's whole fragments reach, read back from the file
    /// (crash safety, fragments.rs). Behind a lock because `committed`
    /// reads it through `&self`.
    fragments: Mutex<crate::fragments::Scan>,
    /// The folder's entry for the file has been synced (once is enough).
    entry_synced: bool,
}

/// One audio track's pipe: the samples that arrived before the child
/// started (buffered with no cap — the engine drains its own
/// pre-start buffer the same way at the first frame), then the queue
/// its own writer thread takes them from.
struct AudioPipe {
    pending: Vec<f32>,
    /// `None` until the child starts and the thread takes the pipe.
    queue: Option<SyncSender<Vec<u8>>>,
    thread: Option<thread::JoinHandle<()>>,
    /// Bytes of samples a full queue refused: the raw pipe carries no
    /// timestamps, so dropping them would move every later sample
    /// earlier, and they go in as silence ahead of the next block that
    /// fits. The watchdog bounds this: a queue that stays full means a
    /// write is in flight, and a wedged child is killed within
    /// [`PIPE_STALL`].
    owed: usize,
}

impl AudioPipe {
    /// `samples` into this track's queue, with the silence it owes ahead
    /// of them: `false` when the child isn't taking samples (they are
    /// owed too, so the file's track keeps its length).
    fn send_samples(&mut self, samples: &[f32]) -> bool {
        let Some(queue) = self.queue.clone() else {
            return false;
        };
        let bytes = f32_bytes(samples);
        let owed = std::mem::take(&mut self.owed);
        let mut block = Vec::with_capacity(owed + bytes.len());
        block.resize(owed, 0);
        block.extend_from_slice(bytes);
        match queue.try_send(block) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                // Full: the child is behind. Disconnected: it is gone,
                // which its exit status will report. Either way these
                // samples are owed, and the ones after them keep their
                // places.
                self.owed = owed + bytes.len();
                false
            }
        }
    }
}

impl FfmpegFileWriter {
    /// A writer for `spec`, with everything that can fail checked
    /// first, because after a take starts a failed encoder is a lost
    /// recording: the binary runs and lists the encoder it will use,
    /// the output's folder exists and takes writes, and the output
    /// isn't already there (`-n` below would refuse it mid-take,
    /// which is too late to say so). The child starts at the first
    /// frame; its stderr's last words are carried in the error if it
    /// dies mid-take.
    pub fn spawn(spec: &WriterSpec) -> Result<Self> {
        let program = ffmpeg_binary();
        let listed = listed_encoders(&program)?;
        let (encoder, mut encoder_args) = choose_encoder(&spec.video.encoder, &listed)?;
        encoder_args.extend(depth_args(&spec.video.encoder, encoder));
        probe_folder(&spec.path)?;
        if spec.path.exists() {
            bail!(
                "{} already exists; not recording over it",
                spec.path.display()
            );
        }
        let writer = Self {
            program,
            spec: spec.clone(),
            audio: spec
                .audio
                .iter()
                .map(|_| AudioPipe {
                    pending: Vec::new(),
                    queue: None,
                    thread: None,
                    owed: 0,
                })
                .collect(),
            encoder: encoder.to_owned(),
            encoder_args,
            start: None,
            child: None,
            video: None,
            video_thread: None,
            progress: Arc::new(Progress::new()),
            watchdog: None,
            watchdog_stop: Arc::new(AtomicBool::new(false)),
            stderr: Arc::new(Mutex::new(Vec::new())),
            stderr_ended: Arc::new(AtomicBool::new(false)),
            reader: None,
            completed: Arc::new(AtomicU64::new(0)),
            counting: false,
            progress_reader: None,
            last_pts: None,
            last_bytes: None,
            repeats: 0,
            broken: false,
            failure: Arc::new(Mutex::new(None)),
            pixels: None,
            fragments: Mutex::new(crate::fragments::Scan::default()),
            entry_synced: false,
        };
        isocast_diagnostics::debug!(
            "writer: {} {:?} {}x{}@{} ({} audio tracks): {} preflighted",
            spec.path.display(),
            spec.video.encoder.codec,
            spec.video.width,
            spec.video.height,
            spec.video.fps,
            spec.audio.len(),
            writer.encoder
        );
        Ok(writer)
    }

    /// Start the ffmpeg child. The input's pixel format comes from the
    /// first frame, so the encoder starts here, at the first video
    /// picture, and the audio buffered until now follows it into the
    /// pipes.
    fn spawn_child(&mut self, kind: FrameKind, pixels: (u32, u32)) -> Result<()> {
        let (width, height) = pixels;
        let mut command = Command::new(&self.program);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Nothing it says goes anywhere but stderr (its errors) and
        // stdout (only the machine-readable `-progress` report, a
        // pipe of its own so it never mixes with the errors), and
        // nothing it reads comes from anywhere but the pipes; nothing
        // is ever overwritten.
        command.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-n"]);
        // The completed-picture counter's source: a report every 0.1 s
        // (the default 0.5 s is too coarse for calibration's 0.6 s window).
        command.args(["-progress", "pipe:1", "-stats_period", PROGRESS_PERIOD]);
        // The raw input: BGRA as it lies, Y'CbCr's two planes in
        // NV12's order, luma then interleaved chroma.
        let pixel_format = match kind {
            FrameKind::Bgra => "bgra",
            FrameKind::Ycbcr8 => "nv12",
        };
        command
            .arg("-f")
            .arg("rawvideo")
            .arg("-pixel_format")
            .arg(pixel_format)
            .arg("-video_size")
            .arg(format!("{width}x{height}"))
            .arg("-framerate")
            .arg(self.spec.video.fps.to_string())
            .arg("-i")
            .arg("pipe:0");
        // A program file written smaller than the canvas is scaled to
        // the file's size the way AVAssetWriter's ResizeAspect is on
        // macOS: the picture keeps its shape, centred, with black bars
        // beside it.
        let (out_width, out_height) = (self.spec.video.width, self.spec.video.height);
        let mut filters = Vec::new();
        if kind == FrameKind::Ycbcr8 {
            // The engine's Y'CbCr is Rec. 709, video range, which a raw
            // pipe can't say: untagged, ffmpeg took it for another matrix
            // and converted every picture through RGB on the CPU to the
            // Rec. 709 the output is tagged with, shifting its colours.
            filters.push(
                "setparams=colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv"
                    .to_string(),
            );
        }
        if pixels != (out_width, out_height) {
            filters.push(format!(
                "scale={out_width}:{out_height}:force_original_aspect_ratio=decrease,\
                 pad={out_width}:{out_height}:(ow-iw)/2:(oh-ih)/2"
            ));
        }
        // Output options: they go after every input, or ffmpeg applies
        // them to the next input on the line (an audio pipe), and fails.
        let filters = filters.join(",");
        // One pipe per audio track, its read end at the fixed fd
        // `3 + the track's index`. The read ends are first duplicated
        // onto high fds (100 and up): every pipe2 hands out numbers
        // from the same low end of the table, so without that step the
        // child's dup2 would move one pipe's read end onto another
        // pipe's own fd.
        let mut read_ends: Vec<(OwnedFd, RawFd)> = Vec::new();
        let mut writes: Vec<OwnedFd> = Vec::new();
        for (index, track) in self.spec.audio.iter().enumerate() {
            let mut pair = [0 as libc::c_int; 2];
            // SAFETY: pipe2 writes the two fds of a new pipe into
            // `pair` on success; O_CLOEXEC keeps both ends from
            // surviving an exec by accident (the read end moves to its
            // fixed number before the exec, and the write end stays
            // in this process).
            if unsafe { libc::pipe2(pair.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                let err = std::io::Error::last_os_error();
                return Err(anyhow!("could not make audio track {index}'s pipe: {err}"));
            }
            let (read, write) = (pair[0], pair[1]);
            // SAFETY: fcntl(F_DUPFD_CLOEXEC) duplicates `read` onto the
            // lowest free fd at or above 100 — out of the small
            // numbers the child moves things onto — and marks the
            // duplicate close-on-exec, so an exec the pre_exec step
            // below doesn't run can't leak it either.
            let high = unsafe { libc::fcntl(read, libc::F_DUPFD_CLOEXEC, 100) };
            if high < 0 {
                let err = std::io::Error::last_os_error();
                // SAFETY: both fds are pipe2's, owned by nothing else.
                unsafe {
                    libc::close(read);
                    libc::close(write);
                }
                return Err(anyhow!("could not park audio track {index}'s pipe: {err}"));
            }
            // SAFETY: `high` and `write` are the two fds just made by
            // fcntl and pipe2, owned by nothing else.
            read_ends.push((unsafe { OwnedFd::from_raw_fd(high) }, 3 + index as RawFd));
            writes.push(unsafe { OwnedFd::from_raw_fd(write) });
            // SAFETY: the original read end is owned only here, in the
            // parent; the child hasn't been forked yet, so closing it
            // now leaves the parent holding exactly one end of it.
            unsafe { libc::close(read) };
            command
                .arg("-f")
                .arg("f32le")
                .arg("-ar")
                .arg(SAMPLE_RATE.to_string())
                .arg("-ac")
                .arg(track.channels.to_string())
                .arg("-i")
                .arg(format!("pipe:{}", 3 + index));
        }
        let handed_over = read_ends
            .iter()
            .map(|(fd, target)| (fd.as_raw_fd(), *target))
            .collect::<Vec<_>>();
        let move_read_ends = move || {
            for (high, target) in &handed_over {
                // SAFETY (in the child): dup2 copies the pipe's read end
                // at `high` onto `target`, the number ffmpeg was told;
                // dup2 clears close-on-exec on the fd it makes, which is
                // the point — the pipe must survive the exec.
                if unsafe { libc::dup2(*high, *target) } < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY (in the child): FD_CLOEXEC off, said the only
                // way that leaves no doubt about the fd's state across
                // the exec.
                unsafe { libc::fcntl(*target, libc::F_SETFD, 0 as libc::c_int) };
                // SAFETY (in the child): the high duplicate was only a
                // way to move the read end here; the child won't look at
                // that number again.
                unsafe { libc::close(*high) };
            }
            Ok(())
        };
        // SAFETY: the closure runs in the forked child between stdio's
        // own dup2s and the exec, when no other thread exists, and it
        // only calls dup2, fcntl and close — async-signal-safe, and
        // only on fds made for it above.
        unsafe { command.pre_exec(move_read_ends) };
        // Every input goes to its own track in the output: ffmpeg's
        // default selection would keep one audio stream and drop the
        // rest.
        command.arg("-map").arg("0:v");
        for index in 0..self.spec.audio.len() {
            command.arg("-map").arg(format!("{}:a", index + 1));
        }
        if !filters.is_empty() {
            command.arg("-vf").arg(&filters);
        }
        // The video encoder and its quality, then what every file
        // takes: no B-pictures (a recoverable fragment timeline keeps
        // presentation order), a keyframe every two seconds (the
        // fragments' length), Rec. 709 like the engine's Y'CbCr.
        command.arg("-c:v").arg(&self.encoder);
        command.args(&self.encoder_args);
        command
            .arg("-bf")
            .arg("0")
            .arg("-g")
            .arg(self.spec.video.fps.saturating_mul(2).to_string())
            .arg("-color_primaries")
            .arg("bt709")
            .arg("-color_trc")
            .arg("bt709")
            .arg("-colorspace")
            .arg("bt709");
        if !self.spec.audio.is_empty() {
            command.arg("-c:a").arg("aac");
        }
        // Fragmented muxing: a killed process keeps what it stored. Each
        // fragment goes to the file when the muxer has it (`flush_packets`):
        // left to its I/O buffer, a quiet picture's fragments waited in
        // memory for tens of seconds, where a crash of ffmpeg loses them.
        command
            .arg("-flush_packets")
            .arg("1")
            .arg("-movflags")
            .arg("+frag_keyframe+empty_moov+default_base_moof")
            .arg("-f")
            .arg(container_name(self.spec.container))
            .arg(&self.spec.path);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                self.broken = true;
                return Err(anyhow!(
                    "could not start ffmpeg ({}): {err}",
                    self.program.to_string_lossy()
                ));
            }
        };
        // The parent's duplicates existed only to hand the read ends
        // over; the child now holds its own at 3 + each track's index.
        drop(read_ends);
        let stderr = child.stderr.take();
        if let Some(stderr) = stderr {
            self.start_stderr_reader(stderr);
        }
        if let Some(stdout) = child.stdout.take() {
            self.start_progress_reader(stdout);
        }
        // The pictures go in from a thread of their own as well. The
        // child picks between its inputs by timestamp, so it stops
        // taking video until it has taken samples that the program mix
        // only produces a beat behind live: written from the engine's
        // packet loop, a picture would hold those samples back, and
        // each would wait for the other for good.
        let expected = raw_len(pixels, kind).max(1);
        let depth = (VIDEO_QUEUE_BYTES / expected)
            .clamp(*VIDEO_QUEUE_FRAMES.start(), *VIDEO_QUEUE_FRAMES.end());
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            self.broken = true;
            self.note_failure("ffmpeg has no stdin to feed the pictures into");
            return Err(anyhow!(
                "ffmpeg for {} has no stdin to feed the pictures into",
                self.spec.path.display()
            ));
        };
        let (queue, pictures) = sync_channel(depth);
        let progress = self.progress.clone();
        let failure = self.failure.clone();
        match isocast_diagnostics::spawn(
            "video writer",
            move || video_writer(stdin, pictures, progress, failure),
            |why| isocast_diagnostics::diagnostic!("error"; "the video writer stopped: {why}"),
        ) {
            Ok(thread) => {
                self.video = Some(queue);
                self.video_thread = Some(thread);
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                self.broken = true;
                let why = format!("the video writer thread: {err}");
                self.note_failure(&why);
                return Err(anyhow!("could not start {why}"));
            }
        }
        // Each track's few kilobytes go in from a thread of its own too,
        // for the same reason in the other direction: the samples must
        // reach the child while a picture waits, not behind it.
        for (index, write) in writes.into_iter().enumerate() {
            let (queue, samples) = sync_channel(AUDIO_QUEUE_DEPTH);
            let progress = self.progress.clone();
            let failure = self.failure.clone();
            let thread = isocast_diagnostics::spawn(
                format!("audio writer {index}"),
                move || audio_writer(write, samples, progress, failure),
                move |why| {
                    isocast_diagnostics::diagnostic!(
                        "error"; "the audio writer for track {index} stopped: {why}"
                    )
                },
            );
            match thread {
                Ok(thread) => {
                    self.audio[index].queue = Some(queue);
                    self.audio[index].thread = Some(thread);
                }
                Err(err) => {
                    // This track has no way to feed the child. The
                    // child is already up: it goes now rather than
                    // leaving a file that quietly lost a track.
                    let _ = child.kill();
                    let _ = child.wait();
                    self.broken = true;
                    let why = format!("audio track {index}'s writer thread: {err}");
                    self.note_failure(&why);
                    return Err(anyhow!("could not start {why}"));
                }
            }
        }
        self.watchdog = match start_watchdog(
            child.id(),
            self.progress.clone(),
            self.watchdog_stop.clone(),
        ) {
            Ok(watchdog) => Some(watchdog),
            // Without it, a wedged encoder would hang the take for
            // good: the child goes now, like the other failures here.
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                self.broken = true;
                let why = format!("the encoder watchdog thread: {err}");
                self.note_failure(&why);
                return Err(anyhow!("could not start {why}"));
            }
        };
        self.child = Some(child);
        self.pixels = Some(pixels);
        // The samples that arrived before the child follow it in.
        let buffered = self
            .audio
            .iter_mut()
            .enumerate()
            .map(|(track, pipe)| (track, std::mem::take(&mut pipe.pending)))
            .collect::<Vec<_>>();
        for (track, samples) in buffered {
            if samples.is_empty() {
                continue;
            }
            if !self.audio[track].send_samples(&samples) {
                isocast_diagnostics::diagnostic!(
                    "warn";
                    "{}: audio track {track} arrived with {} bytes before its first \
                     picture, more than the queue takes: the rest goes in as silence \
                     with the next block that fits",
                    self.spec.path.display(),
                    f32_bytes(&samples).len()
                );
            }
        }
        isocast_diagnostics::debug!(
            "writer: {} ffmpeg started ({}, {}x{} {} input, {} audio pipes)",
            self.spec.path.display(),
            self.encoder,
            width,
            height,
            pixel_format,
            self.audio.len()
        );
        Ok(())
    }

    /// A picture's bytes into the video thread's queue: `Ok(false)` when
    /// the queue is full (the child is more than a beat behind, so the
    /// picture is dropped whole — the newest picture already queued
    /// repeats for its ticks) or when the thread is gone (the child died
    /// under it).
    fn queue_video(&mut self, bytes: &[u8]) -> Result<bool> {
        let Some(queue) = &self.video else {
            return Ok(false);
        };
        Ok(queue.try_send(bytes.to_vec()).is_ok())
    }

    /// A picture into the queue, waiting for room: for the tail at Stop,
    /// which must land for the file to reach its end. The watchdog is
    /// still watching whoever calls this, so a wedged child ends the
    /// wait rather than the take.
    fn queue_video_waiting(&mut self, bytes: &[u8]) -> Result<bool> {
        let Some(queue) = &self.video else {
            return Ok(false);
        };
        Ok(queue.send(bytes.to_vec()).is_ok())
    }

    /// The encoder process ended, or one of its pipes broke: latch the
    /// file (every later append says dropped, so the engine counts
    /// frames, not repeated errors) and say why, with the last words
    /// its stderr has reached.
    fn pipe_died(&mut self, why: impl std::fmt::Display) -> anyhow::Error {
        self.broken = true;
        // The child isn't waited for here: reaping it would free its pid
        // while the watchdog may still be watching (the failing thread
        // may have left a write blocked in another pipe, which only the
        // watchdog can end), and this runs in the engine's packet loop,
        // where no other file may wait either. What the encoder has said
        // so far is the tail.
        self.drain_stderr(STDERR_GLANCE);
        let tail = self.stderr_tail();
        anyhow!(
            "the ffmpeg encoder ended early writing {}: {why}; its last words: {tail}",
            self.spec.path.display()
        )
    }

    /// What a stream thread's write failed with, taken: `None` when
    /// there is nothing to report.
    fn stream_failure(&self) -> Option<String> {
        self.failure
            .lock()
            .expect("the writer's failure slot is poisoned")
            .take()
    }

    /// Put `why` in the failure slot (the first one wins), for the
    /// `finish` that has no exit status to report: a file that never got
    /// a child, or one whose stream stopped.
    fn note_failure(&self, why: impl std::fmt::Display) {
        record_why(&self.failure, &why.to_string());
    }

    /// No more kills: the watchdog is stopped and joined, so no tick can
    /// outlive the child it watches. Every reap is preceded by this.
    fn stop_watchdog(&mut self) {
        self.watchdog_stop.store(true, Ordering::Release);
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }

    /// Let the stderr reader reach the child's end, for at most `wait`:
    /// a dead child's reader ends in a moment, a live one's hasn't, and
    /// nothing here may wait on a child that is still running. The
    /// handle is joined when the reader is done, and otherwise left to
    /// end on its own.
    fn drain_stderr(&mut self, wait: Duration) {
        let Some(reader) = self.reader.take() else {
            return;
        };
        let deadline = Instant::now() + wait;
        while !self.stderr_ended.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = reader.join();
    }

    /// What the encoder said last: its stderr, kept to its final
    /// [`STDERR_TAIL`] bytes.
    fn stderr_tail(&self) -> String {
        let tail = self.stderr.lock().expect("ffmpeg stderr tail poisoned");
        String::from_utf8_lossy(&tail).trim().to_string()
    }

    /// The thread that keeps the child's stderr drained, so a chatty
    /// encoder can't deadlock on a pipe nobody reads. If the thread
    /// can't be made, the read end is dropped instead and the encoder
    /// writes its errors to a closed pipe.
    fn start_stderr_reader(&mut self, mut stderr: ChildStderr) {
        let tail = self.stderr.clone();
        let ended = self.stderr_ended.clone();
        let reader = isocast_diagnostics::spawn(
            "ffmpeg stderr",
            move || {
                let mut chunk = [0u8; 4096];
                loop {
                    let read = match stderr.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => read,
                        Err(_) => break,
                    };
                    let mut tail = tail.lock().expect("ffmpeg stderr tail poisoned");
                    tail.extend_from_slice(&chunk[..read]);
                    let excess = tail.len().saturating_sub(STDERR_TAIL);
                    if excess > 0 {
                        tail.drain(..excess);
                    }
                }
                ended.store(true, Ordering::Release);
            },
            |why| {
                isocast_diagnostics::diagnostic!(
                    "error"; "the ffmpeg stderr reader stopped: {why}"
                );
            },
        );
        self.reader = reader.ok();
    }
}

impl FfmpegFileWriter {
    /// The thread that reads ffmpeg's `-progress` report and keeps its
    /// latest `frame=`. Without the thread the pipe would fill and stall
    /// the encoder, so if it can't start the read end is dropped instead
    /// (ffmpeg then ignores the closed pipe) and no count is offered.
    fn start_progress_reader(&mut self, stdout: ChildStdout) {
        let completed = self.completed.clone();
        let reader = isocast_diagnostics::spawn(
            "ffmpeg progress",
            move || {
                let mut lines = std::io::BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match std::io::BufRead::read_line(&mut lines, &mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Some(frames) = progress_frames(&line) {
                                completed.store(frames, Ordering::Release);
                            }
                        }
                    }
                }
            },
            |why| {
                isocast_diagnostics::diagnostic!(
                    "error"; "the ffmpeg progress reader stopped: {why}"
                );
            },
        );
        self.counting = reader.is_ok();
        self.progress_reader = reader.ok();
    }

    /// Join the progress reader once the child is reaped (its pipe is at
    /// EOF, so this doesn't wait).
    fn join_progress_reader(&mut self) {
        if let Some(reader) = self.progress_reader.take() {
            let _ = reader.join();
        }
    }
}

/// The picture count in one line of ffmpeg's `-progress` report
/// (`frame=123`), if it is that line.
fn progress_frames(line: &str) -> Option<u64> {
    line.trim().strip_prefix("frame=")?.trim().parse().ok()
}

impl FileWriter for FfmpegFileWriter {
    type Frame = LinuxFrame;

    /// The ffmpeg child starts at the first picture, whose pixel format
    /// it needs, so NVENC's initialisation happens after `start`.
    fn starts_at_first_picture(&self) -> bool {
        true
    }

    /// Pictures ffmpeg's encoder has returned so far (its `-progress`
    /// `frame=`, at most 0.1 s old), `None` before the child starts or if
    /// the report can't be read.
    fn completed_video_frames(&self) -> Option<u64> {
        (self.counting && self.child.is_some()).then(|| self.completed.load(Ordering::Acquire))
    }

    /// Whether the chosen ffmpeg encoder is a hardware one, by the family
    /// suffix of its name (`hevc_nvenc`); `prores_ks`, `libx264` and
    /// `libx265` are software.
    fn hardware_accelerated(&self) -> Option<bool> {
        Some(crate::system::is_hardware_encoder(&self.encoder))
    }

    /// The file is fragmented MOV/MP4, playable up to its last whole
    /// fragment: what that reaches is read back from the file itself.
    fn crash_safe(&self) -> bool {
        true
    }

    /// Everything ffmpeg has written reaches the disk (`fsync`, and the
    /// folder's entry the first time), and the answer is what the whole
    /// fragments then cover. Pictures still inside ffmpeg's encoder, or in
    /// the fragment it is filling (a keyframe every 2 s ends one), are
    /// not in it: the answer is a fact, not a promise.
    fn commit(&mut self) -> Result<Option<std::time::Duration>> {
        let covered = {
            let mut scan = self.fragments.lock().unwrap();
            scan.advance(&self.spec.path)?;
            scan.covered()
        };
        match std::fs::File::open(&self.spec.path) {
            Ok(file) => file
                .sync_data()
                .with_context(|| format!("syncing {}", self.spec.path.display()))?,
            // ffmpeg starts at the first picture: nothing to keep yet.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some(std::time::Duration::ZERO));
            }
            Err(err) => return Err(err).context("opening the file to sync it"),
        }
        if !self.entry_synced
            && let Some(folder) = self.spec.path.parent()
            && let Ok(folder) = std::fs::File::open(folder)
        {
            self.entry_synced = folder.sync_all().is_ok();
        }
        Ok(Some(covered))
    }

    fn committed(&self) -> Option<std::time::Duration> {
        let mut scan = self.fragments.lock().unwrap();
        scan.advance(&self.spec.path).ok()?;
        Some(scan.covered())
    }

    /// The timeline's start, recorded: the raw streams carry no
    /// timestamps, so the first picture's own bytes start the file.
    fn start(&mut self, at: Time) -> Result<()> {
        self.start = Some(at);
        Ok(())
    }

    fn append_video(&mut self, frame: &LinuxFrame, pts: Time) -> Result<bool> {
        if self.broken {
            return Ok(false);
        }
        // A picture thread that stopped says why here, once: the rest of
        // the take's appends say dropped instead.
        if let Some(why) = self.stream_failure() {
            return Err(self.pipe_died(why));
        }
        // The compositor may have rendered this slot's texels since the
        // last read-back: the CPU bytes must catch up before the
        // encoder can take them.
        if frame.render_version() > frame.written_back() {
            crate::gpu::read_back(frame)?;
        }
        let pixels = frame.size();
        let bytes = raw_bytes(frame)?;
        let expected = raw_len(pixels, frame.kind());
        if bytes.len() != expected {
            bail!(
                "the frame's planes hold {} bytes where a {}x{} {:?} frame needs \
                 {expected}: the pool's layout and its size disagree",
                bytes.len(),
                pixels.0,
                pixels.1,
                frame.kind()
            );
        }
        if self.child.is_none() {
            self.spawn_child(frame.kind(), pixels)?;
        } else if let Some(opened) = self.pixels.filter(|opened| *opened != pixels) {
            bail!(
                "a {}x{} frame can't join a raw stream opened at {}x{}: rawvideo's \
                 layout is fixed at its first frame",
                pixels.0,
                pixels.1,
                opened.0,
                opened.1
            );
        }
        // Raw video has no timestamps, so a dropped frame compresses
        // the file's timeline: the previous picture goes in again for
        // the missing ticks, keeping every picture that did arrive
        // where it fell.
        let fps = self.spec.video.fps;
        if let Some(last) = self.last_pts {
            let missing = (pts.ticks_since(last, fps) - 1).clamp(0, REPEATS_PER_CALL as i64) as u64;
            if missing > 0 {
                let again = self
                    .last_bytes
                    .clone()
                    .expect("a written picture's bytes follow its time");
                let mut written_again = 0;
                for _ in 0..missing {
                    if !self.queue_video(&again)? {
                        break;
                    }
                    written_again += 1;
                }
                if written_again > 0 {
                    self.last_pts = Some(last.add_ticks(written_again, fps));
                    self.repeats += written_again;
                }
                if written_again < missing {
                    // Room ran out before the gap was covered: this
                    // frame is dropped whole rather than half of it
                    // going in (half a frame would shift every
                    // following one), and takes the older picture's
                    // place as what a later gap repeats — the newest
                    // picture the encoder can be given is the one that
                    // should stand in for the ticks, or a queue that is
                    // always full by one would hold that old one there
                    // for the rest of the take.
                    self.last_bytes = Some(bytes);
                    return Ok(false);
                }
            }
        }
        if !self.queue_video(&bytes)? {
            self.last_bytes = Some(bytes);
            return Ok(false);
        }
        self.last_pts = Some(pts);
        self.last_bytes = Some(bytes);
        Ok(true)
    }

    fn append_audio(&mut self, track: usize, samples: &[f32], _pts: Time) -> Result<bool> {
        if self.broken {
            return Ok(false);
        }
        if let Some(why) = self.stream_failure() {
            return Err(self.pipe_died(why));
        }
        // The child starts at the first video frame; until then the
        // samples wait here with no cap, drained the moment it starts.
        let unspawned = self.child.is_none();
        let tracks = self.audio.len();
        let Some(pipe) = self.audio.get_mut(track) else {
            bail!("audio track {track} doesn't exist on this file (it has {tracks} tracks)");
        };
        if unspawned {
            pipe.pending.extend_from_slice(samples);
            return Ok(true);
        }
        if samples.is_empty() {
            return Ok(true);
        }
        // The track's own thread writes it, with any silence it owes
        // ahead of it. A queue that is full means the child has stopped
        // taking samples: they are dropped (and counted) rather than
        // holding up this file's video.
        Ok(pipe.send_samples(samples))
    }

    fn finish(mut self, end: Time) -> Result<Finished> {
        // Cover the tail: the last picture that arrived may be frames
        // short of `end`, and the file should play up to its end. The
        // padding waits for room rather than being skipped — a file
        // whose video stops short of the audio the engine padded to
        // `end` would end misaligned — while the watchdog is still
        // watching, so a wedged encoder fails the file instead of the
        // wait being forever.
        let fps = self.spec.video.fps;
        if !self.broken
            && let Some(last) = self.last_pts
        {
            let missing = (end.ticks_since(last, fps) - 1).clamp(0, REPEATS_PER_CALL as i64) as u64;
            if missing > 0 {
                let again = self
                    .last_bytes
                    .clone()
                    .expect("a written picture's bytes follow its time");
                for _ in 0..missing {
                    match self.queue_video_waiting(&again) {
                        Ok(true) => self.repeats += 1,
                        // The thread is gone: the child ended on its
                        // own, and its exit status below says why.
                        _ => break,
                    }
                }
            }
        }
        // EOF on every pipe: the child flushes and writes its trailer,
        // and nothing kills it early — what it stored must stay. Every
        // sender goes before any thread is joined, or the two streams
        // would wait for each other: ffmpeg's muxer holds the stream
        // that is ahead (the engine has just padded audio to `end`)
        // until the other reaches it, and each thread would wait for its
        // peer's EOF. A dropped sender keeps the items already in its
        // queue: the threads drain those, then close their pipes.
        for pipe in &mut self.audio {
            // Silence still owed for blocks the queue refused: the
            // track must reach the end its video does.
            if pipe.owed > 0 {
                let _ = pipe.send_samples(&[]);
            }
        }
        for pipe in &mut self.audio {
            pipe.queue = None;
        }
        self.video = None;
        for pipe in &mut self.audio {
            if let Some(thread) = pipe.thread.take() {
                let _ = thread.join();
            }
        }
        if let Some(thread) = self.video_thread.take() {
            let _ = thread.join();
        }
        // The child is this writer's alone from here: no stray kill may
        // land on a pid that was reaped and handed out again.
        self.stop_watchdog();
        let Some(mut child) = self.child.take() else {
            if let Some(why) = self.stream_failure() {
                bail!("{}: no file was written: {why}", self.spec.path.display());
            }
            bail!(
                "{}: finished without a frame ever arriving, so no file was written",
                self.spec.path.display()
            );
        };
        let deadline = Instant::now() + FINISH_WAIT;
        let exited = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::sleep(Duration::from_millis(5));
        };
        self.drain_stderr(STDERR_WAIT);
        if exited.is_some() {
            self.join_progress_reader();
        }
        let tail = self.stderr_tail();
        // A stream thread's own failure is the story the exit status
        // can't tell: its pipe closed early, so the child may still
        // have ended on a file that stopped short.
        let failed = self.stream_failure();
        match exited {
            Some(status) if status.success() && failed.is_none() => {
                isocast_diagnostics::debug!(
                    "writer: finished {} ({} tail frames repeated, {:.1} s of timeline)",
                    self.spec.path.display(),
                    self.repeats,
                    self.start
                        .map_or(0.0, |at| end.saturating_since(at).as_secs_f64())
                );
                // ffmpeg's encoders don't report pictures or audio
                // they discard on their own, so those counts stay 0.
                Ok(Finished {
                    repeated: self.repeats,
                    lost_frames: 0,
                    lost_audio_tracks: 0,
                })
            }
            Some(_) if failed.is_some() => Err(anyhow!(
                "a stream stopped early writing {}: {}; its last words: {tail}",
                self.spec.path.display(),
                failed.expect("checked by the arm's guard")
            )),
            Some(status) => Err(anyhow!(
                "ffmpeg did not finish {} ({status}); its last words: {tail}",
                self.spec.path.display()
            )),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                Err(anyhow!(
                    "ffmpeg did not finish {} within {} s, so it was killed; its last \
                     words: {tail}",
                    self.spec.path.display(),
                    FINISH_WAIT.as_secs()
                ))
            }
        }
    }

    fn cancel(mut self) {
        // The take is abandoned: the process dies, the pipes close, and
        // what it began writing is removed — idempotently, since a
        // child that never started may have made nothing at all. The
        // watchdog goes first (it must never signal a pid this call
        // reaps), then the child dies, so the streams' blocked writes
        // break at once.
        self.stop_watchdog();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        for pipe in &mut self.audio {
            pipe.queue = None;
        }
        self.video = None;
        for pipe in &mut self.audio {
            if let Some(thread) = pipe.thread.take() {
                let _ = thread.join();
            }
        }
        if let Some(thread) = self.video_thread.take() {
            let _ = thread.join();
        }
        self.drain_stderr(STDERR_WAIT);
        self.join_progress_reader();
        if let Err(err) = std::fs::remove_file(&self.spec.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            isocast_diagnostics::diagnostic!(
                "warn"; "could not remove {}: {err}", self.spec.path.display()
            );
        }
    }
}

impl Drop for FfmpegFileWriter {
    /// A writer that was neither finished nor cancelled — a panic on the
    /// thread that held it: the watchdog is stopped, so it can't signal
    /// a pid this writer no longer owns. The child, if any, is left to
    /// the engine's own cleanup (a dropped `Child` isn't killed).
    fn drop(&mut self) {
        self.stop_watchdog();
    }
}

/// The ffmpeg this machine records and captures through:
/// `ISOCAST_FFMPEG` names another one (a full build with prores_ks, an
/// NVENC-enabled one), else `ffmpeg` from the PATH.
pub(crate) fn ffmpeg_binary() -> OsString {
    std::env::var_os("ISOCAST_FFMPEG").unwrap_or_else(|| "ffmpeg".into())
}

/// The encoders this machine's ffmpeg lists, one name per line of
/// `-encoders`' output.
pub(crate) fn listed_encoders(program: &OsString) -> Result<Vec<String>> {
    let listed = Command::new(program)
        .arg("-hide_banner")
        .arg("-encoders")
        .output()
        .with_context(|| {
            format!(
                "the ffmpeg binary ({}) could not be run: is ffmpeg installed, or does \
                 ISOCAST_FFMPEG name one?",
                program.to_string_lossy()
            )
        })?;
    if !listed.status.success() {
        bail!(
            "ffmpeg -encoders failed ({}): {}",
            listed.status,
            String::from_utf8_lossy(&listed.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let flags = fields.next()?;
            let name = fields.next()?;
            // Encoder lines carry a six-character flag field before the
            // name; the headers and the dashed rule have other shapes,
            // and none of their second fields is an encoder's name.
            (flags.len() == 6).then(|| name.to_owned())
        })
        .collect())
}

/// The ffmpeg encoder for `spec`'s codec, and the arguments that carry
/// its quality: the machine's own NVENC first, the software fallback
/// behind it, ProRes through `prores_ks`'s profiles.
pub(crate) fn choose_encoder(
    encoder: &Encoder,
    listed: &[String],
) -> Result<(&'static str, Vec<String>)> {
    let quality = u32::from(encoder.quality.value());
    let (name, args) = match encoder.codec {
        Codec::Hevc => {
            if listed.iter().any(|listed| listed == "hevc_nvenc") {
                ("hevc_nvenc", nvenc_args(quality))
            } else {
                ("libx265", vec!["-crf".into(), crf(quality)])
            }
        }
        Codec::H264 => {
            if listed.iter().any(|listed| listed == "h264_nvenc") {
                ("h264_nvenc", nvenc_args(quality))
            } else {
                (
                    "libx264",
                    vec![
                        "-crf".into(),
                        crf(quality),
                        "-preset".into(),
                        "veryfast".into(),
                    ],
                )
            }
        }
        Codec::Prores422Proxy => ("prores_ks", prores_args(0, quality)),
        Codec::Prores422Lt => ("prores_ks", prores_args(1, quality)),
        Codec::Prores422 => ("prores_ks", prores_args(2, quality)),
        Codec::Prores422Hq => ("prores_ks", prores_args(3, quality)),
        Codec::Prores4444 => ("prores_ks", prores_args(4, quality)),
    };
    if !listed.iter().any(|listed| listed == name) {
        bail!(
            "this machine's ffmpeg lists no {name} encoder, so {:?} can't be recorded \
             here: install an ffmpeg with it, or point ISOCAST_FFMPEG at one",
            encoder.codec
        );
    }
    Ok((name, args))
}

/// What a 10-bit HEVC file needs on top of `choose_encoder`'s arguments:
/// Main 10, with the encoder given 10-bit 4:2:0 (an 8-bit input is
/// widened by ffmpeg). Without it the file would be written as 8-bit
/// Main whatever `bit_depth` said. ProRes is always 10-bit and H.264
/// always 8.
fn depth_args(encoder: &Encoder, name: &str) -> Vec<String> {
    if encoder.codec != Codec::Hevc || encoder.bit_depth != 10 {
        return Vec::new();
    }
    let format = if name == "hevc_nvenc" {
        "p010le"
    } else {
        "yuv420p10le"
    };
    ["-profile:v", "main10", "-pix_fmt", format]
        .map(String::from)
        .into()
}

/// NVENC's constant quality on ffmpeg's 1–51 `-cq` scale, which runs
/// the other way round from the engine's 1–100.
fn nvenc_args(quality: u32) -> Vec<String> {
    let cq = (51 - quality / 2).clamp(1, 51);
    vec![
        "-rc".into(),
        "vbr".into(),
        "-cq".into(),
        cq.to_string(),
        "-preset".into(),
        "p5".into(),
    ]
}

/// x264's and x265's constant rate factor for a 1–100 quality.
fn crf(quality: u32) -> String {
    (51.0 - f64::from(quality) * 51.0 / 100.0)
        .round()
        .to_string()
}

/// `prores_ks`'s profile and its 1–10 quality scale, 10 the best.
fn prores_args(profile: u32, quality: u32) -> Vec<String> {
    let qscale = (10.0 - 9.0 * f64::from(quality) / 100.0)
        .round()
        .clamp(1.0, 10.0);
    vec![
        "-profile:v".into(),
        profile.to_string(),
        "-qscale:v".into(),
        qscale.to_string(),
    ]
}

/// The muxer's name for a container.
pub(crate) fn container_name(container: Container) -> &'static str {
    match container {
        Container::Mov => "mov",
        Container::Mp4 => "mp4",
    }
}

/// `path`'s parent folder exists and takes writes: `create_dir_all`,
/// then a one-byte probe file, removed again. A bare file name
/// records into the working directory, whose folder is this one.
fn probe_folder(path: &Path) -> Result<()> {
    let folder = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(folder)
        .with_context(|| format!("{}: the recording's folder can't be made", folder.display()))?;
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let probe = folder.join(format!(
        ".isocast-write-probe-{}-{unique}",
        std::process::id()
    ));
    let written = std::fs::write(&probe, b"\0").with_context(|| {
        format!(
            "{}: the recording's folder doesn't take writes",
            folder.display()
        )
    });
    let _ = std::fs::remove_file(&probe);
    written?;
    Ok(())
}

/// The frame's planes as the raw stream's bytes: BGRA's one plane as
/// it lies, Y'CbCr's two planes in NV12's order, luma then interleaved
/// chroma.
fn raw_bytes(frame: &LinuxFrame) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for index in 0..plane_count(frame.kind()) {
        let (plane, _stride) = frame.plane(index)?;
        bytes.extend_from_slice(&plane);
    }
    Ok(bytes)
}

/// How many planes a frame kind feeds the raw stream.
fn plane_count(kind: FrameKind) -> usize {
    match kind {
        FrameKind::Bgra => 1,
        FrameKind::Ycbcr8 => 2,
    }
}

/// How many bytes one frame of `kind` at `pixels` feeds the raw
/// stream.
fn raw_len(pixels: (u32, u32), kind: FrameKind) -> usize {
    let (width, height) = (pixels.0 as usize, pixels.1 as usize);
    match kind {
        FrameKind::Bgra => width * height * 4,
        FrameKind::Ycbcr8 => width * height + width * height / 2,
    }
}

/// The samples as ffmpeg's f32le input: 32-bit floats in memory
/// order, which is little-endian on every machine Isocast runs on.
fn f32_bytes(samples: &[f32]) -> &[u8] {
    // SAFETY: `samples` is a live slice of `samples.len()` f32s, so
    // its first `len * 4` bytes are initialized and readable, and
    // u8's alignment (1) divides f32's (4).
    unsafe { std::slice::from_raw_parts(samples.as_ptr().cast::<u8>(), samples.len() * 4) }
}

/// One file's pictures into the child's stdin, from the picture
/// thread, until the writer drops the queue (the take is over, however
/// it ended) or the pipe breaks (the child is gone, which its exit
/// status reports).
fn video_writer(
    stdin: ChildStdin,
    pictures: Receiver<Vec<u8>>,
    progress: Arc<Progress>,
    failure: Arc<Mutex<Option<String>>>,
) {
    let fd = stdin.as_raw_fd();
    while let Ok(bytes) = pictures.recv() {
        if let Err(err) = write_item(fd, &bytes, &progress) {
            if err.kind() != std::io::ErrorKind::BrokenPipe {
                isocast_diagnostics::diagnostic!("error"; "a picture's pipe stopped: {err}");
            }
            record_failure(&failure, &err);
            return;
        }
    }
}

/// One track's samples into its pipe, from the track's own thread,
/// until the writer drops the queue (the take is over, however it
/// ended). Returns once the pipe breaks: the child is gone, which the
/// video path and the exit status report.
fn audio_writer(
    write: OwnedFd,
    samples: Receiver<Vec<u8>>,
    progress: Arc<Progress>,
    failure: Arc<Mutex<Option<String>>>,
) {
    let fd = write.as_raw_fd();
    while let Ok(bytes) = samples.recv() {
        if let Err(err) = write_item(fd, &bytes, &progress) {
            // A broken pipe is the child being gone; anything else
            // would otherwise be lost with the take.
            if err.kind() != std::io::ErrorKind::BrokenPipe {
                isocast_diagnostics::diagnostic!(
                    "error"; "an audio track's pipe stopped: {err}"
                );
            }
            record_failure(&failure, &err);
            return;
        }
    }
}

/// The first thing a stream thread's write failed with, for the writer
/// to report (the child's exit status may say something else).
fn record_failure(slot: &Mutex<Option<String>>, err: &std::io::Error) {
    record_why(slot, &err.to_string());
}

/// The first reason recorded: later ones are consequences of the first.
fn record_why(slot: &Mutex<Option<String>>, why: &str) {
    let mut slot = slot.lock().expect("the writer's failure slot is poisoned");
    if slot.is_none() {
        *slot = Some(why.to_owned());
    }
}

/// How far the pipes' writes have got, for the watchdog: it must not
/// take an idle writer (no frames to write) for a wedged encoder, so
/// it counts the time since the last byte of a write still in flight.
struct Progress {
    base: Instant,
    /// Writes begun and not finished.
    flying: AtomicUsize,
    /// The base clock's reading when the last byte went in.
    last_nanos: AtomicU64,
}

impl Progress {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            flying: AtomicUsize::new(0),
            last_nanos: AtomicU64::new(0),
        }
    }

    /// A write is beginning: nothing of it has gone in yet.
    fn begin(&self) {
        self.flying.fetch_add(1, Ordering::AcqRel);
        self.took();
    }

    /// Some of it went in.
    fn took(&self) {
        self.last_nanos
            .store(self.base.elapsed().as_nanos() as u64, Ordering::Release);
    }

    /// The write is over, whatever the pipe took.
    fn end(&self) {
        self.flying.fetch_sub(1, Ordering::AcqRel);
    }

    /// How long the writes in flight have gone without taking a byte:
    /// zero when none are.
    fn stalled(&self) -> Duration {
        if self.flying.load(Ordering::Acquire) == 0 {
            return Duration::ZERO;
        }
        let last = Duration::from_nanos(self.last_nanos.load(Ordering::Acquire));
        self.base.elapsed().saturating_sub(last)
    }
}

/// Watch the child's pipes: a write that takes nothing at all for
/// [`PIPE_STALL`] is an encoder that will never take it, so it is
/// killed — the blocked writes then fail their file, instead of a take
/// that never ends.
fn start_watchdog(
    pid: u32,
    progress: Arc<Progress>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<thread::JoinHandle<()>> {
    isocast_diagnostics::spawn(
        "encoder watchdog",
        move || {
            loop {
                thread::sleep(WATCHDOG_TICK);
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let stalled = progress.stalled();
                if stalled >= PIPE_STALL {
                    isocast_diagnostics::diagnostic!(
                        "error";
                        "the encoder took nothing for {:.0} s: killing it",
                        stalled.as_secs_f64()
                    );
                    // SAFETY: kill takes a pid and a signal and only
                    // reports whether it worked. The child can't have
                    // been reaped: every reap stops this thread and
                    // joins it first (see `stop_watchdog`).
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                    return;
                }
            }
        },
        |why| isocast_diagnostics::diagnostic!("error"; "the encoder watchdog stopped: {why}"),
    )
}

/// `bytes` into the pipe at `fd`, whole, blocking: each stream has a
/// thread of its own to do the waiting, and the watchdog kills a child
/// that stops taking bytes, so this can't wait forever on one.
fn write_item(fd: RawFd, bytes: &[u8], progress: &Progress) -> std::io::Result<()> {
    progress.begin();
    let result = write_all(fd, bytes, progress);
    progress.end();
    result
}

/// `bytes` into the pipe at `fd`, blocking: this is only called once
/// the pipe proved writable, so it is the encoder draining behind its
/// own pace, and the bytes go in whole or raise the pipe's error.
fn write_all(fd: RawFd, bytes: &[u8], progress: &Progress) -> std::io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        // A signal mid-write (a timer, the pool's own threads) is not a
        // broken pipe: retrying is what `write_all` does, and losing the
        // frame instead would be a drop nobody asked for.
        let took = match write(fd, &bytes[written..]) {
            Ok(took) => took,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if took == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        written += took;
        progress.took();
    }
    Ok(())
}

/// One write into the pipe at `fd`.
fn write(fd: RawFd, bytes: &[u8]) -> std::io::Result<usize> {
    // SAFETY: write reads `bytes` and returns how much of the pipe it
    // took; this program's SIGPIPE is off (the Rust runtime turns it
    // into EPIPE at startup), so a dead reader returns an error
    // instead of killing the process.
    let took = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if took < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(took as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pipe with nothing behind it: what would take the blocks.
    fn pipe(depth: usize) -> (AudioPipe, Receiver<Vec<u8>>) {
        let (queue, blocks) = sync_channel(depth);
        (
            AudioPipe {
                pending: Vec::new(),
                queue: Some(queue),
                thread: None,
                owed: 0,
            },
            blocks,
        )
    }

    /// The f32s in a block of raw sample bytes.
    fn samples_of(bytes: &[u8]) -> Vec<f32> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|four| f32::from_ne_bytes(*four))
            .collect()
    }

    /// A block a full queue refused is owed, and goes in as silence
    /// ahead of the next block that fits: the raw pipe carries no
    /// timestamps, so without that every later sample would move
    /// earlier and the file's sound would drift out of sync with its
    /// video for the rest of the take.
    #[test]
    fn progress_report_lines_give_the_picture_count() {
        assert_eq!(progress_frames("frame=42\n"), Some(42));
        assert_eq!(progress_frames("frame=  7 \n"), Some(7));
        assert_eq!(progress_frames("frame=N/A\n"), None);
        assert_eq!(progress_frames("out_time_us=16667\n"), None);
        assert_eq!(progress_frames("progress=continue\n"), None);
        assert_eq!(progress_frames("\n"), None);
    }

    #[test]
    fn ten_bit_hevc_asks_for_main_10() {
        use isocast_protocol::{EncoderQuality, QualityPreset};
        let encoder = |codec, bit_depth| Encoder {
            codec,
            quality: EncoderQuality::Preset(QualityPreset::High),
            container: Container::Mov,
            bit_depth,
        };
        let ten = depth_args(&encoder(Codec::Hevc, 10), "hevc_nvenc");
        assert_eq!(ten, ["-profile:v", "main10", "-pix_fmt", "p010le"]);
        let soft = depth_args(&encoder(Codec::Hevc, 10), "libx265");
        assert_eq!(soft.last().map(String::as_str), Some("yuv420p10le"));
        assert!(depth_args(&encoder(Codec::Hevc, 8), "hevc_nvenc").is_empty());
        assert!(depth_args(&encoder(Codec::H264, 8), "h264_nvenc").is_empty());
        assert!(depth_args(&encoder(Codec::Prores422, 10), "prores_ks").is_empty());
    }

    #[test]
    fn refused_samples_are_owed_as_silence() {
        let (mut pipe, blocks) = pipe(1);
        assert!(pipe.send_samples(&[1.0, 2.0]), "the first block fits");
        assert!(!pipe.send_samples(&[3.0, 4.0]), "the queue is full");
        assert!(!pipe.send_samples(&[5.0, 6.0]), "still full");
        assert_eq!(pipe.owed, 16, "two stereo f32 blocks are owed");

        assert_eq!(
            samples_of(&blocks.recv().expect("the first block")),
            [1.0, 2.0]
        );
        assert!(pipe.send_samples(&[7.0, 8.0]), "room again");
        let block = samples_of(&blocks.recv().expect("the block with its silence"));
        assert_eq!(block[0..4], [0.0; 4], "the owed silence comes first");
        assert_eq!(block[4..6], [7.0, 8.0]);
        assert_eq!(pipe.owed, 0, "the debt is paid");
    }

    /// A track the child never takes keeps what it owes, so `finish`
    /// can pay it with silence and reach the end its video does.
    #[test]
    fn owed_silence_survives_a_child_that_takes_nothing() {
        let (mut pipe, blocks) = pipe(1);
        assert!(pipe.send_samples(&[1.0]));
        assert!(!pipe.send_samples(&[2.0]));
        drop(blocks);
        assert_eq!(pipe.owed, 4);
        // No queue takes it now: the write failed, and the debt stands
        // (the file is failing anyway, which its exit status reports).
        assert!(!pipe.send_samples(&[]));
        assert_eq!(pipe.owed, 4);
    }

    /// f32's bytes are the samples the engine handed over, native-endian
    /// and in order.
    #[test]
    fn f32_bytes_are_the_samples_themselves() {
        let samples = [1.5f32, -2.25, 0.0];
        assert_eq!(samples_of(f32_bytes(&samples)), samples);
    }
}
