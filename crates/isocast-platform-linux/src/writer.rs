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

use std::ffi::OsString;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{FileWriter, SAMPLE_RATE, Time, WriterSpec};
use isocast_protocol::{Codec, Container, Encoder};

use crate::frames::{FrameKind, LinuxFrame};

/// How long a full pipe is retried before the frame it holds is
/// dropped whole: long enough for an encoder's hiccup, short enough
/// that a stalled one drops instead of stalling the take.
const PIPE_WAIT: Duration = Duration::from_millis(50);
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
    stdin: Option<ChildStdin>,
    /// One pipe per audio track: the samples that arrived before the
    /// child, then the child's write end.
    audio: Vec<AudioPipe>,
    /// The child's stderr, drained by its own thread (a chatty encoder
    /// must not deadlock on a pipe nobody reads) and kept to its last
    /// words.
    stderr: Arc<Mutex<Vec<u8>>>,
    reader: Option<thread::JoinHandle<()>>,
    /// The last picture written: its time and its bytes, what a later
    /// frame's gaps repeat and the tail repeats at Stop.
    last_pts: Option<Time>,
    last_bytes: Option<Vec<u8>>,
    /// Pictures written again to cover missing ticks.
    repeats: u64,
    /// The encoder ended early: appends say dropped instead of raising
    /// the same dead-child error again.
    broken: bool,
    /// The raw stream's frame size, fixed where the child started.
    pixels: Option<(u32, u32)>,
}

/// One audio track's pipe: the samples that arrived before the child
/// started (buffered with no cap — the engine drains its own
/// pre-start buffer the same way at the first frame), then the write
/// end of the pipe itself.
struct AudioPipe {
    pending: Vec<f32>,
    write: Option<OwnedFd>,
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
        let (encoder, encoder_args) = choose_encoder(&spec.video.encoder, &listed)?;
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
                    write: None,
                })
                .collect(),
            encoder: encoder.to_owned(),
            encoder_args,
            start: None,
            child: None,
            stdin: None,
            stderr: Arc::new(Mutex::new(Vec::new())),
            reader: None,
            last_pts: None,
            last_bytes: None,
            repeats: 0,
            broken: false,
            pixels: None,
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
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // Nothing it says goes anywhere but stderr, and nothing it
        // reads comes from anywhere but the pipes; nothing is ever
        // overwritten.
        command.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-n"]);
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
        if pixels != (out_width, out_height) {
            command.arg("-vf").arg(format!(
                "scale={out_width}:{out_height}:force_original_aspect_ratio=decrease,\
                 pad={out_width}:{out_height}:(ow-iw)/2:(oh-ih)/2"
            ));
        }
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
        // Fragmented muxing: a killed process keeps what it stored.
        command
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
        self.stdin = child.stdin.take();
        if let Some(stderr) = stderr {
            self.start_stderr_reader(stderr);
        }
        for (pipe, write) in self.audio.iter_mut().zip(writes) {
            pipe.write = Some(write);
        }
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
            let bytes = f32_bytes(&samples);
            let _ = self.write_audio_bytes(track, bytes)?;
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

    /// The samples' bytes into `track`'s pipe under the stdin's rules:
    /// all or nothing, and one error at the encoder's death.
    fn write_audio_bytes(&mut self, track: usize, bytes: &[u8]) -> Result<bool> {
        let Some(fd) = self.audio[track]
            .write
            .as_ref()
            .map(|write| write.as_raw_fd())
        else {
            return Ok(false);
        };
        match wait_writable(fd) {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(err) => return Err(self.pipe_died(err)),
        }
        write_all(fd, bytes).map_err(|err| self.pipe_died(err))?;
        Ok(true)
    }

    /// A picture's bytes into the child's stdin: `Err` once when the
    /// encoder died, `Ok(false)` when its pipe stayed full past
    /// [`PIPE_WAIT`] and the picture was dropped whole.
    fn write_video_bytes(&mut self, bytes: &[u8]) -> Result<bool> {
        let Some(fd) = self.stdin.as_ref().map(|stdin| stdin.as_raw_fd()) else {
            return Ok(false);
        };
        match wait_writable(fd) {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(err) => return Err(self.pipe_died(err)),
        }
        write_all(fd, bytes).map_err(|err| self.pipe_died(err))?;
        Ok(true)
    }

    /// The encoder process ended: latch the file (every later append
    /// says dropped, so the engine counts frames, not repeated
    /// errors), take its stderr's last words, and say why it went.
    fn pipe_died(&mut self, err: std::io::Error) -> anyhow::Error {
        self.broken = true;
        // The child may still be in its last breaths; its stderr holds
        // the reason, and the reader ends at its end.
        if let Some(child) = self.child.as_mut() {
            let _ = child.try_wait();
        }
        self.drain_stderr();
        let tail = self.stderr_tail();
        anyhow!(
            "the ffmpeg encoder ended early writing {}: {err}; its last words: {tail}",
            self.spec.path.display()
        )
    }

    /// Wait for the stderr reader to reach the child's end, which it
    /// can only be a moment behind: a broken pipe means the child's
    /// fds are gone, and a finished child is gone or waited for.
    fn drain_stderr(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
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
        let reader = isocast_diagnostics::spawn(
            "ffmpeg stderr",
            move || {
                let mut chunk = [0u8; 4096];
                loop {
                    let read = match stderr.read(&mut chunk) {
                        Ok(0) => return,
                        Ok(read) => read,
                        Err(_) => return,
                    };
                    let mut tail = tail.lock().expect("ffmpeg stderr tail poisoned");
                    tail.extend_from_slice(&chunk[..read]);
                    let excess = tail.len().saturating_sub(STDERR_TAIL);
                    if excess > 0 {
                        tail.drain(..excess);
                    }
                }
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

impl FileWriter for FfmpegFileWriter {
    type Frame = LinuxFrame;

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
        // the file's timeline: the previous picture is written again
        // for the missing ticks, keeping every picture that did arrive
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
                    if !self.write_video_bytes(&again)? {
                        break;
                    }
                    written_again += 1;
                }
                if written_again > 0 {
                    self.last_pts = Some(last.add_ticks(written_again, fps));
                    self.repeats += written_again;
                }
                if written_again < missing {
                    // The encoder stayed full past the wait: this frame
                    // is dropped whole rather than half of it going in
                    // (half a frame would shift every following one).
                    return Ok(false);
                }
            }
        }
        if !self.write_video_bytes(&bytes)? {
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
        let bytes = f32_bytes(samples);
        self.write_audio_bytes(track, bytes)
    }

    fn finish(mut self, end: Time) -> Result<u64> {
        // Cover the tail: the last picture that arrived may be frames
        // short of `end`, and the file should play up to its end.
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
                    match self.write_video_bytes(&again) {
                        Ok(true) => self.repeats += 1,
                        // The tail is padding: an ended encoder is
                        // reported by its exit status below, and a full
                        // one at Stop leaves the file short of its end.
                        _ => break,
                    }
                }
            }
        }
        // EOF on every pipe: the child flushes and writes its trailer,
        // and nothing kills it early — what it stored must stay.
        self.stdin = None;
        for pipe in &mut self.audio {
            pipe.write = None;
        }
        let Some(mut child) = self.child.take() else {
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
        self.drain_stderr();
        let tail = self.stderr_tail();
        match exited {
            Some(status) if status.success() => {
                isocast_diagnostics::debug!(
                    "writer: finished {} ({} tail frames repeated, {:.1} s of timeline)",
                    self.spec.path.display(),
                    self.repeats,
                    self.start
                        .map_or(0.0, |at| end.saturating_since(at).as_secs_f64())
                );
                Ok(self.repeats)
            }
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
        // child that never started may have made nothing at all.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        for pipe in &mut self.audio {
            pipe.write = None;
        }
        self.drain_stderr();
        if let Err(err) = std::fs::remove_file(&self.spec.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            isocast_diagnostics::diagnostic!(
                "warn"; "could not remove {}: {err}", self.spec.path.display()
            );
        }
    }
}

/// The ffmpeg this machine records through: `ISOCAST_FFMPEG` names
/// another one (a full build with prores_ks, an NVENC-enabled one),
/// else `ffmpeg` from the PATH.
fn ffmpeg_binary() -> OsString {
    std::env::var_os("ISOCAST_FFMPEG").unwrap_or_else(|| "ffmpeg".into())
}

/// The encoders this machine's ffmpeg lists, one name per line of
/// `-encoders`' output.
fn listed_encoders(program: &OsString) -> Result<Vec<String>> {
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
fn choose_encoder(encoder: &Encoder, listed: &[String]) -> Result<(&'static str, Vec<String>)> {
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
fn container_name(container: Container) -> &'static str {
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

/// Whether the pipe at `fd` takes a write within [`PIPE_WAIT`]: a
/// full pipe gets a brief bounded retry, then the caller drops the
/// frame whole. A pipe that broke reports ERR or HUP instead of OUT,
/// and counts as ready — the write that follows raises its EPIPE.
fn wait_writable(fd: RawFd) -> std::io::Result<bool> {
    let deadline = Instant::now() + PIPE_WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut wanted = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: poll only writes its findings into `wanted`'s
        // revents; the timeout is what is left of the whole budget, and
        // an interrupted wait repeats against the same deadline.
        let ready = unsafe {
            libc::poll(
                &mut wanted,
                1,
                remaining.as_millis().min(i32::MAX as u128) as libc::c_int,
            )
        };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        if ready > 0 && wanted.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
    }
}

/// `bytes` into the pipe at `fd`, blocking: this is only called once
/// the pipe proved writable, so it is the encoder draining behind its
/// own pace, and the bytes go in whole or raise the pipe's error.
fn write_all(fd: RawFd, bytes: &[u8]) -> std::io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        let took = write(fd, &bytes[written..])?;
        if took == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        written += took;
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
