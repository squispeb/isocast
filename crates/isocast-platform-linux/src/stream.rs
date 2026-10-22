//! Real-time encoders for streaming and live monitors: in-process libav
//! sessions against the system's shared libavcodec (FFmpeg n9), so
//! `receive` can hand back each encoded frame the moment it is done,
//! keyed to the encoder's own pacing — which an ffmpeg pipe cannot
//! honour. H.264 through NVENC (the GPU encoder, like macOS's
//! VideoToolbox sessions); AAC-LC is still L.5.2's task. The LGPL
//! libraries are linked dynamically, which the LGPL allows.

use std::collections::VecDeque;
use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use ffmpeg_sys_next as av;
use isocast_engine::{
    AudioEncoder, EncodedVideo, LiveAudioSpec, LiveVideoSpec, ParameterSets, SAMPLE_RATE, Time,
    VideoEncoder,
};

use crate::frames::{FrameKind, LinuxFrame};

/// Frames submitted but not yet encoded. A monitor discards the frame
/// instead of binding the tap's whole surface pool when the hardware
/// encoder is momentarily behind (macOS's `MAX_IN_FLIGHT`).
const MAX_IN_FLIGHT: usize = 8;

/// How long a stream waits for a submission slot before it too drops a
/// frame (macOS's `STALL_WAIT`).
const STALL_WAIT: Duration = Duration::from_millis(400);

/// A libav call's error as readable text.
pub(crate) fn av_error(code: c_int) -> String {
    let mut text = [0u8; 128];
    let ok = unsafe { av::av_strerror(code, text.as_mut_ptr().cast(), text.len()) };
    if ok == 0 {
        let end = text
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(text.len());
        String::from_utf8_lossy(&text[..end]).into_owned()
    } else {
        format!("libav error {code}")
    }
}

/// The error libav uses for "try again": `AVERROR(EAGAIN)`.
const AV_EAGAIN: c_int = -libc::EAGAIN;
/// libav's answer at the end of what a codec or container has.
const AV_EOF: c_int = -0x20464F45;

/// One access unit's NAL units, without their start codes, from an
/// Annex B payload. Answers `None` if the bytes are not Annex B (the
/// length-prefixed AVC layout needs no conversion here, but nvenc's
/// packets are Annex B, measured on this machine).
fn nal_units(payload: &[u8]) -> Option<Vec<&[u8]>> {
    if !(payload.starts_with(&[0, 0, 0, 1]) || payload.starts_with(&[0, 0, 1])) {
        return None;
    }
    let mut units = Vec::new();
    let mut unit_start = if payload.starts_with(&[0, 0, 0, 1]) {
        4
    } else {
        3
    };
    let mut scan = unit_start;
    while scan + 2 < payload.len() {
        if payload[scan..].starts_with(&[0, 0, 1]) {
            // A four-byte start code is a three-byte one behind a zero
            // byte that belongs to neither unit.
            let (code_start, unit_end) = if scan > 0 && payload[scan - 1] == 0 {
                (scan - 1, scan - 1)
            } else {
                (scan, scan)
            };
            if unit_end > unit_start {
                units.push(&payload[unit_start..unit_end]);
            }
            unit_start = code_start + 4;
            scan = unit_start;
        } else {
            scan += 1;
        }
    }
    if unit_start < payload.len() {
        units.push(&payload[unit_start..]);
    }
    Some(units)
}

/// The NAL units as the AVC file format holds them (see
/// [`EncodedVideo::data`]), each after its length as 4 big-endian bytes.
fn avc_layout(units: &[&[u8]]) -> Vec<u8> {
    let mut data = Vec::with_capacity(units.iter().map(|unit| unit.len() + 4).sum());
    for unit in units {
        data.extend_from_slice(&u32::try_from(unit.len()).expect("a NAL unit").to_be_bytes());
        data.extend_from_slice(unit);
    }
    data
}

/// The bare NAL payloads `ParameterSets` holds: the sequence and picture
/// parameter sets, as the FLV muxer uses them.
fn parameter_sets(units: &[&[u8]]) -> Option<ParameterSets> {
    let mut sets = ParameterSets::default();
    for unit in units {
        match unit.first()? & 0x1f {
            7 => sets.sps.push(unit.to_vec()),
            8 => sets.pps.push(unit.to_vec()),
            _ => {}
        }
    }
    if sets.sps.is_empty() || sets.pps.is_empty() {
        return None;
    }
    Some(sets)
}

/// One encoded frame, collected by the encoder's worker.
struct Output {
    frames: Mutex<VecDeque<EncodedVideo>>,
    ready: Condvar,
    /// Frames the encoder dropped instead of encoding, when the
    /// submission slot limit was behind.
    dropped: AtomicU64,
    /// The first thing that went wrong inside the worker, if anything
    /// did; `receive` surfaces it once, then fails like macOS's
    /// encoder callbacks failing the session.
    failed: Mutex<Option<String>>,
}

impl Output {
    fn push(&self, encoded: EncodedVideo) {
        self.frames.lock().unwrap().push_back(encoded);
        self.ready.notify_all();
    }
}

/// What the encoder's handle sends its worker.
enum Job {
    Encode {
        frame: LinuxFrame,
        pts: Time,
        keyframe: bool,
    },
}

enum Control {
    Rate { fps: u32, bitrate_kbps: u32 },
    Close,
}

/// A real-time H.264 encoder: NVENC through the system's libavcodec,
/// opened when its first frame names the pixel format.
pub struct LiveH264Encoder {
    spec: LiveVideoSpec,
    /// Given to `None` in `Drop`, *before* joining the worker: the
    /// worker leaves its `recv` when the last sender is gone, so the
    /// original sender must be gone while the join still waits.
    jobs: Option<SyncSender<Job>>,
    control: mpsc::Sender<Control>,
    output: Arc<Output>,
    worker: Option<JoinHandle<()>>,
}

impl LiveH264Encoder {
    pub fn new(spec: &LiveVideoSpec) -> Result<Self> {
        if !spec.width.is_multiple_of(2) || !spec.height.is_multiple_of(2) {
            bail!(
                "H.264 encoder width and height must be even (4:2:0), got \
                 {}x{} at the frame_stream::scaled output",
                spec.width,
                spec.height
            );
        }
        let codec = unsafe { av::avcodec_find_encoder_by_name(c"h264_nvenc".as_ptr()) };
        if codec.is_null() {
            bail!(
                "no hardware H.264 encoder: this system's libavcodec has no \
                 h264_nvenc, and only the GPU encoder honours the real-time \
                 contract"
            );
        }
        // A bounded submission queue is the in-flight limit: monitors
        // drop rather than queue behind eight frames; streams wait a
        // moment first and then drop as well.
        let (jobs, job_rx) = mpsc::sync_channel::<Job>(MAX_IN_FLIGHT);
        let (control, control_rx) = mpsc::channel::<Control>();
        let output = Arc::new(Output {
            frames: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            dropped: AtomicU64::new(0),
            failed: Mutex::new(None),
        });
        let spec = *spec;
        let shared = Arc::clone(&output);
        let worker = thread::Builder::new()
            .name("isocast h264 encoder".into())
            .spawn(move || unsafe { run_worker(&spec, job_rx, control_rx, &shared) })
            .map_err(|error| anyhow!("spawning the H.264 encoder worker: {error}"))?;
        Ok(Self {
            spec,
            jobs: Some(jobs),
            control,
            output,
            worker: Some(worker),
        })
    }
}

impl Drop for LiveH264Encoder {
    fn drop(&mut self) {
        let _ = self.control.send(Control::Close);
        drop(self.jobs.take()); // releases the worker blocked on recv
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl VideoEncoder for LiveH264Encoder {
    type Frame = LinuxFrame;

    /// Submit `frame`, shown at `pts`. The pixels are cloned onto the
    /// worker (a `LinuxFrame` clone shares the pool slot, like a
    /// retained `CVPixelBuffer` does), so the tap draws on while NVENC
    /// runs. When the submission queue is full a monitor drops the
    /// frame; a stream waits `STALL_WAIT` first and then drops it too.
    fn encode(&mut self, frame: &LinuxFrame, pts: Time, keyframe: bool) -> Result<()> {
        let new_job = || Job::Encode {
            frame: frame.clone(),
            pts,
            keyframe,
        };
        let Some(jobs) = self.jobs.as_ref() else {
            return Err(anyhow!("the H.264 encoder session is gone"));
        };
        match jobs.try_send(new_job()) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) if !self.spec.low_latency => {
                // A stream waits a moment for a slot before it too
                // drops the frame (polling, so this stays on stable
                // primitives).
                for _ in 0..(STALL_WAIT.as_millis() / 20).max(1) {
                    thread::sleep(Duration::from_millis(20));
                    match jobs.try_send(new_job()) {
                        Ok(()) => return Ok(()),
                        Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                }
                self.output.dropped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.output.dropped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
    }

    /// The next encoded frame, if one is ready.
    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        if let Some(failure) = self.output.failed.lock().unwrap().take() {
            return Err(anyhow!("H.264 encoder session failed: {failure}"));
        }
        Ok(self.output.frames.lock().unwrap().pop_front())
    }

    /// The next encoded frame, waiting up to `timeout` for one.
    fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<EncodedVideo>> {
        let mut frames = self.output.frames.lock().unwrap();
        let deadline = std::time::Instant::now() + timeout;
        while frames.is_empty() {
            (frames, _) = self.output.ready.wait_timeout(frames, timeout).unwrap();
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        drop(frames);
        self.receive()
    }

    fn dropped(&self) -> u64 {
        self.output.dropped.load(Ordering::Relaxed)
    }

    /// New rate control, keeping the session: no new keyframe.
    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) -> Result<()> {
        self.control
            .send(Control::Rate { fps, bitrate_kbps })
            .map_err(|_| anyhow!("H.264 encoder session is gone"))
    }
}

/// The encoder's worker: one NVENC session, frames in decode order out.
///
/// The session opens when the first frame arrives, because the pixel
/// format follows the pool the frame comes from: the program canvas's
/// Y'CbCr 4:2:0 slots and a source's BGRA slots feed different
/// formats, and NVENC takes both natively.
unsafe fn run_worker(
    spec: &LiveVideoSpec,
    jobs: mpsc::Receiver<Job>,
    control: mpsc::Receiver<Control>,
    output: &Output,
) {
    unsafe {
        let codec = av::avcodec_find_encoder_by_name(c"h264_nvenc".as_ptr());
        if codec.is_null() {
            record_failure(
                output,
                String::from("this system's libavcodec has no h264_nvenc"),
            );
            return;
        }
        let mut session = match Session::new(spec, codec) {
            Ok(session) => session,
            Err(error) => {
                record_failure(output, format!("opening the NVENC session: {error:#}"));
                return;
            }
        };
        while let Ok(job) = jobs.recv() {
            // Rate control comes first: it cannot wait behind a queue of
            // frames (it cannot be dropped, unlike a frame).
            while let Ok(command) = control.try_recv() {
                match command {
                    Control::Rate { fps, bitrate_kbps } => session.set_rate(fps, bitrate_kbps),
                    Control::Close => return,
                }
            }
            match job {
                Job::Encode {
                    frame,
                    pts,
                    keyframe,
                } => {
                    if session.encode(&frame, pts, keyframe, output).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

fn record_failure(output: &Output, failure: String) {
    let mut failed = output.failed.lock().unwrap();
    if failed.is_none() {
        *failed = Some(failure);
    }
    output.ready.notify_all();
}

/// One NVENC session through libavcodec.
struct Session {
    ctx: *mut av::AVCodecContext,
    frame: *mut av::AVFrame,
    packet: *mut av::AVPacket,
    pixel_format: av::AVPixelFormat,
    width: u32,
    height: u32,
    low_latency: bool,
    constant_bitrate: bool,
    open: bool,
}

impl Session {
    /// The codec configuration the session opens with; the pixel
    /// format arrives with the first frame.
    unsafe fn new(spec: &LiveVideoSpec, codec: *const av::AVCodec) -> Result<Session> {
        unsafe {
            let ctx = av::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                bail!("libavcodec could not allocate an encoder context");
            }
            (*ctx).width = spec.width as c_int;
            (*ctx).height = spec.height as c_int;
            // Nanosecond ticks everywhere, like the engine's media clock:
            // `encode` takes nanoseconds and `receive` hands them back.
            (*ctx).time_base = av::AVRational {
                num: 1,
                den: 1_000_000_000,
            };
            (*ctx).framerate = av::AVRational {
                num: spec.fps as c_int,
                den: 1,
            };
            // No B-frames: decode order is presentation order, so FLV needs
            // no composition offsets and `receive` hands frames out in
            // order (the S.2c finding).
            (*ctx).max_b_frames = 0;
            // The backstop cadence: a stream asks for its keyframes itself,
            // on time, and the monitors ask through `encode` as well.
            (*ctx).gop_size = spec.keyframe_interval as c_int;
            (*ctx).bit_rate = i64::from(spec.bitrate_kbps) * 1_000;
            attr_color(&mut *ctx);
            Ok(Session {
                ctx,
                frame: av::av_frame_alloc(),
                packet: av::av_packet_alloc(),
                pixel_format: av::AVPixelFormat::AV_PIX_FMT_NONE,
                width: spec.width,
                height: spec.height,
                low_latency: spec.low_latency,
                constant_bitrate: spec.constant_bitrate,
                open: false,
            })
        }
    }

    /// Open (once) against the pool's pixel format.
    unsafe fn open(&mut self, codec: *const av::AVCodec, kind: FrameKind) -> Result<()> {
        unsafe {
            if self.open {
                return Ok(());
            }
            let wanted = match kind {
                FrameKind::Bgra => av::AVPixelFormat::AV_PIX_FMT_BGRA,
                FrameKind::Ycbcr8 => av::AVPixelFormat::AV_PIX_FMT_NV12,
            };
            let formats = supported_formats(codec)?;
            if !formats.contains(&wanted) {
                let names = formats
                    .iter()
                    .map(|format| format_name(*format))
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    "the h264_nvenc encoder takes one of {names} as pixels, not \
                 the pool's {kind:?} format"
                );
            }
            (*self.ctx).pix_fmt = wanted;
            (*self.ctx).sw_pix_fmt = wanted;
            let mut options: *mut av::AVDictionary = std::ptr::null_mut();
            // Monitors want the frame out as soon as it is encoded with an
            // average bitrate; streams hold the bitrate every second when
            // asked (NVENC's constant bitrate padding) and otherwise run
            // variable bitrate.
            let preset = if self.low_latency { c"p1" } else { c"p4" };
            let tune = if self.low_latency { c"ull" } else { c"hq" };
            let rate_control = if self.low_latency {
                c"vbr"
            } else if self.constant_bitrate {
                c"cbr"
            } else {
                c"vbr"
            };
            for (name, value) in [
                (c"preset", preset),
                (c"tune", tune),
                (c"rc", rate_control),
                (c"profile", c"high"),
                // A monitor that joins mid-stream asks for a keyframe;
                // nvenc's forced picture type defaults to a trick I frame
                // without the key flag or new headers, which a joining
                // viewer cannot decode. Forced keyframes are IDRs instead.
                (c"forced-idr", c"1"),
            ] {
                av::av_dict_set(&mut options, name.as_ptr(), value.as_ptr(), 0);
            }
            // One second of room for the rate control to breathe.
            (*self.ctx).rc_max_rate = (*self.ctx).bit_rate;
            (*self.ctx).rc_buffer_size = (*self.ctx).bit_rate as c_int;
            let result = av::avcodec_open2(self.ctx, codec, &mut options);
            av::av_dict_free(&mut options);
            if result != 0 {
                bail!("opening the h264_nvenc session: {}", av_error(result));
            }
            self.pixel_format = wanted;
            self.open = true;
        }
        Ok(())
    }

    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) {
        unsafe {
            (*self.ctx).bit_rate = i64::from(bitrate_kbps) * 1_000;
            (*self.ctx).rc_max_rate = (*self.ctx).bit_rate;
            (*self.ctx).framerate = av::AVRational {
                num: fps as c_int,
                den: 1,
            };
        }
    }

    /// One frame through the session; every packet it completes comes
    /// out on `output` in decode order.
    unsafe fn encode(
        &mut self,
        frame: &LinuxFrame,
        pts: Time,
        keyframe: bool,
        output: &Output,
    ) -> Result<()> {
        unsafe {
            if frame.size() != (self.width, self.height) {
                bail!(
                    "an H.264 frame of {:?} cannot enter a session opened for                  {}x{}",
                    frame.size(),
                    self.width,
                    self.height
                );
            }
            self.open((*self.ctx).codec, frame.kind())?;
            let planes = match frame.kind() {
                FrameKind::Bgra => vec![frame.plane(0)?],
                FrameKind::Ycbcr8 => vec![frame.plane(0)?, frame.plane(1)?],
            };
            let av_frame = self.frame;
            (*av_frame).width = (*self.ctx).width;
            (*av_frame).height = (*self.ctx).height;
            (*av_frame).format = self.pixel_format as c_int;
            for (index, (bytes, stride)) in planes.iter().enumerate() {
                (*av_frame).data[index] = bytes.as_ptr().cast_mut();
                (*av_frame).linesize[index] = *stride as c_int;
            }
            (*av_frame).pts = pts.as_nanos();
            (*av_frame).pict_type = if keyframe {
                av::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                av::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            (*av_frame).colorspace = av::AVColorSpace::AVCOL_SPC_BT709;
            (*av_frame).color_range = av::AVColorRange::AVCOL_RANGE_MPEG;
            let send = av::avcodec_send_frame(self.ctx, av_frame);
            if send != 0 {
                let failure = format!("submitting frame at {pts:?}: {}", av_error(send));
                record_failure(output, failure);
                return Err(anyhow!("H.264 encoder rejected a frame"));
            }
            loop {
                let take = av::avcodec_receive_packet(self.ctx, self.packet);
                if take == AV_EAGAIN {
                    break;
                }
                if take == av::AVERROR_EOF as c_int {
                    break;
                }
                if take != 0 {
                    let failure = format!("taking an encoded frame: {}", av_error(take));
                    record_failure(output, failure);
                    return Err(anyhow!("H.264 encoder stopped handing out frames"));
                }
                let encoded = convert_packet(self.packet);
                av::av_packet_unref(self.packet);
                let Some(encoded) = encoded else {
                    continue;
                };
                output.push(encoded);
            }
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            av::av_frame_free(&mut self.frame);
            av::av_packet_free(&mut self.packet);
            av::avcodec_free_context(&mut self.ctx);
        }
    }
}

/// Rec. 709 video range, the engine's Y'CbCr convention.
fn attr_color(ctx: &mut av::AVCodecContext) {
    ctx.colorspace = av::AVColorSpace::AVCOL_SPC_BT709;
    ctx.color_primaries = av::AVColorPrimaries::AVCOL_PRI_BT709;
    ctx.color_trc = av::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
    ctx.color_range = av::AVColorRange::AVCOL_RANGE_MPEG;
}

/// The pixel formats `h264_nvenc` takes, from libavcodec 9's supported
/// config (the per-codec lists replaced the codec's own fields).
unsafe fn supported_formats(codec: *const av::AVCodec) -> Result<&'static [av::AVPixelFormat]> {
    unsafe {
        let mut out: *const c_void = std::ptr::null();
        let mut count: c_int = 0;
        let result = av::avcodec_get_supported_config(
            std::ptr::null(),
            codec,
            av::AVCodecConfig::AV_CODEC_CONFIG_PIX_FORMAT,
            0,
            &mut out,
            &mut count,
        );
        if result != 0 {
            bail!(
                "asking libavcodec for h264_nvenc's pixel formats: {}",
                av_error(result)
            );
        }
        if out.is_null() || count == 0 {
            return Ok(&[]);
        }
        Ok(std::slice::from_raw_parts(out.cast(), count as usize))
    }
}

fn format_name(format: av::AVPixelFormat) -> String {
    let name = unsafe { av::av_get_pix_fmt_name(format) };
    if name.is_null() {
        format!("{format:?}")
    } else {
        unsafe {
            std::ffi::CStr::from_ptr(name)
                .to_string_lossy()
                .into_owned()
        }
    }
}

/// An encoded packet into an [`EncodedVideo`]: Annex B NAL units into
/// the AVC file format's length-prefixed layout, SPS and PPS taken off
/// keyframes (nvenc's extradata is empty at open; keyframes carry the
/// sets, which is the contract's `parameter_sets` anyway).
unsafe fn convert_packet(packet: *const av::AVPacket) -> Option<EncodedVideo> {
    unsafe {
        let payload = std::slice::from_raw_parts((*packet).data, (*packet).size as usize);
        let units = nal_units(payload)?;
        let keyframe = (*packet).flags & av::AV_PKT_FLAG_KEY as c_int != 0
            && units
                .iter()
                .any(|unit| unit.first().is_some_and(|header| header & 0x1f == 7));
        let pts = if (*packet).pts == av::AV_NOPTS_VALUE {
            (*packet).dts
        } else {
            (*packet).pts
        };
        let dts = if (*packet).dts == av::AV_NOPTS_VALUE {
            pts
        } else {
            (*packet).dts
        };
        Some(EncodedVideo {
            data: avc_layout(&units),
            pts: Time::from_nanos(pts),
            dts: Time::from_nanos(dts),
            keyframe,
            parameter_sets: if keyframe {
                parameter_sets(&units)
            } else {
                None
            },
        })
    }
}

/// An AAC-LC encoder through libavcodec's native `aac` encoder: 48 kHz
/// interleaved floats in, raw AAC frames of 1024 samples per channel
/// out. The engine builds the FLV decoder configuration itself, so the
/// packets need no ADTS header - the same contract as macOS's
/// AudioToolbox encoder.
/// Samples per channel in one AAC frame, a fallback if the encoder
/// downplays none.
const AAC_FRAME: usize = 1024;

pub struct LiveAacEncoder {
    channels: usize,
    /// Samples per channel in one AAC frame, read from the encoder at
    /// open.
    frame_size: usize,
    /// Samples the encoder consumes before its first output frame.
    delay: u32,
    ctx: *mut av::AVCodecContext,
    frame: *mut av::AVFrame,
    packet: *mut av::AVPacket,
    /// Samples given to `encode` and not yet a whole frame.
    pending: Vec<f32>,
    /// One plane per channel, reused for each frame.
    planar: Vec<Vec<f32>>,
    /// Samples handed in so far, per channel: the frames' pts.
    samples: u64,
}

unsafe impl Send for LiveAacEncoder {}

impl LiveAacEncoder {
    pub fn new(spec: &LiveAudioSpec) -> Result<Self> {
        let channels = spec.channels.clamp(1, 2);
        unsafe {
            let codec = av::avcodec_find_encoder(av::AVCodecID::AV_CODEC_ID_AAC);
            if codec.is_null() {
                bail!("libavcodec has no AAC encoder");
            }
            let formats = supported_sample_formats(codec)?;
            if !formats.contains(&av::AVSampleFormat::AV_SAMPLE_FMT_FLTP) {
                let names = formats
                    .iter()
                    .filter_map(|format| {
                        let name = av::av_get_sample_fmt_name(*format);
                        (!name.is_null()).then(|| {
                            std::ffi::CStr::from_ptr(name)
                                .to_string_lossy()
                                .into_owned()
                        })
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("the AAC encoder takes {names} as samples, not float");
            }
            let mut ctx = av::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                bail!("libavcodec gave no AAC encoder context");
            }
            (*ctx).sample_rate = SAMPLE_RATE as c_int;
            (*ctx).sample_fmt = av::AVSampleFormat::AV_SAMPLE_FMT_FLTP;
            av::av_channel_layout_default(&mut (*ctx).ch_layout, channels as c_int);
            (*ctx).bit_rate = i64::from(spec.bitrate_kbps) * 1000;
            let result = av::avcodec_open2(ctx, codec, std::ptr::null_mut());
            if result != 0 {
                av::avcodec_free_context(&mut ctx);
                bail!("opening the AAC encoder: {}", av_error(result));
            }
            let frame_size = usize::try_from((*ctx).frame_size)
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(AAC_FRAME);
            let delay = (*ctx).initial_padding.max(0) as u32;
            let mut frame = av::av_frame_alloc();
            let mut packet = av::av_packet_alloc();
            if frame.is_null() || packet.is_null() {
                av::av_frame_free(&mut frame);
                av::av_packet_free(&mut packet);
                av::avcodec_free_context(&mut ctx);
                bail!("libavcodec gave no AAC frame or packet");
            }
            (*frame).format = (*ctx).sample_fmt as c_int;
            (*frame).sample_rate = (*ctx).sample_rate;
            av::av_channel_layout_copy(&mut (*frame).ch_layout, &(*ctx).ch_layout);
            let mut planar = Vec::with_capacity(channels as usize);
            for _ in 0..channels as usize {
                planar.push(Vec::with_capacity(frame_size));
            }
            Ok(Self {
                channels: channels as usize,
                frame_size,
                delay,
                ctx,
                frame,
                packet,
                pending: Vec::new(),
                planar,
                samples: 0,
            })
        }
    }

    /// Send one whole frame (its samples sit in `planar`) and take
    /// every packet the encoder answers with.
    unsafe fn submit_frame(&mut self) -> Result<Vec<Vec<u8>>> {
        unsafe {
            (*self.frame).nb_samples = self.frame_size as c_int;
            (*self.frame).pts = self.samples as i64;
            self.samples += self.frame_size as u64;
            for (plane, data) in self.planar.iter().enumerate() {
                (*self.frame).data[plane] = data.as_ptr().cast_mut().cast::<u8>();
                (*self.frame).linesize[plane] = (data.len() * std::mem::size_of::<f32>()) as c_int;
            }
            let send = av::avcodec_send_frame(self.ctx, self.frame);
            if send != 0 {
                return Err(anyhow!(
                    "the AAC encoder rejected a frame: {}",
                    av_error(send)
                ));
            }
            self.take_packets()
        }
    }

    /// Samples per channel in one AAC frame.
    pub(crate) fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Everything the encoder still holds once no more samples come:
    /// its last frames, whose pts keep the feed's sample count. The
    /// join's tail padding has made the feed end on a whole frame by
    /// the time this runs.
    pub fn flush(&mut self) -> Result<Vec<Vec<u8>>> {
        unsafe {
            let send = av::avcodec_send_frame(self.ctx, std::ptr::null());
            if send != 0 && send != AV_EAGAIN {
                return Err(anyhow!(
                    "the AAC encoder did not finish: {}",
                    av_error(send)
                ));
            }
            self.take_packets()
        }
    }

    /// The encoder's raw context, for a muxer copying codec parameters
    /// into a file header it writes.
    pub(crate) fn codec_context(&self) -> *mut av::AVCodecContext {
        self.ctx
    }

    /// Every packet the encoder answers with, buffers drained.
    unsafe fn take_packets(&mut self) -> Result<Vec<Vec<u8>>> {
        unsafe {
            let mut frames = Vec::new();
            loop {
                let take = av::avcodec_receive_packet(self.ctx, self.packet);
                if take == AV_EAGAIN || take == AV_EOF {
                    // EAGAIN: more input wanted; EOF: the flush has
                    // given up everything.
                    break;
                }
                if take != 0 {
                    self.discard_packet();
                    return Err(anyhow!("the AAC encoder stopped: {}", av_error(take)));
                }
                let size = (*self.packet).size.max(0) as usize;
                let bytes = std::slice::from_raw_parts((*self.packet).data.cast::<u8>(), size);
                frames.push(bytes.to_vec());
                self.discard_packet();
            }
            Ok(frames)
        }
    }

    /// Let the packet's buffers go for reuse.
    unsafe fn discard_packet(&mut self) {
        unsafe {
            av::av_packet_unref(self.packet);
            (*self.packet).size = 0;
            (*self.packet).data = std::ptr::null_mut();
        }
    }
}

impl Drop for LiveAacEncoder {
    fn drop(&mut self) {
        unsafe {
            av::av_frame_free(&mut self.frame);
            av::av_packet_free(&mut self.packet);
            av::avcodec_free_context(&mut self.ctx);
        }
    }
}

impl AudioEncoder for LiveAacEncoder {
    fn encode(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>> {
        self.pending.extend_from_slice(samples);
        let whole = self.frame_size * self.channels;
        let mut frames = Vec::new();
        while self.pending.len() >= whole {
            // Interleaved float into planar float, one plane a channel.
            for plane in self.planar.iter_mut() {
                plane.clear();
            }
            let show = &self.pending[..whole];
            for frame in 0..self.frame_size {
                for (channel, plane) in self.planar.iter_mut().enumerate() {
                    plane.push(show[frame * self.channels + channel]);
                }
            }
            frames.extend_from_slice(&unsafe { self.submit_frame() }?);
            self.pending.drain(..whole);
        }
        Ok(frames)
    }

    fn delay(&self) -> u32 {
        self.delay
    }
}

/// A codec's supported sample formats.
unsafe fn supported_sample_formats(
    codec: *const av::AVCodec,
) -> Result<&'static [av::AVSampleFormat]> {
    unsafe {
        let mut out: *const c_void = std::ptr::null();
        let mut count: c_int = 0;
        let result = av::avcodec_get_supported_config(
            std::ptr::null(),
            codec,
            av::AVCodecConfig::AV_CODEC_CONFIG_SAMPLE_FORMAT,
            0,
            &mut out,
            &mut count,
        );
        if result != 0 {
            bail!(
                "asking libavcodec for the AAC sample formats: {}",
                av_error(result)
            );
        }
        if out.is_null() || count == 0 {
            return Ok(&[]);
        }
        Ok(std::slice::from_raw_parts(out.cast(), count as usize))
    }
}
