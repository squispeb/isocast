//! A source's live picture in Properties and Filters (the web app's
//! SourcePreview): a thread of its own receives the source's newest frame
//! and the window shows it. It stops when the dialog closes.
//!
//! Two transports, chosen by platform, as in [`crate::frames`]:
//! - macOS: `shared_frames` with the source as its target, a small ring of
//!   shared GPU surfaces, shown on the window's own device without copying.
//! - Everywhere else: the daemon's JPEG `frame` method, decoded to pixels
//!   (the no-copy path for sources is what `docs/port-omarchy.md`'s L.4
//!   brings to Linux). That picture is the source's own, not the canvas's,
//!   so a filter's effect shows on the canvas until L.4 carries filtered
//!   frames here too.

use std::cell::RefCell;
#[cfg(target_os = "macos")]
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(target_os = "macos")]
use isocast_client::shared::SharedFrame;
#[cfg(target_os = "macos")]
use isocast_protocol::api::SharedFramesParams;
#[cfg(not(target_os = "macos"))]
use isocast_protocol::api::{Frame, FrameParams, Method};
use slint::wgpu_30::wgpu;

#[cfg(not(target_os = "macos"))]
use crate::frames::Picture;

/// macOS: the shared surface the daemon drew.
#[cfg(target_os = "macos")]
type Received = SharedFrame;
/// Everywhere else: the daemon's JPEG, decoded.
#[cfg(not(target_os = "macos"))]
type Received = Picture;

type Slot = Arc<Mutex<Option<Received>>>;

struct Watching {
    source: String,
    stop: Arc<AtomicBool>,
    newest: Slot,
    /// macOS: the images made from the ring's surfaces (one per slot).
    #[cfg(target_os = "macos")]
    images: HashMap<(u64, u32), slint::Image>,
    /// The frames shown now and just before (the GPU may still read them).
    #[cfg(target_os = "macos")]
    showing: Option<Received>,
    #[cfg(target_os = "macos")]
    previous: Option<Received>,
}

#[derive(Default)]
pub struct SourcePreview {
    /// The window's GPU device: the shared surfaces are its textures
    /// (macOS only; the JPEG path needs no device).
    #[cfg(target_os = "macos")]
    device: RefCell<Option<wgpu::Device>>,
    watching: Rc<RefCell<Option<Watching>>>,
    timer: slint::Timer,
}

impl SourcePreview {
    /// The GPU device the window draws with (the pictures are its
    /// textures).
    pub fn set_device(&self, device: wgpu::Device) {
        #[cfg(target_os = "macos")]
        {
            *self.device.borrow_mut() = Some(device);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = device;
    }

    /// Show `source`'s picture through `show` (each new frame's image).
    pub fn start(&self, source: &str, show: impl Fn(slint::Image, f32) + 'static) {
        if self
            .watching
            .borrow()
            .as_ref()
            .is_some_and(|w| w.source == source)
        {
            return;
        }
        self.stop();
        let stop = Arc::new(AtomicBool::new(false));
        let newest: Slot = Arc::default();
        {
            let (stop, newest, target) = (stop.clone(), newest.clone(), source.to_string());
            let _ = thread::Builder::new()
                .name("source preview".into())
                .spawn(move || receive(&target, &stop, &newest));
        }
        *self.watching.borrow_mut() = Some(Watching {
            source: source.to_string(),
            stop,
            newest,
            #[cfg(target_os = "macos")]
            images: HashMap::new(),
            #[cfg(target_os = "macos")]
            showing: None,
            #[cfg(target_os = "macos")]
            previous: None,
        });
        let watching = self.watching.clone();
        #[cfg(target_os = "macos")]
        {
            let Some(device) = self.device.borrow().clone() else {
                return;
            };
            self.timer.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(16),
                move || tick_shared(&watching, &device, &show),
            );
        }
        #[cfg(not(target_os = "macos"))]
        self.timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || tick_jpeg(&watching, &show),
        );
    }

    /// Stop showing a picture.
    pub fn stop(&self) {
        self.timer.stop();
        if let Some(w) = self.watching.borrow_mut().take() {
            w.stop.store(true, Ordering::Relaxed);
        }
    }

    pub fn source(&self) -> Option<String> {
        self.watching.borrow().as_ref().map(|w| w.source.clone())
    }
}

/// macOS: show the newest shared frame, if a new one came.
#[cfg(target_os = "macos")]
fn tick_shared(
    watching: &RefCell<Option<Watching>>,
    device: &wgpu::Device,
    show: &dyn Fn(slint::Image, f32),
) {
    {
        let mut watching = watching.borrow_mut();
        let Some(w) = watching.as_mut() else { return };
        let Some(frame) = w.newest.lock().ok().and_then(|mut f| f.take()) else {
            return;
        };
        let key = frame.slot();
        if !w.images.contains_key(&key) {
            // A new ring: the old one's images go.
            w.images.retain(|(generation, _), _| *generation == key.0);
            match frame
                .texture(device)
                .and_then(|t| slint::Image::try_from(t).map_err(Into::into))
            {
                Ok(image) => {
                    w.images.insert(key, image);
                }
                Err(error) => {
                    isocast_diagnostics::diagnostic!("warn";
                        "source preview: can't show a shared frame: {error:#}"
                    );
                    return;
                }
            }
        }
        let aspect = frame.width() as f32 / frame.height().max(1) as f32;
        show(w.images[&key].clone(), aspect);
        w.previous = w.showing.replace(frame);
    }
}

/// Everywhere but macOS: show the newest decoded picture, if one came.
#[cfg(not(target_os = "macos"))]
fn tick_jpeg(watching: &RefCell<Option<Watching>>, show: &dyn Fn(slint::Image, f32)) {
    let mut watching = watching.borrow_mut();
    let Some(w) = watching.as_mut() else { return };
    let Some(picture) = w.newest.lock().ok().and_then(|mut f| f.take()) else {
        return;
    };
    let aspect = picture.width as f32 / picture.height.max(1) as f32;
    let image = slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
        &picture.rgba,
        picture.width.max(1),
        picture.height.max(1),
    ));
    show(image, aspect);
}

/// The receiving thread: the newest frame into `newest`, until stopped.
#[cfg(target_os = "macos")]
fn receive(target: &str, stop: &AtomicBool, newest: &Slot) {
    let mut delay = Duration::from_millis(250);
    while !stop.load(Ordering::Relaxed) {
        let result = crate::daemon::connect().and_then(|client| {
            let mut frames = client.shared_frames(SharedFramesParams {
                target: target.into(),
                max_width: Some(1280),
                max_height: Some(720),
                fps: Some(30),
                slots: None,
                // As the program shows it: Filters' preview shows each
                // change, Properties' the source as it will look.
                filtered: true,
            })?;
            while !stop.load(Ordering::Relaxed) {
                if let Some(frame) = frames.newest(Some(Duration::from_millis(500)))?
                    && let Ok(mut slot) = newest.lock()
                {
                    *slot = Some(frame);
                }
            }
            Ok(())
        });
        if result.is_ok() {
            break;
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

/// The receiving thread, everywhere but macOS: the daemon's JPEG pictures,
/// asked for again as each arrives (the daemon waits for one newer than
/// `after`, so this is live, and at most 30 a second).
#[cfg(not(target_os = "macos"))]
fn receive(target: &str, stop: &AtomicBool, newest: &Slot) {
    use base64::Engine;

    let mut delay = Duration::from_millis(250);
    while !stop.load(Ordering::Relaxed) {
        let result = crate::daemon::connect().and_then(|mut client| {
            let mut after: Option<u64> = None;
            while !stop.load(Ordering::Relaxed) {
                let params = FrameParams {
                    target: target.to_string(),
                    max_width: Some(1280),
                    after,
                    timeout_ms: Some(500),
                    quality: Some(0.75),
                    fps: Some(30),
                };
                let reply: Frame = match client.call(Method::Frame(params)) {
                    Ok(reply) => reply,
                    // Nothing new within the wait: ask again.
                    Err(error) if format!("{error:#}").contains("no new frame") => continue,
                    Err(error) => return Err(error),
                };
                after = Some(reply.seq);
                let jpeg = base64::engine::general_purpose::STANDARD
                    .decode(&reply.jpeg_base64)
                    .map_err(|error| {
                        anyhow::anyhow!("the daemon's picture isn't base64: {error}")
                    })?;
                let decoded = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
                    .map_err(|error| {
                        anyhow::anyhow!("the daemon's picture isn't a JPEG: {error}")
                    })?;
                let rgba = decoded.to_rgba8();
                if let Ok(mut slot) = newest.lock() {
                    *slot = Some(Picture {
                        width: rgba.width(),
                        height: rgba.height(),
                        rgba: rgba.into_raw(),
                        revision: reply.revision,
                    });
                }
            }
            Ok(())
        });
        if result.is_ok() {
            break;
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}
