//! Joining files end to end (a timeline's export) with the system's
//! libavformat in-process: the video packets are copied as they are (no
//! decoding or encoding), retimed to follow each other; each audio
//! track is decoded and encoded again (AAC, as recordings are) as one
//! continuous track, cut or padded with silence to each part's length,
//! so it lines up with the video at every join. Each part fills exactly
//! its clip's length (see [`JoinPart::length`]), so files at different
//! frame rates joined from the same clips switch clips at the same
//! moments.
//!
//! Why not the concat demuxer: it cannot do the per-part hold, pad and
//! cut that a clip's length asks for (a shorter take's last frame held
//! past its end, its audio padded with silence; a longer take cut
//! short). Why not copy the recorded AAC through the way the video is
//! copied: every take's AAC starts with its own encoder priming, so a
//! passthrough join needs an edit list entry per join to skip it, and
//! FFmpeg (and the editors built on it) placed the second take's audio
//! ~20 ms early (measured 2026-10-13), more than a frame at 60 fps.
//! Re-encoded as one continuous track, there is no priming between
//! parts, and the one of the head is the only left: the plain muxing of
//! this AAC encoder's first packet at -frame_size writes the head edit
//! list and lands the decoded content at 0 (measured with this
//! system's libavformat).

use std::ffi::{CString, c_int};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ffmpeg_sys_next as av;
use isocast_engine::{
    AudioEncoder, JoinPart, JoinSpec, Joined, JoinedPart, LiveAudioSpec, OnJoinProgress,
    SAMPLE_RATE,
};
use isocast_protocol::Container;

use crate::stream::{LiveAacEncoder, av_error};

/// libav's timestamp standing for "no timestamp here".
const AV_NOPTS: i64 = i64::MIN;
/// libav's answer at the end of what is read.
const AV_EOF: c_int = -0x20464F45;
/// libav's "try again": `AVERROR(EAGAIN)`.
const AV_EAGAIN: c_int = -libc::EAGAIN;

/// Silence is added in pieces of at most this many samples (0.1 s).
const PAD_CHUNK: usize = 4_800;
/// How often progress is reported.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

/// A time base's scale as integers. Everything here deals in positive
/// times, and no stream is without a base.
fn scale(tb: av::AVRational) -> (i64, i64) {
    (i64::from(tb.num.max(1)), i64::from(tb.den.max(1)))
}

/// `seconds` in `tb` ticks, rounded half away from zero.
fn in_ticks(seconds: f64, tb: av::AVRational) -> i64 {
    let (num, den) = scale(tb);
    (seconds * den as f64 / num as f64).round() as i64
}

/// `ticks` of `tb` as seconds.
fn in_seconds(ticks: i64, tb: av::AVRational) -> f64 {
    let (num, den) = scale(tb);
    ticks as f64 * num as f64 / den as f64
}

/// Stream ticks at the audio sample rate, rounded half up.
fn audio_samples(ticks: i64, tb: av::AVRational) -> i64 {
    let (num, den) = scale(tb);
    (ticks * num * SAMPLE_RATE as i64 + den / 2) / den
}

/// Audio samples in stream ticks, rounded half up.
fn audio_ticks(samples: i64, tb: av::AVRational) -> i64 {
    let (num, den) = scale(tb);
    let rate = SAMPLE_RATE as i64;
    (samples * den * 2 + rate * num) / (2 * rate * num)
}

/// A time rescaled to another base (libav rounds that to nearest, half
/// away from zero: the same places CMTime's `RoundHalfAwayFromZero`
/// gave macOS). Only finite times go in; a caller with a sentinel
/// (`AV_NOPTS`) passes it through itself.
fn rescale(time: i64, from: av::AVRational, to: av::AVRational) -> i64 {
    debug_assert!(time != AV_NOPTS);
    unsafe { av::av_rescale_q(time, from, to) }
}

/// The AAC bitrate macOS's `aac_settings` picks: 128 kb/s up to stereo.
fn aac_bitrate_kbps(channels: u32) -> u32 {
    128 * channels.min(2)
}

/// A codec's name, for the error that names what differs.
fn codec_name(id: av::AVCodecID) -> String {
    let name = unsafe { av::avcodec_get_name(id) };
    if name.is_null() {
        "no codec".into()
    } else {
        unsafe {
            std::ffi::CStr::from_ptr(name)
                .to_string_lossy()
                .into_owned()
        }
    }
}

/// A pixel format's name, for the same reason.
fn pixel_name(format: c_int) -> String {
    let name = unsafe {
        let fmt: av::AVPixelFormat = std::mem::transmute(format);
        av::av_get_pix_fmt_name(fmt)
    };
    if name.is_null() {
        format!("{format}")
    } else {
        unsafe {
            std::ffi::CStr::from_ptr(name)
                .to_string_lossy()
                .into_owned()
        }
    }
}

pub fn join(spec: &JoinSpec, progress: OnJoinProgress) -> Result<Joined> {
    if spec.parts.is_empty() {
        bail!("nothing to join");
    }
    if spec.output.exists() {
        bail!("{} already exists", spec.output.display());
    }
    let partial = partial_path(&spec.output);
    let result = write(spec, &partial, progress);
    match result {
        Ok(joined) => {
            rename_new(&partial, &spec.output).with_context(|| {
                format!(
                    "could not rename {} to {}",
                    partial.display(),
                    spec.output.display()
                )
            })?;
            Ok(joined)
        }
        Err(err) => {
            // Only what this join wrote.
            let _ = std::fs::remove_file(&partial);
            Err(err)
        }
    }
}

/// Rename `from` to `to` unless `to` exists (a file made there while
/// the join ran is never replaced). Linux's plain rename would replace
/// it, so unlike macOS's `renamex_np(RENAME_EXCL)` the check instead of
/// the rename does the keeping: the window it narrows is between the
/// two.
fn rename_new(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        bail!(
            "{} appeared while the export ran; the joined file is kept as {}",
            to.display(),
            from.display()
        );
    }
    std::fs::rename(from, to)?;
    Ok(())
}

/// `name.mov` → `name.partial.mov`, written until the join is complete.
fn partial_path(output: &Path) -> PathBuf {
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match output.extension() {
        Some(extension) => format!("{stem}.partial.{}", extension.to_string_lossy()),
        None => format!("{stem}.partial"),
    };
    output.with_file_name(name)
}

/// One file to join, opened and inspected.
struct Part {
    path: PathBuf,
    input: *mut av::AVFormatContext,
    /// The video stream's index, its time base (which the output video
    /// stream copies), and its length in those ticks if the demuxer
    /// could tell.
    video: usize,
    video_tb: av::AVRational,
    length: i64,
    /// Its audio tracks in file order: stream index and channels.
    audio: Vec<(usize, u32)>,
}

impl Drop for Part {
    fn drop(&mut self) {
        unsafe { av::avformat_close_input(&mut self.input) };
    }
}

/// A read packet whose own buffer is freed when dropped.
struct ReadPacket(*mut av::AVPacket);

impl Drop for ReadPacket {
    fn drop(&mut self) {
        unsafe { av::av_packet_free(&mut self.0) };
    }
}

/// A decode frame freed when dropped.
struct DecodeFrame(*mut av::AVFrame);

impl Drop for DecodeFrame {
    fn drop(&mut self) {
        unsafe { av::av_frame_free(&mut self.0) };
    }
}

/// One audio stream's decoder, for one part.
struct Decoder {
    ctx: *mut av::AVCodecContext,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { av::avcodec_free_context(&mut self.ctx) };
    }
}

/// One audio track's place in the join.
struct Audio {
    channels: usize,
    /// Samples encoded so far, over all parts: every sample's place.
    written: i64,
    /// AAC packets muxed so far (their places derive from this count
    /// and the encoder's delay: frame n decodes the samples of
    /// `n × frame_size`, presented from `n × frame_size - delay`).
    packets: i64,
    /// Silence still to add before the current part's audio, and
    /// samples of it still to skip (its audio starts before or after
    /// its video).
    pad: i64,
    skip: i64,
    /// The current part's decoder and converter; they end with the
    /// part.
    decoder: Option<Decoder>,
    swr: *mut av::SwrContext,
}

impl Drop for Audio {
    fn drop(&mut self) {
        unsafe {
            if !self.swr.is_null() {
                av::swr_free(&mut self.swr);
            }
        }
    }
}

/// The file being written, closed and freed when dropped.
struct Out {
    ctx: *mut av::AVFormatContext,
    /// Whether the muxer's own `AVIOContext` needs closing (a file
    /// output opened one).
    file: bool,
}

impl Drop for Out {
    fn drop(&mut self) {
        unsafe {
            if self.file {
                av::avio_closep(&mut (*self.ctx).pb);
            }
            av::avformat_free_context(self.ctx);
        }
    }
}

/// What one audio track's AAC is written through.
struct Mux {
    out: *mut av::AVFormatContext,
    /// The one packet all the muxed AAC goes out in, reused: an
    /// interleaved write frees its buffer, a fresh one goes in the
    /// next call.
    packet: *mut av::AVPacket,
    stream_index: usize,
    audio_tb: av::AVRational,
}

/// One stream of a part, by index.
unsafe fn stream_of(part: &Part, index: usize) -> *const av::AVStream {
    unsafe { *(*part.input).streams.add(index) }
}

/// Whether a video stream the demuxer saw any content of.
unsafe fn has_frames(stream: *const av::AVStream) -> bool {
    unsafe { (*stream).nb_frames > 0 || ((*stream).duration != AV_NOPTS && (*stream).duration > 0) }
}

/// A stream's start, when the demuxer named one. The demuxer's
/// `start_time` stands where the content begins, the edit list's own
/// skipping included.
unsafe fn stream_start(stream: *const av::AVStream) -> Option<i64> {
    unsafe {
        let time = (*stream).start_time;
        (time != AV_NOPTS).then_some(time)
    }
}

fn open_part(spec: &JoinPart) -> Result<Part> {
    let path = &spec.path;
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let text = path.to_str().context("the file path is not valid UTF-8")?;
    let file = CString::new(text).context("the file path has a NUL")?;
    let mut input: *mut av::AVFormatContext = std::ptr::null_mut();
    let result = unsafe {
        av::avformat_open_input(
            &mut input,
            file.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result < 0 {
        // The context frees itself on the way out.
        bail!("could not open {}: {}", path.display(), av_error(result));
    }

    let build = (|| -> Result<Part> {
        let result = unsafe { av::avformat_find_stream_info(input, std::ptr::null_mut()) };
        if result < 0 {
            bail!("could not read {}: {}", path.display(), av_error(result));
        }
        unsafe {
            let streams = std::slice::from_raw_parts(
                (*input).streams as *mut *const av::AVStream,
                (*input).nb_streams as usize,
            );
            let video = av::av_find_best_stream(
                input,
                av::AVMediaType::AVMEDIA_TYPE_VIDEO,
                -1,
                -1,
                std::ptr::null_mut(),
                0,
            );
            if video < 0 {
                bail!("{} has no video", path.display());
            }
            let stream = streams[video as usize];
            if !has_frames(stream) {
                bail!("{} has no video frames", path.display());
            }
            // The file's video length, for a part whose clip length the
            // spec does not give.
            let length = if (*stream).duration != AV_NOPTS && (*stream).duration > 0 {
                (*stream).duration
            } else if (*input).duration > 0 {
                rescale(
                    (*input).duration,
                    av::AVRational {
                        num: 1,
                        den: av::AV_TIME_BASE,
                    },
                    (*stream).time_base,
                )
            } else {
                0
            };
            let mut audio = Vec::new();
            for (index, stream) in streams.iter().copied().enumerate() {
                let par = (*stream).codecpar;
                if (*par).codec_type != av::AVMediaType::AVMEDIA_TYPE_AUDIO {
                    continue;
                }
                let channels = (*par).ch_layout.nb_channels;
                if channels <= 0 {
                    bail!("{}: an audio track without channels", path.display());
                }
                audio.push((index, channels as u32));
            }
            Ok(Part {
                path: path.to_path_buf(),
                input,
                video: video as usize,
                video_tb: (*stream).time_base,
                length,
                audio,
            })
        }
    })();
    if build.is_err() {
        unsafe { av::avformat_close_input(&mut input) };
    }
    build
}

fn open_decoder(part: &Part, stream: *const av::AVStream) -> Result<Decoder> {
    unsafe {
        let par = (*stream).codecpar;
        let codec = av::avcodec_find_decoder((*par).codec_id);
        if codec.is_null() {
            bail!("{}'s audio has no decoder", part.path.display());
        }
        let mut ctx = av::avcodec_alloc_context3(codec);
        if ctx.is_null() {
            bail!("libavcodec gave no decoder for {}", part.path.display());
        }
        let result = av::avcodec_parameters_to_context(ctx, par);
        if result < 0 {
            av::avcodec_free_context(&mut ctx);
            bail!("{}'s audio: {}", part.path.display(), av_error(result));
        }
        let result = av::avcodec_open2(ctx, codec, std::ptr::null_mut());
        if result < 0 {
            av::avcodec_free_context(&mut ctx);
            bail!(
                "could not decode {}: {}",
                part.path.display(),
                av_error(result)
            );
        }
        Ok(Decoder { ctx })
    }
}

/// A track's part ends here: its decoder and converter end with it.
fn close_audio(state: &mut Audio) {
    state.decoder = None;
    if !state.swr.is_null() {
        unsafe { av::swr_free(&mut state.swr) };
    }
}

/// One raw AAC frame muxed to the audio track. Its place comes from the
/// encoder's own sample count: the packets' contents sit at sample
/// positions however they were encoded, and the muxer's edit list
/// trims the encoder's priming off the head (the plain muxing of the
/// native encoder's first packet at -frame_size lands the decoded
/// content at 0, measured with this system's libavformat).
fn mux_aac(mux: &Mux, bytes: &[u8], pts: i64, duration: i64) -> Result<()> {
    unsafe {
        av::av_packet_unref(mux.packet);
        if av::av_new_packet(mux.packet, bytes.len() as c_int) != 0 {
            bail!("libavformat gave no room for an AAC frame");
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), (*mux.packet).data.cast(), bytes.len());
        (*mux.packet).stream_index = mux.stream_index as c_int;
        (*mux.packet).pts = pts;
        (*mux.packet).dts = pts;
        (*mux.packet).duration = duration;
        (*mux.packet).flags = av::AV_PKT_FLAG_KEY;
        // Takes the packet's buffer (even on error), so nothing is
        // unref'd after this.
        let result = av::av_interleaved_write_frame(mux.out, mux.packet);
        if result < 0 {
            bail!("could not write an audio frame: {}", av_error(result));
        }
    }
    Ok(())
}

/// Encode samples into the track's AAC and mux what comes out. This is
/// where the track's whole place moves: what goes in gets counted at
/// both ends, the samples encoded and the frames muxed.
fn encode_audio(
    mux: &Mux,
    enc: &mut LiveAacEncoder,
    samples: &[f32],
    state: &mut Audio,
) -> Result<()> {
    let channels = state.channels;
    let frame_size = enc.frame_size();
    let frame_ticks = audio_ticks(frame_size as i64, mux.audio_tb);
    let delay = i64::from(enc.delay());
    for bytes in enc.encode(samples)? {
        // Frame n decodes the samples of `n × frame_size`.
        let pts = audio_ticks(state.packets * frame_size as i64 - delay, mux.audio_tb);
        mux_aac(mux, &bytes, pts, frame_ticks)?;
        state.packets += 1;
    }
    state.written += (samples.len() / channels) as i64;
    Ok(())
}

/// Pad a track with silence, in pieces at most [`PAD_CHUNK`], until the
/// part's end. Answers what went in.
fn pad_audio(
    mux: &Mux,
    state: &mut Audio,
    enc: &mut LiveAacEncoder,
    end: i64,
    count: i64,
) -> Result<i64> {
    let mut rest = count.min(end - state.written).max(0) as usize;
    let mut filled = 0;
    while rest > 0 {
        let chunk = rest.min(PAD_CHUNK);
        let silence = vec![0.0f32; chunk * state.channels];
        encode_audio(mux, enc, &silence, state)?;
        filled += chunk as i64;
        rest -= chunk;
    }
    Ok(filled)
}

/// Samples added to a track's place: the part's silence before its own
/// audio, then the audio itself, cut at the region's end.
fn add_audio(
    mux: &Mux,
    state: &mut Audio,
    enc: &mut LiveAacEncoder,
    end: i64,
    samples: &[f32],
) -> Result<()> {
    if state.pad > 0 {
        let filled = pad_audio(mux, state, enc, end, state.pad.min(end - state.written))?;
        state.pad -= filled;
    }
    let channels = state.channels;
    let mut samples = samples;
    if state.skip > 0 {
        let skipped = (state.skip as usize).min(samples.len() / channels);
        samples = &samples[skipped * channels..];
        state.skip -= skipped as i64;
    }
    let take = ((end - state.written).max(0) as usize).min(samples.len() / channels);
    if take == 0 {
        // Content starting past the region: dropped like a longer
        // take's surplus (macOS's reader keeps running; so does this
        // loop, the samples just go by).
        return Ok(());
    }
    encode_audio(mux, enc, &samples[..take * channels], state)
}

/// One packet fed to the track's decoder, its output taken out,
/// converted and placed.
fn decode_audio(
    part: &Part,
    state: &mut Audio,
    end: i64,
    packet: *mut av::AVPacket,
    frame: *mut av::AVFrame,
    mux: &Mux,
    enc: &mut LiveAacEncoder,
) -> Result<()> {
    let decoder = state.decoder.as_ref().expect("a decoder, opened").ctx;
    let mut sent = unsafe { av::avcodec_send_packet(decoder, packet) };
    if sent == AV_EAGAIN {
        // The decoder was holding output; it comes out below, then the
        // packet goes in.
        take_audio(part, state, end, frame, mux, enc)?;
        sent = unsafe { av::avcodec_send_packet(decoder, packet) };
    }
    if sent < 0 {
        bail!(
            "could not decode {}: {}",
            part.path.display(),
            av_error(sent)
        );
    }
    take_audio(part, state, end, frame, mux, enc)
}

/// Everything the decoder holds, converted to interleaved 48 kHz
/// floats (AAC comes in planar ones) and placed.
fn take_audio(
    part: &Part,
    state: &mut Audio,
    end: i64,
    frame: *mut av::AVFrame,
    mux: &Mux,
    enc: &mut LiveAacEncoder,
) -> Result<()> {
    let decoder = state.decoder.as_ref().expect("a decoder").ctx;
    loop {
        let got = unsafe { av::avcodec_receive_frame(decoder, frame) };
        if got == AV_EAGAIN {
            return Ok(());
        }
        if got < 0 {
            bail!(
                "could not decode {}: {}",
                part.path.display(),
                av_error(got)
            );
        }
        unsafe { convert_audio(part, state, end, frame, mux, enc)? };
        unsafe { av::av_frame_unref(frame) };
    }
}

/// A decoded frame's samples, through the converter, into the track's
/// place.
unsafe fn convert_audio(
    part: &Part,
    state: &mut Audio,
    end: i64,
    frame: *mut av::AVFrame,
    mux: &Mux,
    enc: &mut LiveAacEncoder,
) -> Result<()> {
    unsafe {
        if state.swr.is_null() {
            // A converter from whatever the decoder gave (its layout
            // and planar floats) to interleaved 48 kHz: the engine's
            // encoder contract. AAC pass-through needs no rate change.
            let mut to_layout: av::AVChannelLayout = std::mem::zeroed();
            av::av_channel_layout_default(&mut to_layout, (*frame).ch_layout.nb_channels);
            let fmt: av::AVSampleFormat = std::mem::transmute((*frame).format);
            let result = av::swr_alloc_set_opts2(
                &mut state.swr,
                &to_layout,
                av::AVSampleFormat::AV_SAMPLE_FMT_FLT,
                SAMPLE_RATE as c_int,
                &(*frame).ch_layout,
                fmt,
                (*frame).sample_rate,
                0,
                std::ptr::null_mut(),
            );
            if result < 0 {
                bail!(
                    "{}'s audio needs a converter this libav does not have: {}",
                    part.path.display(),
                    av_error(result)
                );
            }
            // A default layout is no allocation: nothing to uninit.
            if av::swr_init(state.swr) < 0 {
                av::swr_free(&mut state.swr);
                bail!("could not convert {}'s audio", part.path.display());
            }
        }
        let samples = (*frame).nb_samples.max(0) as usize;
        let channels = state.channels;
        let mut flat = vec![0.0f32; samples * channels];
        let planes = [flat.as_mut_ptr() as *mut u8];
        let taken = av::swr_convert(
            state.swr,
            planes.as_ptr().cast(),
            samples as c_int,
            (*frame).extended_data.cast(),
            (*frame).nb_samples,
        );
        if taken < 0 {
            bail!(
                "could not convert {}'s audio: {}",
                part.path.display(),
                av_error(taken)
            );
        }
        flat.truncate(taken.max(0) as usize * channels);
        add_audio(mux, state, enc, end, &flat)
    }
}

fn write(spec: &JoinSpec, path: &Path, progress: OnJoinProgress) -> Result<Joined> {
    let parts = spec
        .parts
        .iter()
        .map(open_part)
        .collect::<Result<Vec<_>>>()?;
    let tracks = parts[0].audio.len();
    let track_channels: Vec<u32> = parts[0]
        .audio
        .iter()
        .map(|&(_, channels)| channels)
        .collect();
    for part in &parts[1..] {
        if part.audio.len() != tracks {
            bail!(
                "{} has {} audio tracks, {} has {tracks}; files joined need the same tracks",
                part.path.display(),
                part.audio.len(),
                parts[0].path.display()
            );
        }
    }
    for part in parts.iter().skip(1) {
        for (t, &(_, channels)) in part.audio.iter().enumerate() {
            if channels != track_channels[t] {
                bail!(
                    "{} has {channels} channels on its track {t}, {} has {}; files joined need \
                     the same channels",
                    part.path.display(),
                    parts[0].path.display(),
                    track_channels[t]
                );
            }
        }
    }

    // The first part's video is the one the output holds.
    let (codec_id, width, height, format) = unsafe {
        let par = (*stream_of(&parts[0], parts[0].video)).codecpar;
        ((*par).codec_id, (*par).width, (*par).height, (*par).format)
    };
    for part in &parts[1..] {
        unsafe {
            let par = (*stream_of(part, part.video)).codecpar;
            if (*par).codec_id != codec_id {
                bail!(
                    "{}: {} video, {} has {}; files joined need the same codec",
                    part.path.display(),
                    codec_name((*par).codec_id),
                    parts[0].path.display(),
                    codec_name(codec_id)
                );
            }
            if (*par).width != width || (*par).height != height {
                bail!(
                    "{}: {}x{}, {} has {width}x{height}; files joined need the same size",
                    part.path.display(),
                    (*par).width,
                    (*par).height,
                    parts[0].path.display()
                );
            }
            if (*par).format != format {
                bail!(
                    "{}: {} pixels, {} has {}; files joined need the same pixel format",
                    part.path.display(),
                    pixel_name((*par).format),
                    parts[0].path.display(),
                    pixel_name(format)
                );
            }
        }
    }

    let out_text = path.to_str().context("the file path is not valid UTF-8")?;
    let out_file = CString::new(out_text).context("the file path has a NUL")?;
    let mut ctx: *mut av::AVFormatContext = std::ptr::null_mut();
    // The container names its muxer: the output's extension would say
    // the same, but a named one cannot be guessed wrong.
    let format_name = match spec.container {
        Container::Mov => c"mov",
        Container::Mp4 => c"mp4",
    };
    let result = unsafe {
        av::avformat_alloc_output_context2(
            &mut ctx,
            std::ptr::null_mut(),
            format_name.as_ptr(),
            out_file.as_ptr(),
        )
    };
    if result < 0 || ctx.is_null() {
        bail!(
            "could not open {} for writing: {}",
            path.display(),
            av_error(result)
        );
    }
    let mut out = Out { ctx, file: false };
    // One packet comes in (reference-counted; the video ones go out as
    // they came, retimed in place), one goes out per muxed AAC frame
    // (the interleaved write frees its buffer, so a fresh one goes in
    // every call). One decode frame, reused.
    let read_packet = ReadPacket(unsafe { av::av_packet_alloc() });
    let out_packet = ReadPacket(unsafe { av::av_packet_alloc() });
    let read_frame = DecodeFrame(unsafe { av::av_frame_alloc() });
    if read_packet.0.is_null() || out_packet.0.is_null() || read_frame.0.is_null() {
        bail!("libavformat gave no packet or frame for {}", path.display());
    }

    // The video stream, copied from the first part, followed by one AAC
    // track per audio track, encoded over all the parts.
    let mut encoders: Vec<LiveAacEncoder> = Vec::new();
    let mut audio_streams: Vec<*mut av::AVStream> = Vec::new();
    let video_stream: *mut av::AVStream =
        unsafe { av::avformat_new_stream(out.ctx, std::ptr::null()) };
    unsafe {
        if video_stream.is_null() {
            bail!("libavformat gave no video stream for {}", path.display());
        }
        let first_video = stream_of(&parts[0], parts[0].video);
        (*video_stream).time_base = (*first_video).time_base;
        let result = av::avcodec_parameters_copy((*video_stream).codecpar, (*first_video).codecpar);
        if result < 0 {
            bail!("could not set up the video track: {}", av_error(result));
        }
        // The muxer decides the file's own tag from the codec.
        (*(*video_stream).codecpar).codec_tag = 0;

        for channels in &track_channels {
            let enc = LiveAacEncoder::new(&LiveAudioSpec {
                channels: *channels,
                bitrate_kbps: aac_bitrate_kbps(*channels),
            })?;
            let stream = av::avformat_new_stream(out.ctx, std::ptr::null());
            if stream.is_null() {
                bail!("libavformat gave no audio track for {}", path.display());
            }
            (*stream).time_base = av::AVRational {
                num: 1,
                den: SAMPLE_RATE as c_int,
            };
            let result =
                av::avcodec_parameters_from_context((*stream).codecpar, enc.codec_context());
            if result < 0 {
                bail!("could not set up an audio track: {}", av_error(result));
            }
            audio_streams.push(stream);
            encoders.push(enc);
        }

        if (*(*out.ctx).oformat).flags & av::AVFMT_NOFILE == 0 {
            let result = av::avio_open(&mut (*out.ctx).pb, out_file.as_ptr(), av::AVIO_FLAG_WRITE);
            if result < 0 {
                bail!("could not create {}: {}", path.display(), av_error(result));
            }
            out.file = true;
        }
        let result = av::avformat_write_header(out.ctx, std::ptr::null_mut());
        if result < 0 {
            bail!(
                "could not start writing {}: {}",
                path.display(),
                av_error(result)
            );
        }
    }

    // The muxer replaced the time base hints with what it will write;
    // every place below is counted against those, not the hints.
    let (out_tb, video_index) =
        unsafe { ((*video_stream).time_base, (*video_stream).index as usize) };
    let muxes: Vec<Mux> = audio_streams
        .iter()
        .map(|stream| unsafe {
            Mux {
                out: out.ctx,
                packet: out_packet.0,
                stream_index: (**stream).index as usize,
                audio_tb: (**stream).time_base,
            }
        })
        .collect();

    // Where each part starts in the joined file: the parts' lengths
    // added up, exactly. Each track's audio region is those starts, in
    // samples.
    let mut lengths: Vec<i64> = Vec::with_capacity(parts.len());
    for (k, part) in parts.iter().enumerate() {
        lengths.push(match spec.parts[k].length {
            Some((frames, fps)) if fps > 0 => {
                // A clip exactly `frames / fps` long, in the output's
                // ticks, rounded half away from zero: the same places
                // every file rate joins at.
                let (num, den) = scale(out_tb);
                let (frames, fps) = (frames as i64, fps as i64);
                (frames * den * 2 + fps * num) / (2 * fps * num)
            }
            // The file's own video length.
            _ => rescale(part.length, part.video_tb, out_tb),
        });
    }
    let mut starts: Vec<i64> = Vec::with_capacity(parts.len());
    let mut offsets: Vec<i64> = Vec::with_capacity(parts.len());
    let mut cursor = 0;
    for (k, _part) in parts.iter().enumerate() {
        starts.push(cursor);
        cursor += lengths[k];
        offsets.push(in_ticks(spec.parts[k].offset_seconds.max(0.0), out_tb));
    }
    let total = cursor;
    let bounds: Vec<i64> = {
        let mut samples = starts
            .iter()
            .map(|start| audio_samples(*start, out_tb))
            .collect::<Vec<_>>();
        samples.push(audio_samples(total, out_tb));
        samples
    };

    let mut audio: Vec<Audio> = track_channels
        .iter()
        .map(|&channels| Audio {
            channels: channels as usize,
            written: 0,
            packets: 0,
            pad: 0,
            skip: 0,
            decoder: None,
            swr: std::ptr::null_mut(),
        })
        .collect();
    let mut frames: Vec<u64> = vec![0; parts.len()];
    let mut left_out: Vec<u64> = vec![0; parts.len()];
    let mut appended = 0;
    let mut reported = Instant::now();

    let mut loop_parts = || -> Result<()> {
        if !progress(0.0) {
            bail!("the join was stopped");
        }
        let total_seconds = in_seconds(total, out_tb).max(1e-9);
        for (k, part) in parts.iter().enumerate() {
            let start = starts[k];
            let offset = offsets[k];
            let end = starts[k] + lengths[k];
            let vt = part.video_tb;
            // Where the part's own audio starts against its video, in
            // samples: after the file's offset in its clip, plus the
            // audio track's own start against the file's video. Silence
            // fills what comes before it; its first samples skip what
            // of it is already past the region's start.
            let video = unsafe { stream_of(part, part.video) };
            let video_start = audio_samples(unsafe { stream_start(video).unwrap_or(0) }, vt);
            let mut base = unsafe { stream_start(video) };
            let mut base_dts: Option<i64> = None;
            let offset_samples =
                (spec.parts[k].offset_seconds.max(0.0) * SAMPLE_RATE as f64).round() as i64;
            for (t, &(stream, _)) in part.audio.iter().enumerate() {
                let stream = unsafe { stream_of(part, stream) };
                let audio_start =
                    audio_samples(unsafe { stream_start(stream).unwrap_or(0) }, unsafe {
                        (*stream).time_base
                    });
                let lead = offset_samples + audio_start - video_start;
                audio[t].pad = lead.max(0);
                audio[t].skip = (-lead).max(0);
                close_audio(&mut audio[t]);
            }
            loop {
                let got = unsafe { av::av_read_frame(part.input, read_packet.0) };
                if got == AV_EOF {
                    break;
                }
                if got < 0 {
                    bail!("could not read {}: {}", part.path.display(), av_error(got));
                }
                let index = unsafe { (*read_packet.0).stream_index } as usize;
                if index == part.video {
                    let pts = unsafe { (*read_packet.0).pts };
                    if pts == AV_NOPTS {
                        bail!("{}'s video has a frame without a time", part.path.display());
                    }
                    // The first frame measures where the file's video
                    // starts, the demuxer having named nothing.
                    let base = *base.get_or_insert(pts);
                    // Every moved time: the frame's place against the
                    // file's own start, plus the part's place (and, a
                    // file having started after its clip, its offset in
                    // it).
                    let out_pts = rescale(pts - base, vt, out_tb) + start + offset;
                    if out_pts >= end {
                        // A frame starting past the clip's end isn't
                        // in it.
                        left_out[k] += 1;
                        unsafe { av::av_packet_unref(read_packet.0) };
                        continue;
                    }
                    let dts = unsafe { (*read_packet.0).dts };
                    let base_dts = *base_dts.get_or_insert(if dts != AV_NOPTS { dts } else { pts });
                    let out_dts = if dts != AV_NOPTS {
                        rescale(dts - base_dts, vt, out_tb) + start + offset
                    } else {
                        out_pts
                    };
                    unsafe {
                        (*read_packet.0).pts = out_pts;
                        (*read_packet.0).dts = out_dts;
                        let duration = (*read_packet.0).duration;
                        (*read_packet.0).duration = if duration == AV_NOPTS {
                            duration
                        } else {
                            rescale(duration, vt, out_tb)
                        };
                        (*read_packet.0).pos = -1;
                        (*read_packet.0).stream_index = video_index as c_int;
                        let wrote = av::av_interleaved_write_frame(out.ctx, read_packet.0);
                        if wrote < 0 {
                            bail!(
                                "could not write a video frame of {}: {}",
                                part.path.display(),
                                av_error(wrote)
                            );
                        }
                    }
                    frames[k] += 1;
                    appended = out_pts;
                } else if let Some(t) = part.audio.iter().position(|&(stream, _)| stream == index) {
                    if audio[t].written < bounds[k + 1] {
                        if audio[t].decoder.is_none() {
                            audio[t].decoder =
                                Some(open_decoder(part, unsafe { stream_of(part, index) })?);
                        }
                        decode_audio(
                            part,
                            &mut audio[t],
                            bounds[k + 1],
                            read_packet.0,
                            read_frame.0,
                            &muxes[t],
                            &mut encoders[t],
                        )?;
                    }
                    unsafe { av::av_packet_unref(read_packet.0) };
                } else {
                    // Data and the like: nothing this file join uses.
                    unsafe { av::av_packet_unref(read_packet.0) };
                }
                if reported.elapsed() >= PROGRESS_EVERY {
                    reported = Instant::now();
                    if !progress((in_seconds(appended, out_tb) / total_seconds).clamp(0.0, 1.0)) {
                        bail!("the join was stopped");
                    }
                }
            }
            // The part's audio regions end here: whatever its own audio
            // did not fill them with, silence does (a part whose audio
            // ran out, or never got there).
            for t in 0..audio.len() {
                let end = bounds[k + 1];
                let count = end - audio[t].written;
                pad_audio(&muxes[t], &mut audio[t], &mut encoders[t], end, count)?;
                close_audio(&mut audio[t]);
                audio[t].pad = 0;
                audio[t].skip = 0;
            }
        }
        Ok(())
    };
    loop_parts()?;

    // The encoder's last frames only come out on a flush, and its
    // priming holds a first frame of its own: the written count is
    // padded across enough whole frames that the flushed packets'
    // places reach the regions' end (their content is silence, the
    // video still ends the file), then the flush takes what is left.
    // Without this the audio ends its frames' worth short of the last
    // part's end.
    for t in 0..audio.len() {
        let frame_size = encoders[t].frame_size() as i64;
        let delay = i64::from(encoders[t].delay());
        let frame_ticks = audio_ticks(frame_size, muxes[t].audio_tb);
        let state = &mut audio[t];
        let enc = &mut encoders[t];
        let pad = (frame_size - state.written % frame_size) % frame_size;
        if pad > 0 {
            pad_audio(&muxes[t], state, enc, i64::MAX, pad)?;
        }
        for bytes in enc.flush()? {
            // Frame n decodes the samples of `n × frame_size`.
            let pts = audio_ticks(state.packets * frame_size - delay, muxes[t].audio_tb);
            mux_aac(&muxes[t], &bytes, pts, frame_ticks)?;
            state.packets += 1;
        }
    }

    unsafe {
        // Whatever the interleaving queues held goes out now, the way
        // ffmpeg's own writes end: a flush, then the trailer.
        let flush = av::av_interleaved_write_frame(out.ctx, std::ptr::null_mut());
        if flush < 0 {
            bail!("could not flush {}: {}", path.display(), av_error(flush));
        }
        let trailer = av::av_write_trailer(out.ctx);
        if trailer < 0 {
            bail!("could not finish {}: {}", path.display(), av_error(trailer));
        }
    }
    progress(1.0);

    let parts_out: Vec<JoinedPart> = starts
        .iter()
        .zip(lengths.iter())
        .enumerate()
        .map(|(k, (start, length))| JoinedPart {
            start_seconds: in_seconds(*start, out_tb),
            duration_seconds: in_seconds(*length, out_tb),
            frames: frames[k],
            frames_left_out: left_out[k],
            // Video packets are copied as they are; a part in another
            // format isn't converted here (it fails the join instead).
            reencoded: 0,
            converted: false,
        })
        .collect();
    Ok(Joined {
        duration_seconds: in_seconds(total, out_tb),
        frames: frames.iter().sum(),
        parts: parts_out,
    })
}
