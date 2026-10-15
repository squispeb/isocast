//! A monitor's picture: a thread of its own receives each frame and hands
//! it to the window's thread. Reconnects with back-off when the daemon goes
//! away, and at once when the target changes (the edited monitor shows
//! `preview` in Studio Mode, `program` otherwise).
//!
//! Two transports, chosen by platform:
//! - macOS: `shared_frames`, a GPU surface the daemon has finished drawing —
//!   the picture without copying it.
//! - Everywhere else: the daemon's JPEG `frame` method, asked again for
//!   each new frame (`after` is the last `seq`, so the daemon waits for a
//!   newer one and this is live video rather than a poll of a stale
//!   picture), decoded to pixels Slint shows. The no-copy path is what
//!   `docs/port-omarchy.md`'s L.4 brings to Linux; until then this is the
//!   picture, at the cost of one encode and one decode a frame.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use isocast_client::shared::{SharedController, SharedFrame};
#[cfg(target_os = "macos")]
use isocast_protocol::api::SharedFramesParams;
#[cfg(not(target_os = "macos"))]
use isocast_protocol::api::{Frame, FrameParams, Method};

/// A JPEG picture from the daemon, decoded: the portable monitor path.
/// (macOS gets [`SharedFrame`]s instead, which never copy the pixels.)
#[derive(Clone)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// RGBA, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    /// For the program and the preview: the setup revision the picture
    /// was drawn from, for drawing marks over it.
    pub revision: Option<u64>,
}

/// What the window gets.
pub enum Incoming {
    /// macOS: a shared GPU surface (never constructed elsewhere).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Frame(SharedFrame),
    Picture(Picture),
    /// No picture: why (the daemon can't be reached, or refused).
    Lost(String),
}

/// What the window wants: the target (none: nothing), the picture's size
/// in pixels and the rate.
#[derive(Debug, Clone, PartialEq)]
struct Want {
    target: Option<String>,
    width: u32,
    height: u32,
    fps: u32,
}

/// The receiving thread's handle: change the target, the size or the rate.
#[derive(Clone)]
pub struct Feed {
    want: Arc<Mutex<Want>>,
    control: Arc<Mutex<Option<SharedController>>>,
}

impl Feed {
    /// Start receiving `target`'s picture (none: wait for one), fit in
    /// `width`×`height` pixels at `fps`. `deliver` runs on the receiving
    /// thread.
    pub fn start(
        target: Option<&str>,
        (width, height): (u32, u32),
        fps: u32,
        deliver: impl Fn(Incoming) + Send + 'static,
    ) -> Self {
        let feed = Self {
            want: Arc::new(Mutex::new(Want {
                target: target.map(str::to_string),
                width,
                height,
                fps,
            })),
            control: Arc::default(),
        };
        let thread_feed = feed.clone();
        thread::Builder::new()
            .name("shared frames".into())
            .spawn(move || thread_feed.receive(deliver))
            .expect("spawn a thread");
        feed
    }

    /// Show `target` instead (none: nothing).
    pub fn set_target(&self, target: Option<&str>) {
        let mut want = self.want.lock().unwrap();
        if want.target.as_deref() == target {
            return;
        }
        // The open stream ends at its next frame (or within 250 ms).
        want.target = target.map(str::to_string);
    }

    /// Fit the picture in `width`×`height` pixels (a new ring of surfaces
    /// follows when the size changes).
    pub fn set_size(&self, width: u32, height: u32) {
        let mut want = self.want.lock().unwrap();
        if (want.width, want.height) == (width, height) {
            return;
        }
        want.width = width;
        want.height = height;
        if let Some(control) = self.control.lock().unwrap().as_ref() {
            let _ = control.set_size(Some(width), Some(height));
        }
    }

    /// At most `fps` frames a second (1 while the window is hidden).
    pub fn set_fps(&self, fps: u32) {
        let mut want = self.want.lock().unwrap();
        if want.fps == fps {
            return;
        }
        want.fps = fps;
        if let Some(control) = self.control.lock().unwrap().as_ref() {
            let _ = control.set_fps(fps);
        }
    }

    fn receive(&self, deliver: impl Fn(Incoming)) {
        #[cfg(target_os = "macos")]
        self.receive_shared(deliver);
        #[cfg(not(target_os = "macos"))]
        self.receive_jpeg(deliver);
    }

    /// macOS: the daemon's shared GPU surfaces.
    #[cfg(target_os = "macos")]
    fn receive_shared(&self, deliver: impl Fn(Incoming)) {
        let mut delay = Duration::from_millis(250);
        loop {
            let want = self.want.lock().unwrap().clone();
            let Some(target) = want.target.clone() else {
                thread::sleep(Duration::from_millis(50));
                continue;
            };
            let stream = crate::daemon::connect().and_then(|client| {
                client.shared_frames(SharedFramesParams {
                    target: target.clone(),
                    max_width: Some(want.width.max(16)),
                    max_height: Some(want.height.max(16)),
                    fps: Some(want.fps),
                    slots: None,
                    filtered: false,
                })
            });
            let result: anyhow::Result<()> = stream.and_then(|mut frames| {
                *self.control.lock().unwrap() = Some(frames.controller());
                // A change asked for while connecting.
                let now = self.want.lock().unwrap().clone();
                if (now.width, now.height, now.fps) != (want.width, want.height, want.fps) {
                    let control = frames.controller();
                    control.set_size(Some(now.width), Some(now.height))?;
                    control.set_fps(now.fps)?;
                }
                delay = Duration::from_millis(250);
                loop {
                    if self.want.lock().unwrap().target.as_deref() != Some(target.as_str()) {
                        return Ok(());
                    }
                    // The newest frame: older ones that arrived meanwhile
                    // go straight back to the daemon.
                    if let Some(frame) = frames.newest(Some(Duration::from_millis(250)))? {
                        deliver(Incoming::Frame(frame));
                    }
                }
            });
            *self.control.lock().unwrap() = None;
            match result {
                // The target changed: the new one at once.
                Ok(()) => continue,
                Err(error) => deliver(Incoming::Lost(format!("{error:#}"))),
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }

    /// Everywhere but macOS: the daemon's JPEG frames, asked for again as
    /// each arrives. The daemon waits for one newer than `after`, so the
    /// rate is the program's own (up to what this monitor asks for).
    #[cfg(not(target_os = "macos"))]
    fn receive_jpeg(&self, deliver: impl Fn(Incoming)) {
        let mut delay = Duration::from_millis(250);
        loop {
            let want = self.want.lock().unwrap().clone();
            let Some(target) = want.target.clone() else {
                thread::sleep(Duration::from_millis(50));
                continue;
            };
            let result: anyhow::Result<()> = self.stream_jpeg(&target, &deliver);
            match result {
                // The target changed: the new one at once.
                Ok(()) => continue,
                Err(error) => deliver(Incoming::Lost(format!("{error:#}"))),
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }

    /// One target's frames until it changes (`Ok`) or the stream fails.
    #[cfg(not(target_os = "macos"))]
    fn stream_jpeg(&self, target: &str, deliver: &dyn Fn(Incoming)) -> anyhow::Result<()> {
        use base64::Engine;

        let mut client = crate::daemon::connect()?;
        let mut after: Option<u64> = None;
        loop {
            let want = self.want.lock().unwrap().clone();
            if want.target.as_deref() != Some(target) {
                return Ok(());
            }
            // A monitor's pixels, not the program's: a 1280x720 program in
            // a 400-pixel dock travels as a 400-pixel picture. At most 30 a
            // second: an interim path pays a JPEG encode and decode a frame,
            // and that is the rate this shows without loading the box.
            let params = FrameParams {
                target: target.to_string(),
                max_width: Some(want.width.max(16)),
                after,
                timeout_ms: Some(1_000),
                quality: Some(0.75),
                fps: Some(want.fps.clamp(1, 30)),
            };
            let reply: Frame = match client.call(Method::Frame(params)) {
                Ok(reply) => reply,
                // Nothing new within the wait: ask again; a monitor that has
                // been on a still scene for a while is not a lost stream.
                Err(error) if format!("{error:#}").contains("no new frame") => continue,
                Err(error) => return Err(error),
            };
            after = Some(reply.seq);
            let jpeg = base64::engine::general_purpose::STANDARD
                .decode(&reply.jpeg_base64)
                .map_err(|error| anyhow::anyhow!("the daemon's picture isn't base64: {error}"))?;
            let decoded = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
                .map_err(|error| anyhow::anyhow!("the daemon's picture isn't a JPEG: {error}"))?;
            let rgba = decoded.to_rgba8();
            deliver(Incoming::Picture(Picture {
                width: rgba.width(),
                height: rgba.height(),
                rgba: rgba.into_raw(),
                revision: reply.revision,
            }));
        }
    }
}
