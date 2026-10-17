//! A source's live picture in Properties and Filters (the web app's
//! SourcePreview): a thread of its own receives the source's newest frame
//! and the window shows it. It stops when the dialog closes.
//!
//! Two transports, chosen by platform, as in [`crate::frames`]:
//! - macOS and Linux: `shared_frames` with the source as its target, a
//!   small ring of shared GPU surfaces, shown on the window's own device
//!   without a JPEG detour (`filtered`, so a filter's effect shows).
//! - Everywhere else: the daemon's JPEG `frame` method, decoded to pixels.
//!   That picture is the source's own, not the canvas's, and unfiltered.

use std::cell::RefCell;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use isocast_client::shared::SharedFrame;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use isocast_protocol::api::SharedFramesParams;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use isocast_protocol::api::{Frame, FrameParams, Method};
use slint::wgpu_30::wgpu;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use crate::frames::Picture;

/// macOS and Linux: the shared surface the daemon drew.
#[cfg(any(target_os = "macos", target_os = "linux"))]
type Received = SharedFrame;
/// Other platforms: the daemon's JPEG, decoded.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
type Received = Picture;

type Slot = Arc<Mutex<Option<Received>>>;

struct Watching {
    source: String,
    stop: Arc<AtomicBool>,
    newest: Slot,
    /// macOS and Linux: the images made from the ring's surfaces (one per
    /// slot).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    images: HashMap<(u64, u32), slint::Image>,
    /// The frames shown now and just before (the GPU may still read them).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    showing: Option<Received>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    previous: Option<Received>,
}

#[derive(Default)]
pub struct SourcePreview {
    /// The window's GPU device, and the queue that fills the frames'
    /// pictures into its textures (macOS and Linux; other platforms' JPEG
    /// path needs no GPU).

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    gpu: RefCell<Option<(wgpu::Device, wgpu::Queue)>>,
    watching: Rc<RefCell<Option<Watching>>>,
    timer: slint::Timer,
}

impl SourcePreview {
    /// The GPU the window draws with: the frames' textures are its
    /// textures, and its queue fills their pictures in.
    pub fn set_device(&self, device: wgpu::Device, queue: wgpu::Queue) {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            *self.gpu.borrow_mut() = Some((device, queue));
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let (device, queue) = (device, queue);
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
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            images: HashMap::new(),
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            showing: None,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            previous: None,
        });
        let watching = self.watching.clone();
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let Some((device, queue)) = self.gpu.borrow().clone() else {
                return;
            };
            self.timer.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(16),
                move || tick_shared(&watching, &device, &queue, &show),
            );
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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

/// macOS and Linux: show the newest shared frame, if a new one came.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn tick_shared(
    watching: &RefCell<Option<Watching>>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    show: &dyn Fn(slint::Image, f32),
) {
    {
        let mut watching = watching.borrow_mut();
        let Some(w) = watching.as_mut() else { return };
        let Some(frame) = w.newest.lock().ok().and_then(|mut f| f.take()) else {
            return;
        };
        // Every frame: on Linux the picture reaches the GPU inside the
        // texture call, so the call updates a slot's texture even when
        // its image is made already.
        let texture = match frame.texture(device, queue) {
            Ok(texture) => texture,
            Err(error) => {
                isocast_diagnostics::diagnostic!("warn";
                    "source preview: can't show a shared frame: {error:#}"
                );
                return;
            }
        };
        let key = frame.slot();
        if !w.images.contains_key(&key) {
            // A new ring: the old one's images go.
            w.images.retain(|(generation, _), _| *generation == key.0);
            match slint::Image::try_from(texture) {
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

/// Other platforms: show the newest decoded picture, if one came.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
#[cfg(any(target_os = "macos", target_os = "linux"))]
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

/// The receiving thread, other platforms: the daemon's JPEG pictures,
/// asked for again as each arrives (the daemon waits for one newer than
/// `after`, so this is live, and at most 30 a second).
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
