//! Provider order is explicit and observable: NVENC, hardware MFT, software
//! MFT last. This does not select the Windows platform in the daemon yet.

use std::time::Duration;

use anyhow::{Result, bail};
use isocast_engine::{EncodedVideo, LiveVideoSpec, ParameterSets, Time, VideoEncoder};

use crate::{EncodeStats, Frame, GraphicsDevice, MfEncoder, MfMode, NvencEncoder, PixelFormat};
use crate::{h264, hevc};

/// SDR Rec.709 encoding. Main10 preserves a ten-bit input, or expands an
/// eight-bit input on GPU; it does not turn SDR/eight-bit capture into HDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEncoding {
    H264,
    Hevc,
    Hevc10,
}

impl VideoEncoding {
    pub fn bit_depth(self) -> u8 {
        if self == Self::Hevc10 { 10 } else { 8 }
    }

    pub(crate) fn validate(self, input: PixelFormat) -> Result<()> {
        if input == PixelFormat::Rgba10 && self != Self::Hevc10 {
            bail!("video.input: ten-bit RGB requires HEVC Main10; no silent bit-depth reduction");
        }
        Ok(())
    }
}

pub(crate) enum Parameters {
    H264(ParameterSets),
    Hevc { depth: u8, sets: hevc::Parameters },
}

impl Parameters {
    pub(crate) fn new(encoding: VideoEncoding) -> Self {
        match encoding {
            VideoEncoding::H264 => Self::H264(ParameterSets::default()),
            _ => Self::Hevc {
                depth: encoding.bit_depth(),
                sets: hevc::Parameters::default(),
            },
        }
    }

    pub(crate) fn headers(&mut self, bytes: &[u8]) -> Result<()> {
        let nals = h264::annex_b(bytes)?;
        match self {
            Self::H264(sets) => *sets = h264::parameters(&nals),
            Self::Hevc { sets, .. } => {
                let next = hevc::parameters(&nals)?;
                if !sets.vps.is_empty()
                    && !sets.sps.is_empty()
                    && !sets.pps.is_empty()
                    && *sets != next
                {
                    bail!("HEVC parameter sets changed; open a new hvc1 track");
                }
                *sets = next;
            }
        }
        self.config()?;
        Ok(())
    }

    pub(crate) fn config(&self) -> Result<Vec<u8>> {
        match self {
            Self::H264(sets) => h264::config(sets),
            Self::Hevc { depth, sets } => {
                if sets.bit_depth()? != *depth {
                    bail!("encoder returned HEVC with a different bit depth than requested");
                }
                hevc::config(sets)
            }
        }
    }

    pub(crate) fn packet(&mut self, bytes: &[u8], pts: Time) -> Result<Option<EncodedVideo>> {
        match self {
            Self::H264(sets) => h264::packet(bytes, pts, sets),
            Self::Hevc { sets, .. } => hevc::packet(bytes, pts, sets),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderBackend {
    Nvenc,
    MediaFoundationHardware(String),
    /// This provider downloads NV12/P010 for a CPU encoder. Never call it a
    /// zero-copy or hardware encoding path in diagnostics.
    MediaFoundationSoftware(String),
}

enum Encoder {
    Nvenc(NvencEncoder),
    Mf(MfEncoder),
}

pub struct WindowsVideoEncoder {
    encoder: Encoder,
    backend: EncoderBackend,
    fallback_reasons: Vec<String>,
    encoding: VideoEncoding,
}

impl WindowsVideoEncoder {
    pub fn new(device: &GraphicsDevice, spec: &LiveVideoSpec) -> Result<Self> {
        Self::with_format(device, spec, VideoEncoding::H264, PixelFormat::Bgra8)
    }

    pub fn with_format(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        encoding: VideoEncoding,
        input: PixelFormat,
    ) -> Result<Self> {
        encoding.validate(input)?;
        let mut reasons = Vec::new();
        match NvencEncoder::with_format(device, spec, encoding, input) {
            Ok(encoder) => {
                return Ok(Self {
                    encoder: Encoder::Nvenc(encoder),
                    backend: EncoderBackend::Nvenc,
                    fallback_reasons: reasons,
                    encoding,
                });
            }
            Err(error) => reasons.push(format!("NVENC: {error:#}")),
        }
        for mode in [MfMode::Hardware, MfMode::Software] {
            match MfEncoder::with_format(device, spec, mode, encoding, input) {
                Ok(encoder) => {
                    let name = encoder.info().name.clone();
                    let backend = match mode {
                        MfMode::Hardware => EncoderBackend::MediaFoundationHardware(name),
                        MfMode::Software => EncoderBackend::MediaFoundationSoftware(name),
                    };
                    return Ok(Self {
                        encoder: Encoder::Mf(encoder),
                        backend,
                        fallback_reasons: reasons,
                        encoding,
                    });
                }
                Err(error) => reasons.push(format!("MF {mode:?}: {error:#}")),
            }
        }
        bail!(
            "encoder: no {encoding:?} provider could open: {}",
            reasons.join("; ")
        )
    }

    pub fn backend(&self) -> &EncoderBackend {
        &self.backend
    }
    pub fn encoding(&self) -> VideoEncoding {
        self.encoding
    }
    pub fn fallback_reasons(&self) -> &[String] {
        &self.fallback_reasons
    }
    pub fn decoder_config(&self) -> Result<Vec<u8>> {
        match &self.encoder {
            Encoder::Nvenc(encoder) => encoder.decoder_config(),
            Encoder::Mf(encoder) => encoder.decoder_config(),
        }
    }
    pub fn try_encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<bool> {
        match &mut self.encoder {
            Encoder::Nvenc(encoder) => encoder.try_encode(frame, pts, keyframe),
            Encoder::Mf(encoder) => encoder.try_encode(frame, pts, keyframe),
        }
    }
    pub fn finish(&mut self) -> Result<Vec<EncodedVideo>> {
        match &mut self.encoder {
            Encoder::Nvenc(encoder) => encoder.finish(),
            Encoder::Mf(encoder) => encoder.finish(),
        }
    }
    pub fn stats(&self) -> Result<EncodeStats> {
        match &self.encoder {
            Encoder::Nvenc(encoder) => Ok(encoder.stats()),
            Encoder::Mf(encoder) => encoder.stats(),
        }
    }
}

impl VideoEncoder for WindowsVideoEncoder {
    type Frame = Frame;
    fn prioritize_video(&mut self, canvas: isocast_protocol::CanvasId) {
        match &mut self.encoder {
            Encoder::Nvenc(encoder) => encoder.prioritize_video(canvas),
            Encoder::Mf(encoder) => encoder.prioritize_video(canvas),
        }
    }
    fn encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<()> {
        match &mut self.encoder {
            Encoder::Nvenc(encoder) => encoder.encode(frame, pts, keyframe),
            Encoder::Mf(encoder) => encoder.encode(frame, pts, keyframe),
        }
    }
    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        self.receive_timeout(Duration::ZERO)
    }
    fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<EncodedVideo>> {
        match &mut self.encoder {
            Encoder::Nvenc(encoder) => encoder.receive_timeout(timeout),
            Encoder::Mf(encoder) => encoder.receive_timeout(timeout),
        }
    }
    fn dropped(&self) -> u64 {
        match &self.encoder {
            Encoder::Nvenc(encoder) => encoder.dropped(),
            Encoder::Mf(encoder) => encoder.dropped(),
        }
    }
    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) -> Result<()> {
        match &mut self.encoder {
            Encoder::Nvenc(encoder) => encoder.set_rate(fps, bitrate_kbps),
            Encoder::Mf(encoder) => encoder.set_rate(fps, bitrate_kbps),
        }
    }
}
