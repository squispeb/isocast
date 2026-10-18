//! Provider order is explicit and observable: NVENC, hardware MFT, software
//! MFT last. This does not select the Windows platform in the daemon yet.

use std::time::Duration;

use anyhow::{Result, bail};
use isocast_engine::{EncodedVideo, LiveVideoSpec, Time, VideoEncoder};

use crate::{EncodeStats, Frame, GraphicsDevice, MfEncoder, MfMode, NvencEncoder};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderBackend {
    Nvenc,
    MediaFoundationHardware(String),
    /// This provider downloads NV12 for a CPU encoder. Never call it a
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
}

impl WindowsVideoEncoder {
    pub fn new(device: &GraphicsDevice, spec: &LiveVideoSpec) -> Result<Self> {
        let mut reasons = Vec::new();
        match NvencEncoder::new(device, spec) {
            Ok(encoder) => {
                return Ok(Self {
                    encoder: Encoder::Nvenc(encoder),
                    backend: EncoderBackend::Nvenc,
                    fallback_reasons: reasons,
                });
            }
            Err(error) => reasons.push(format!("NVENC: {error:#}")),
        }
        for mode in [MfMode::Hardware, MfMode::Software] {
            match MfEncoder::new(device, spec, mode) {
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
                    });
                }
                Err(error) => reasons.push(format!("MF {mode:?}: {error:#}")),
            }
        }
        bail!(
            "encoder: no H.264 provider could open: {}",
            reasons.join("; ")
        )
    }

    pub fn backend(&self) -> &EncoderBackend {
        &self.backend
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
