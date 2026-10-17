//! Windows native media. WGC delivers bounded, GPU-resident D3D11 frames
//! stamped on the system QPC clock. The daemon continues to select the stub
//! until the compositor, encoders and crash-safe writer are qualified.
#![cfg(windows)]

mod capture;
mod clock;
mod frames;
mod native;

pub use capture::{CaptureStats, CaptureTarget, DisplayCapture, capture_screen};
pub use clock::now;
pub use frames::{Frame, GraphicsDevice, MAX_POOL_FRAMES};
