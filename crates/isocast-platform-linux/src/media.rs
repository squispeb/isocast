//! Media files: one `ffmpeg` child per part, decoding the file's video
//! to BGRA frames or its sound to 48 kHz stereo `f32` samples on its
//! stdout — the same process-per-stream shape the camera capture and the
//! writer already use. macOS gets IOSurfaces from AVAssetReader in
//! process; here the decode is a child process and the frame is copied
//! into the engine's own pool (the copy L.5's in-process encoders would
//! remove on the write side; the read side can stay a child, as OBS's
//! media source is).
//!
//! Times come out of the file, not out of a metronome. A stream with a
//! constant rate (almost every recording) plays by its frame index over
//! its rate; one stored frame-by-frame, when the picture changed (a
//! screen or phone capture), takes each frame's own time from ffprobe's
//! decode of the file, so its pictures stay where the file put them and
//! its sound stays with them.
//!
//! Every frame is read into a pool frame and every sound block into a
//! `Vec<f32>`; a pool with nothing free (the compositor holding all 32)
//! drops that frame and says so, rather than holding the file's
//! timeline back.

use std::io::{BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{FramePool, MediaPart, SAMPLE_RATE};

use crate::camera::read_stderr;
use crate::frames::{CpuPool, LinuxFrame};
use crate::system::{ffmpeg_path, ffprobe_path};

/// Samples one audio block carries: 100 ms at the engine's rate, the
/// block the mixer's own chunks are sized in.
const AUDIO_BLOCK: u64 = SAMPLE_RATE as u64 / 10;
/// How often a full pool's dropped frames are reported.
const DROP_REPORT: Duration = Duration::from_secs(1);

/// Decodes one part of one media file, from its start, as fast as asked.
pub struct MediaReader {
    decoder: Decoder,
    /// The pictures, for a reader opened for video.
    video: Option<Video>,
    /// Samples handed over so far: the time of the next block's first
    /// sample.
    written: u64,
    /// The file, for errors.
    shown: String,
    /// What was dropped for a full pool, reported at most every second.
    dropped: Dropped,
}

struct Video {
    pool: CpuPool,
    width: u32,
    height: u32,
    /// The stream's constant frame rate, when it has one.
    rate: Option<f64>,
    /// Every frame's time in seconds, when it hasn't: `rate` is `None`
    /// then, and each frame's time is its own.
    times: Vec<f64>,
    /// Frames handed over: the index into `times`, the multiplier for
    /// `rate`.
    index: usize,
}

/// The ffmpeg child decoding one part of one file.
struct Decoder {
    child: Child,
    out: BufReader<ChildStdout>,
    /// The child's stderr, drained by a thread of its own (a chatty
    /// decoder must not deadlock on a pipe nobody reads) and kept to its
    /// last words.
    tail: Arc<Mutex<Vec<u8>>>,
    stderr_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // A reader dropped part way (a source that went off air, the
        // next file of a playlist) stops decoding now instead of
        // holding a child process until the next loop.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

/// Frames a full pool refused, reported at most every [`DROP_REPORT`].
#[derive(Default)]
struct Dropped {
    since: Option<Instant>,
    count: u64,
}

impl Dropped {
    fn count(&mut self) {
        let since = *self.since.get_or_insert_with(Instant::now);
        self.count += 1;
        let elapsed = since.elapsed();
        if elapsed >= DROP_REPORT {
            isocast_diagnostics::diagnostic!(
                "warn"; "media: dropped {} picture(s) in {:.0} s: every frame in the pool is still \
                downstream",
                self.count,
                elapsed.as_secs_f64()
            );
            self.since = Some(Instant::now());
            self.count = 0;
        }
    }
}

/// Decode `part` of the media file at `path`.
pub fn open_media(path: &Path, part: MediaPart) -> Result<MediaReader> {
    let shown = path.display().to_string();
    // A missing file would otherwise read as one without that part.
    if !path.exists() {
        bail!("{shown} does not exist");
    }
    let info = crate::system::media_info(path)?;
    let mut reader = MediaReader {
        decoder: spawn(path, part)?,
        video: None,
        written: 0,
        shown,
        dropped: Dropped::default(),
    };
    match part {
        MediaPart::Video => {
            let (width, height) = info
                .video
                .ok_or_else(|| anyhow!("{} has no video", reader.shown))?;
            let (rate, times) = video_timing(path, &reader.shown)?;
            reader.video = Some(Video {
                pool: CpuPool::bgra(width, height),
                width,
                height,
                rate,
                times,
                index: 0,
            });
        }
        MediaPart::Audio => {
            if !info.audio {
                bail!("{} has no sound", reader.shown);
            }
        }
    }
    Ok(reader)
}

/// The ffmpeg child that writes `part` of `path` to its stdout.
fn spawn(path: &Path, part: MediaPart) -> Result<Decoder> {
    let mut command = Command::new(ffmpeg_path());
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-i")
        .arg(path);
    match part {
        MediaPart::Video => {
            command
                // The first video stream, as plain BGRA rows: the same
                // pixels a camera's capture lands in, and what the
                // compositor and the writer both take.
                .arg("-map")
                .arg("0:v:0")
                // A frame stored when the picture changed is kept where
                // it is: the caller paces by each frame's own time,
                // rather than ffmpeg duplicating and dropping frames to
                // reach a constant rate.
                .arg("-fps_mode")
                .arg("passthrough")
                .arg("-f")
                .arg("rawvideo")
                .arg("-pix_fmt")
                .arg("bgra");
        }
        MediaPart::Audio => {
            command
                .arg("-map")
                .arg("0:a:0")
                // A gap between the file's audio packets is silence: the
                // block times must stay with the file's timeline, or the
                // sound would drift from the pictures past the gap.
                .arg("-af")
                .arg("aresample=async=1")
                .arg("-ac")
                .arg("2")
                .arg("-ar")
                .arg(SAMPLE_RATE.to_string())
                .arg("-f")
                .arg("f32le");
        }
    }
    command
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("running {} (is ffmpeg installed?)", ffmpeg_path()))?;
    let out = child.stdout.take().expect("ffmpeg's stdout was piped");
    let stderr = child.stderr.take().expect("ffmpeg's stderr was piped");
    let tail = Arc::new(Mutex::new(Vec::new()));
    let stderr_thread = isocast_diagnostics::spawn(
        "media ffmpeg stderr",
        {
            let tail = tail.clone();
            move || read_stderr(stderr, tail)
        },
        |why| isocast_diagnostics::diagnostic!("error"; "the media stderr reader stopped: {why}"),
    )
    .ok();
    Ok(Decoder {
        child,
        out: BufReader::new(out),
        tail,
        stderr_thread,
    })
}

/// The stream's constant frame rate, when it has one, else every frame's
/// own time in seconds from the stream's first.
fn video_timing(path: &Path, shown: &str) -> Result<(Option<f64>, Vec<f64>)> {
    let rates = ffprobe(
        path,
        &[
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=r_frame_rate,avg_frame_rate",
            "-of",
            "csv=p=0",
        ],
    )?;
    let mut fields = rates.trim().lines().next().unwrap_or("").split(',');
    let rate = fields.next().and_then(rate_of);
    let average = fields.next().and_then(rate_of);
    // A stored rate that matches its average is a constant rate: the
    // usual recording, and the cheap path (no second decode).
    if let (Some(rate), Some(average)) = (rate, average)
        && (rate - average).abs() < 1e-6
    {
        return Ok((Some(rate), Vec::new()));
    }
    let listed = ffprobe(
        path,
        &[
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=pts_time",
            "-of",
            "csv=p=0",
        ],
    )
    .with_context(|| format!("reading every frame's time in {shown}"))?;
    let times: Vec<f64> = listed
        .lines()
        .filter_map(|line| line.trim().trim_end_matches(',').parse::<f64>().ok())
        .collect();
    if times.is_empty() {
        // A rate we can't believe and no frame times either: play it as
        // the average rate rather than refusing the file.
        return Ok((average.or(rate), Vec::new()));
    }
    // Times from the stream's own first frame, as the constant-rate path
    // is: the part plays from its start either way.
    let first = times[0];
    Ok((None, times.into_iter().map(|at| at - first).collect()))
}

/// An ffprobe answer for `path` and these arguments.
fn ffprobe(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(ffprobe_path())
        .arg("-v")
        .arg("error")
        .args(args)
        .arg(path)
        .output()
        .with_context(|| format!("running {} (is ffprobe installed?)", ffprobe_path()))?;
    if !output.status.success() {
        bail!(
            "ffprobe could not read {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A frame rate written the way ffprobe writes it (`30000/1001`,
/// `25/1`, `0/0` for "not known").
fn rate_of(field: &str) -> Option<f64> {
    let (top, bottom) = field.trim().split_once('/')?;
    let (top, bottom) = (top.parse::<f64>().ok()?, bottom.parse::<f64>().ok()?);
    (top > 0.0 && bottom > 0.0).then(|| top / bottom)
}

impl Decoder {
    /// The child has written everything there is: say so, unless it
    /// failed, whose last words are the reason. (`None` from a read
    /// means the file ended; a decoder that died mid-file ends the pipe
    /// the same way, so its exit is what tells them apart.)
    fn ended(&mut self) -> Result<()> {
        let status = self
            .child
            .wait()
            .context("waiting for ffmpeg to finish the file")?;
        if status.success() {
            return Ok(());
        }
        let tail = self.tail.lock().expect("the media stderr tail is poisoned");
        bail!(
            "ffmpeg stopped reading the file: {status}; its last words: {}",
            String::from_utf8_lossy(&tail).trim()
        );
    }
}

/// A frame's time from the part's first frame: its index over a constant
/// rate, or its own time when the file stores frames as they changed.
fn frame_time(video: &Video, index: usize) -> Duration {
    let seconds = match video.rate {
        Some(rate) => index as f64 / rate,
        None => video.times.get(index).copied().unwrap_or_else(|| {
            // A frame ffprobe didn't list (a stream whose count it got
            // wrong): one frame past the last time it did.
            video.times.last().copied().unwrap_or(0.0) + 1.0 / 30.0
        }),
    };
    Duration::from_secs_f64(seconds.max(0.0))
}

/// One BGRA frame from `read` into `pixels`, whose rows are `stride`
/// bytes apart (the pool's own rows, which may be padded). `Ok(false)`
/// is the end of the stream, between frames; a stream that ends in the
/// middle of a frame is a failure, not a shorter file.
fn read_frame(
    read: &mut impl Read,
    pixels: &mut [u8],
    stride: usize,
    width: u32,
    height: u32,
) -> std::io::Result<bool> {
    let row = width as usize * 4;
    let height = height as usize;
    let needed = height.saturating_sub(1) * stride + row;
    if pixels.len() < needed {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "a {width}x{height} BGRA picture at stride {stride} needs {needed} bytes and the \
                 frame's plane holds {}",
                pixels.len()
            ),
        ));
    }
    let mut started = false;
    for y in 0..height {
        let at = y * stride;
        let mut filled = 0;
        while filled < row {
            let read_now = read.read(&mut pixels[at + filled..at + row])?;
            if read_now == 0 {
                if started {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "the file ends in the middle of a {width}x{height} picture \
                             ({y} of {height} rows read)"
                        ),
                    ));
                }
                return Ok(false);
            }
            started = true;
            filled += read_now;
        }
    }
    Ok(true)
}

impl isocast_engine::MediaReader for MediaReader {
    type Frame = LinuxFrame;

    fn next_video(&mut self) -> Result<Option<(LinuxFrame, Duration)>> {
        if self.video.is_none() {
            bail!("{} was opened for its sound, not its pictures", self.shown);
        }
        loop {
            let video = self.video.as_mut().expect("checked just above");
            let (width, height) = (video.width, video.height);
            let stride = width as usize * 4;
            let out = &mut self.decoder.out;
            let mut read_error = None;
            let taken = video.pool.draw(&mut |pixels, stride| {
                match read_frame(out, pixels, stride, width, height) {
                    Ok(true) => {}
                    // The end of the file, read as no frame at all: the
                    // pool's slot goes back untouched.
                    Ok(false) => read_error = Some(None),
                    Err(err) => read_error = Some(Some(err)),
                }
            })?;
            match read_error {
                Some(None) => {
                    self.decoder.ended()?;
                    return Ok(None);
                }
                Some(Some(err)) => return Err(err).context("reading the file's pictures"),
                None => {}
            }
            let index = video.index;
            video.index += 1;
            let at = frame_time(video, index);
            let Some(frame) = taken else {
                // Every frame of the pool is downstream: this picture is
                // read past and skipped — one frame, not the rest of the
                // file — and the ones after it keep their own times, so
                // the file's sound stays with its pictures.
                self.dropped.count();
                let mut scratch = vec![0u8; stride * height as usize];
                match read_frame(out, &mut scratch, stride, width, height) {
                    Ok(true) => continue,
                    // The skipped frame was the file's last.
                    Ok(false) => {
                        self.decoder.ended()?;
                        return Ok(None);
                    }
                    Err(err) => return Err(err).context("reading the file's pictures"),
                }
            };
            return Ok(Some((frame, at)));
        }
    }

    fn next_audio(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        if self.video.is_some() {
            bail!("{} was opened for its pictures, not its sound", self.shown);
        }
        let bytes = AUDIO_BLOCK as usize * 2 * std::mem::size_of::<f32>();
        let mut buffer = vec![0u8; bytes];
        let mut filled = 0;
        while filled < buffer.len() {
            let read = self
                .decoder
                .out
                .read(&mut buffer[filled..])
                .context("reading the file's sound")?;
            if read == 0 {
                if filled == 0 {
                    self.decoder.ended()?;
                    return Ok(None);
                }
                break;
            }
            filled += read;
        }
        let samples: Vec<f32> = buffer[..filled / 4 * 4]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|four| f32::from_le_bytes(*four))
            .collect();
        let at = Duration::from_secs_f64(self.written as f64 / f64::from(SAMPLE_RATE));
        self.written += (samples.len() / 2) as u64;
        Ok(Some((samples, at)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `30000/1001` and `25/1` are rates; ffprobe's "not known" is not.
    #[test]
    fn ffprobe_rates_are_read_or_refused() {
        assert_eq!(rate_of("30000/1001"), Some(30000.0 / 1001.0));
        assert_eq!(rate_of("25/1"), Some(25.0));
        assert_eq!(rate_of(" 60/1 "), Some(60.0));
        assert_eq!(rate_of("0/0"), None);
        assert_eq!(rate_of("N/A"), None);
        assert_eq!(rate_of(""), None);
    }

    /// A stream that ends between frames ends the file; one that ends in
    /// the middle of a frame is a failure (a decoder that died), not a
    /// shorter file.
    #[test]
    fn a_frame_is_whole_or_the_stream_ends() {
        let frame = [1u8; 8]; // 2x1 BGRA
        let mut read: &[u8] = &frame;
        assert!(matches!(
            read_frame(&mut read, &mut [0u8; 8], 8, 2, 1),
            Ok(true)
        ));
        assert!(matches!(
            read_frame(&mut read, &mut [0u8; 8], 8, 2, 1),
            Ok(false)
        ));

        let mut read: &[u8] = &frame[..5];
        assert!(read_frame(&mut read, &mut [0u8; 8], 8, 2, 1).is_err());
    }
}
