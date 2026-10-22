//! Joins the packet copy in join.rs can't make: the editor's exports
//! (L.S1a). A clip cut at an in point, a cropped clip, an angle shown with
//! another file's sound, an edited file at its own picture size (the
//! vertical one), or parts in different formats.
//!
//! macOS re-encodes only what it must (the frames from a cut to the next
//! keyframe, and whole parts that are cropped or in another format) and
//! copies the rest. Here the whole join is decoded and encoded again in one
//! ffmpeg run instead: copied and re-encoded packets would have to share
//! one sample description, which this machine's encoders don't promise.
//! Every part comes out frame-exact on the joined file's frame grid:
//!
//! - its picture from its in point for its clip's length (a file that
//!   started later in the take holds its first frame until it starts, one
//!   that ends early holds its last), cropped if the clip is, and fitted
//!   into the joined size (black around a picture of another shape);
//! - each audio track from the same moment (or from another file of the
//!   take, the program's sound under an angle), padded with silence or
//!   cut to the part's exact length, so every join lines up.
//!
//! The joined file takes the format file's codec (the program's, for an
//! edited file) through the same encoder choice as recordings, at the
//! "very high" quality, so a joined file is no worse than what it was
//! made from beyond one more generation.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_engine::{JoinPart, JoinSpec, Joined, JoinedPart, OnJoinProgress, SAMPLE_RATE};
use isocast_protocol::timeline::Crop;
use isocast_protocol::{Codec, Encoder, EncoderQuality};

use crate::writer;

/// The quality joined files are encoded at ("very high").
const QUALITY: u8 = 85;

/// What ffprobe says about one file.
#[derive(Debug, Clone)]
struct Probed {
    codec: String,
    profile: String,
    width: u32,
    height: u32,
    /// Frames per second, as recorded.
    fps: f64,
    pix_fmt: String,
    /// The video's length in seconds.
    duration: f64,
    /// Each audio track's channel count and layout, in order.
    audio: Vec<(u32, String)>,
}

impl Probed {
    /// The same codec, size and frame rate: joinable by copying.
    fn same_format(&self, other: &Probed) -> bool {
        self.codec == other.codec
            && self.profile == other.profile
            && (self.width, self.height) == (other.width, other.height)
            && (self.fps - other.fps).abs() < 0.01
    }
}

fn probe(path: &Path) -> Result<Probed> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
        ])
        .arg(path)
        .output()
        .context("ffprobe could not be run: is ffmpeg installed?")?;
    if !output.status.success() {
        bail!(
            "{} could not be read: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("ffprobe's answer is not JSON")?;
    let streams = json["streams"].as_array().cloned().unwrap_or_default();
    let video = streams
        .iter()
        .find(|stream| stream["codec_type"] == "video")
        .with_context(|| format!("{} has no video", path.display()))?;
    let rate = |key: &str| {
        let text = video[key].as_str().unwrap_or("0/1");
        let (num, den) = text.split_once('/').unwrap_or((text, "1"));
        num.parse::<f64>().unwrap_or(0.0) / den.parse::<f64>().unwrap_or(1.0).max(1e-9)
    };
    let fps = match rate("avg_frame_rate") {
        fps if fps > 0.0 => fps,
        _ => rate("r_frame_rate"),
    };
    let seconds = |value: &serde_json::Value| value.as_str().and_then(|s| s.parse::<f64>().ok());
    let duration = seconds(&video["duration"])
        .or_else(|| seconds(&json["format"]["duration"]))
        .unwrap_or(0.0);
    let audio = streams
        .iter()
        .filter(|stream| stream["codec_type"] == "audio")
        .map(|stream| {
            let channels = stream["channels"].as_u64().unwrap_or(2) as u32;
            let layout = stream["channel_layout"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| if channels == 1 { "mono" } else { "stereo" }.into());
            (channels, layout)
        })
        .collect();
    Ok(Probed {
        codec: video["codec_name"].as_str().unwrap_or_default().to_string(),
        profile: video["profile"].as_str().unwrap_or_default().to_string(),
        width: video["width"].as_u64().unwrap_or(0) as u32,
        height: video["height"].as_u64().unwrap_or(0) as u32,
        fps,
        pix_fmt: video["pix_fmt"].as_str().unwrap_or("yuv420p").to_string(),
        duration,
        audio,
    })
}

/// Whether `spec` needs this module: an edit the copy can't make, or
/// parts in another format than the joined file's.
pub(crate) fn needed(spec: &JoinSpec) -> Result<bool> {
    if spec.size.is_some() {
        return Ok(true);
    }
    for part in &spec.parts {
        if part.in_seconds > 0.0
            || part.crop.is_some_and(|crop| !crop.is_full())
            || part
                .audio
                .as_ref()
                .is_some_and(|(audio, _)| audio != &part.path)
        {
            return Ok(true);
        }
    }
    let format = probe(spec.format.as_deref().unwrap_or(&spec.parts[0].path))?;
    for part in &spec.parts {
        if !probe(&part.path)?.same_format(&format) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// One part, as the graph takes it.
struct Planned<'a> {
    part: &'a JoinPart,
    probed: Probed,
    /// The input index of its picture's file and of its sound's.
    video_input: usize,
    audio_input: usize,
    /// Where its sound's file starts within the clip, and its tracks.
    audio_offset: f64,
    audio_tracks: usize,
    /// Its length in the joined file, in frames of the joined rate.
    frames: u64,
}

/// Join `spec` into `output` (the partial file), decoding and encoding
/// every part again.
pub(crate) fn write(spec: &JoinSpec, output: &Path, progress: OnJoinProgress) -> Result<Joined> {
    let format_path = spec.format.as_deref().unwrap_or(&spec.parts[0].path);
    let format = probe(format_path)?;
    let fps = format.fps.round().max(1.0) as u32;
    let (width, height) = spec.size.unwrap_or((format.width, format.height));
    if width == 0 || height == 0 {
        bail!("{} has no picture size", format_path.display());
    }

    // The inputs: each file once, in the order first needed.
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut input = |path: &Path| match inputs.iter().position(|known| known == path) {
        Some(index) => index,
        None => {
            inputs.push(path.to_path_buf());
            inputs.len() - 1
        }
    };
    let mut planned = Vec::new();
    for part in &spec.parts {
        let probed = probe(&part.path)?;
        let (audio_path, audio_offset) = match &part.audio {
            Some((path, offset)) => (path.clone(), *offset),
            None => (part.path.clone(), part.offset_seconds),
        };
        let audio_tracks = if audio_path == part.path {
            probed.audio.len()
        } else {
            probe(&audio_path)?.audio.len()
        };
        let frames = match part.length {
            Some((frames, rate)) => {
                (frames as f64 * f64::from(fps) / f64::from(rate.max(1))).round() as u64
            }
            None => {
                let shown = probed.duration - (part.in_seconds - part.offset_seconds);
                (shown.max(0.0) * f64::from(fps)).round() as u64
            }
        };
        planned.push(Planned {
            part,
            video_input: input(&part.path),
            audio_input: input(&audio_path),
            audio_offset,
            audio_tracks,
            frames,
            probed,
        });
    }
    let tracks = planned[0].audio_tracks;
    for (k, plan) in planned.iter().enumerate() {
        if plan.audio_tracks != tracks {
            bail!(
                "parts[{k}]: {} has {} audio track(s), where the first part has {tracks}",
                plan.part.path.display(),
                plan.audio_tracks
            );
        }
    }
    let layouts: Vec<(u32, String)> = match tracks {
        0 => Vec::new(),
        _ => {
            let first = &planned[0];
            let path = match &first.part.audio {
                Some((path, _)) => path.clone(),
                None => first.part.path.clone(),
            };
            probe(&path)?.audio
        }
    };

    let graph = graph(&planned, &layouts, fps, (width, height), &format.pix_fmt);
    let listed = writer::listed_encoders(&writer::ffmpeg_binary())?;
    let encoder = Encoder {
        codec: codec_of(&format),
        quality: EncoderQuality::Value(QUALITY),
        container: spec.container,
        bit_depth: if format.pix_fmt.contains("10") { 10 } else { 8 },
    };
    let (name, args) = writer::choose_encoder(&encoder, &listed)?;

    let mut command = Command::new(writer::ffmpeg_binary());
    command.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-nostats"]);
    command.args(["-progress", "pipe:1"]);
    for path in &inputs {
        command.arg("-i").arg(path);
    }
    command.arg("-filter_complex").arg(&graph);
    command.args(["-map", "[v]"]);
    for t in 0..tracks {
        command.arg("-map").arg(format!("[a{t}]"));
    }
    command.args(["-c:v", name]).args(&args);
    command.args(["-pix_fmt", &format.pix_fmt]);
    command.args(["-g", &(fps * 2).to_string(), "-bf", "0"]);
    command.args([
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
    ]);
    for (t, (channels, _)) in layouts.iter().enumerate() {
        let kbps = if *channels <= 2 { 128 } else { 64 * channels };
        command
            .arg(format!("-c:a:{t}"))
            .arg("aac")
            .arg(format!("-b:a:{t}"))
            .arg(format!("{kbps}k"));
    }
    command.args(["-f", writer::container_name(spec.container)]);
    command.arg(output);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().context(
        "ffmpeg could not be run: is ffmpeg installed, or does ISOCAST_FFMPEG name one?",
    )?;

    // ffmpeg's progress lines, read on a thread of their own so the wait
    // below can also ask the caller whether to carry on.
    let (said, heard) = mpsc::channel();
    let stdout = child.stdout.take().expect("piped above");
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(us) = line.strip_prefix("out_time_us=")
                && let Ok(us) = us.trim().parse::<i64>()
            {
                let _ = said.send(us);
            }
        }
    });
    let stderr = child.stderr.take().expect("piped above");
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut text);
        text
    });
    let total_frames: u64 = planned.iter().map(|plan| plan.frames).sum();
    let total = (total_frames as f64 / f64::from(fps)).max(1e-9);
    progress(0.0);
    let status = loop {
        match heard.recv_timeout(Duration::from_millis(200)) {
            Ok(us) => {
                if !progress((us as f64 / 1e6 / total).clamp(0.0, 0.999)) {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("the join was stopped");
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !progress_alive(&mut child)? {
                    break child.wait()?;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break child.wait()?,
        }
    };
    let said = errors.join().unwrap_or_default();
    if !status.success() {
        bail!(
            "ffmpeg could not join the parts ({status}): {}",
            said.trim()
        );
    }
    progress(1.0);

    let mut start = 0u64;
    let parts = planned
        .iter()
        .map(|plan| {
            let available = ((plan.probed.duration
                - (plan.part.in_seconds - plan.part.offset_seconds).max(0.0))
                * f64::from(fps))
            .round()
            .max(0.0) as u64;
            let joined = JoinedPart {
                start_seconds: start as f64 / f64::from(fps),
                duration_seconds: plan.frames as f64 / f64::from(fps),
                frames: plan.frames,
                frames_left_out: available.saturating_sub(plan.frames),
                reencoded: plan.frames,
                converted: !plan.probed.same_format(&format)
                    || plan.part.crop.is_some_and(|crop| !crop.is_full()),
            };
            start += plan.frames;
            joined
        })
        .collect();
    Ok(Joined {
        duration_seconds: total_frames as f64 / f64::from(fps),
        frames: total_frames,
        parts,
    })
}

/// Whether ffmpeg is still running.
fn progress_alive(child: &mut std::process::Child) -> Result<bool> {
    Ok(child.try_wait()?.is_none())
}

/// The recording codec a probed file is in.
fn codec_of(probed: &Probed) -> Codec {
    match probed.codec.as_str() {
        "h264" => Codec::H264,
        "prores" => match probed.profile.to_lowercase().as_str() {
            profile if profile.contains("proxy") => Codec::Prores422Proxy,
            profile if profile.contains("lt") => Codec::Prores422Lt,
            profile if profile.contains("hq") => Codec::Prores422Hq,
            profile if profile.contains("4444") => Codec::Prores4444,
            _ => Codec::Prores422,
        },
        _ => Codec::Hevc,
    }
}

/// The filter graph: each part's picture and tracks made exactly its
/// length, then all of them concatenated. Outputs `[v]` and `[a<t>]`.
fn graph(
    planned: &[Planned<'_>],
    layouts: &[(u32, String)],
    fps: u32,
    (width, height): (u32, u32),
    pix_fmt: &str,
) -> String {
    let mut chains = Vec::new();
    let mut concat = String::new();
    let samples_per_frame = f64::from(SAMPLE_RATE) / f64::from(fps);
    for (k, plan) in planned.iter().enumerate() {
        let part = plan.part;
        let length = plan.frames as f64 / f64::from(fps);
        // Where the part starts in its file: before the file's start, its
        // first frame is held.
        let start = part.in_seconds - part.offset_seconds;
        // On the frame grid first, then cut by frame: the picture at the
        // in point is the frame on screen at that moment, whatever jitter
        // the capture left in the file's own timestamps (a cut by time
        // lost a frame stamped a millisecond before it).
        let mut video = format!("[{}:v:0]fps={fps}", plan.video_input);
        if start >= 0.0 {
            video += &format!(
                ",trim=start_frame={},setpts=PTS-STARTPTS",
                (start * f64::from(fps)).round() as u64
            );
        } else {
            video += &format!(
                ",setpts=PTS-STARTPTS,tpad=start_mode=clone:start_duration={:.6}",
                -start
            );
        }
        // Held past its end too, then cut to its length on the grid.
        video += &format!(
            ",tpad=stop_mode=clone:stop_duration={:.6},fps={fps},trim=end_frame={},setpts=PTS-STARTPTS",
            length + 1.0,
            plan.frames
        );
        if let Some(crop) = part.crop.filter(|crop| !crop.is_full()) {
            video += &crop_filter(crop);
        }
        video += &format!(
            ",scale={width}:{height}:force_original_aspect_ratio=decrease:flags=bicubic,\
             pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,format={pix_fmt}[v{k}]"
        );
        chains.push(video);
        concat += &format!("[v{k}]");

        let samples = (plan.frames as f64 * samples_per_frame).round() as u64;
        let audio_start = part.in_seconds - plan.audio_offset;
        for (t, (_, layout)) in layouts.iter().enumerate() {
            let mut audio = format!(
                "[{}:a:{t}]atrim=start={:.6},asetpts=PTS-STARTPTS,aresample={SAMPLE_RATE}",
                plan.audio_input,
                audio_start.max(0.0)
            );
            if audio_start < 0.0 {
                let delay = (-audio_start * f64::from(SAMPLE_RATE)).round() as u64;
                audio += &format!(",adelay=delays={delay}S:all=1");
            }
            audio += &format!(
                ",apad,atrim=end_sample={samples},asetpts=PTS-STARTPTS,\
                 aformat=sample_fmts=fltp:sample_rates={SAMPLE_RATE}:channel_layouts={layout}[a{k}_{t}]"
            );
            chains.push(audio);
            concat += &format!("[a{k}_{t}]");
        }
    }
    let outputs: String = (0..layouts.len()).map(|t| format!("[a{t}]")).collect();
    chains.push(format!(
        "{concat}concat=n={}:v=1:a={}[v]{outputs}",
        planned.len(),
        layouts.len()
    ));
    chains.join(";")
}

/// ffmpeg's crop of `crop`'s fractions of the picture, on even pixels
/// (4:2:0 pictures can't be cropped to odd sizes).
fn crop_filter(crop: Crop) -> String {
    format!(
        ",crop=w=trunc(iw*{w:.6}/2)*2:h=trunc(ih*{h:.6}/2)*2:x=trunc(iw*{x:.6}/2)*2:y=trunc(ih*{y:.6}/2)*2",
        w = crop.width,
        h = crop.height,
        x = crop.x,
        y = crop.y
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(path: &str, in_seconds: f64, crop: Option<Crop>) -> JoinPart {
        JoinPart {
            path: PathBuf::from(path),
            length: Some((30, 30)),
            offset_seconds: 0.0,
            in_seconds,
            audio: None,
            fps: 30,
            crop,
        }
    }

    fn probed() -> Probed {
        Probed {
            codec: "hevc".into(),
            profile: "Main".into(),
            width: 1280,
            height: 720,
            fps: 30.0,
            pix_fmt: "yuv420p".into(),
            duration: 3.0,
            audio: vec![(2, "stereo".into())],
        }
    }

    /// A part cut at 2 s and cropped: its picture from 2 s, exactly 30
    /// frames, the crop on even pixels, then fitted and concatenated with
    /// its sound cut to exactly 30 frames' worth of samples.
    #[test]
    fn the_graph_cuts_crops_and_lines_up_each_part() {
        let crop = Crop {
            x: 0.25,
            y: 0.0,
            width: 0.5,
            height: 1.0,
        };
        let parts = [part("a.mov", 0.0, None), part("a.mov", 2.0, Some(crop))];
        let planned: Vec<Planned> = parts
            .iter()
            .map(|part| Planned {
                part,
                probed: probed(),
                video_input: 0,
                audio_input: 0,
                audio_offset: 0.0,
                audio_tracks: 1,
                frames: 30,
            })
            .collect();
        let graph = graph(
            &planned,
            &[(2, "stereo".into())],
            30,
            (1280, 720),
            "yuv420p",
        );
        assert!(
            graph.contains("[0:v:0]fps=30,trim=start_frame=60"),
            "{graph}"
        );
        assert!(graph.contains("trim=end_frame=30"), "{graph}");
        assert!(graph.contains("crop=w=trunc(iw*0.500000/2)*2"), "{graph}");
        assert!(graph.contains("atrim=end_sample=48000"), "{graph}");
        assert!(
            graph.ends_with("[v0][a0_0][v1][a1_0]concat=n=2:v=1:a=1[v][a0]"),
            "{graph}"
        );
        // The uncropped part has no crop.
        let first = graph.split(';').next().unwrap();
        assert!(!first.contains("crop="), "{first}");
    }

    /// A file that started 0.5 s into its clip holds its first frame (and
    /// waits in silence) for that long.
    #[test]
    fn a_late_file_is_held_before_it_starts() {
        let mut late = part("b.mov", 0.0, None);
        late.offset_seconds = 0.5;
        let planned = [Planned {
            part: &late,
            probed: probed(),
            video_input: 0,
            audio_input: 0,
            audio_offset: 0.5,
            audio_tracks: 1,
            frames: 30,
        }];
        let graph = graph(
            &planned,
            &[(2, "stereo".into())],
            30,
            (1280, 720),
            "yuv420p",
        );
        assert!(
            graph.contains("tpad=start_mode=clone:start_duration=0.500000"),
            "{graph}"
        );
        assert!(graph.contains("adelay=delays=24000S:all=1"), "{graph}");
    }
}
