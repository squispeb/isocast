//! Real-time encoders for streaming and live monitors: L.5's
//! in-process NVENC/libav sessions, not this crate's L.0 ffmpeg pipes.
//! A pipe can't honour the `VideoEncoder` contract — `receive` must
//! hand back each encoded frame the moment it's done, keyed to the
//! encoder's own pacing — so until the real encoders arrive, these
//! types are uninhabited: `new` says which milestone brings them, and
//! every other method is unreachable, because the engine can only
//! hold one of these after `Platform::create_video_encoder` or
//! `create_audio_encoder`, which always fail first.

use std::time::Duration;

use anyhow::Result;
use isocast_engine::{
    AudioEncoder, EncodedVideo, LiveAudioSpec, LiveVideoSpec, Time, VideoEncoder,
};

use crate::frames::LinuxFrame;

/// A real-time H.264 encoder, for streaming and monitors.
pub enum LiveH264Encoder {}

impl LiveH264Encoder {
    pub fn new(spec: &LiveVideoSpec) -> Result<Self> {
        let _ = spec;
        anyhow::bail!(
            "real-time H.264 encoders arrive at L.5 of docs/port-omarchy.md (in-process \
             NVENC/libav; an ffmpeg pipe cannot honour the VideoEncoder contract)"
        )
    }
}

/// A real-time AAC-LC encoder, for streaming.
pub enum LiveAacEncoder {}

impl LiveAacEncoder {
    pub fn new(spec: &LiveAudioSpec) -> Result<Self> {
        let _ = spec;
        anyhow::bail!(
            "real-time AAC encoders arrive at L.5 of docs/port-omarchy.md (in-process \
             libav; an ffmpeg pipe cannot honour the AudioEncoder contract)"
        )
    }
}

impl VideoEncoder for LiveH264Encoder {
    type Frame = LinuxFrame;

    /// Uninhabited: only `Platform::create_video_encoder` can make
    /// one of these, and it always fails first.
    fn encode(&mut self, _frame: &LinuxFrame, _pts: Time, _keyframe: bool) -> Result<()> {
        match *self {}
    }

    /// Uninhabited: only `Platform::create_video_encoder` can make
    /// one of these, and it always fails first.
    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        match *self {}
    }

    /// Uninhabited: only `Platform::create_video_encoder` can make
    /// one of these, and it always fails first.
    fn receive_timeout(&mut self, _timeout: Duration) -> Result<Option<EncodedVideo>> {
        match *self {}
    }

    /// Uninhabited: only `Platform::create_video_encoder` can make
    /// one of these, and it always fails first.
    fn dropped(&self) -> u64 {
        match *self {}
    }

    /// Uninhabited: only `Platform::create_video_encoder` can make
    /// one of these, and it always fails first.
    fn set_rate(&mut self, _fps: u32, _bitrate_kbps: u32) -> Result<()> {
        match *self {}
    }
}

impl AudioEncoder for LiveAacEncoder {
    /// Uninhabited: only `Platform::create_audio_encoder` can make
    /// one of these, and it always fails first.
    fn encode(&mut self, _samples: &[f32]) -> Result<Vec<Vec<u8>>> {
        match *self {}
    }

    /// Uninhabited: only `Platform::create_audio_encoder` can make
    /// one of these, and it always fails first.
    fn delay(&self) -> u32 {
        match *self {}
    }
}
