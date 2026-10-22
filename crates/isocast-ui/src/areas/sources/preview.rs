//! A source's live picture in Properties and Filters (the web app's
//! SourcePreview): the daemon draws that source into a small ring of
//! shared GPU surfaces (`shared_frames` with the source as its target), a
//! thread of its own receives them, and the window shows the newest as a
//! Slint image on the same GPU device, without copying it. It stops when
//! the dialog closes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use isocast_client::shared::SharedFrame;
use isocast_protocol::api::SharedFramesParams;
use slint::wgpu_30::wgpu;

type Slot = Arc<Mutex<Option<SharedFrame>>>;

struct Watching {
    source: String,
    stop: Arc<AtomicBool>,
    newest: Slot,
    images: HashMap<(u64, u32), slint::Image>,
    /// The frames shown now and just before (the GPU may still read it).
    showing: Option<SharedFrame>,
    previous: Option<SharedFrame>,
}

#[derive(Default)]
pub struct SourcePreview {
    /// The window's GPU device, and the queue that fills the frames'
    /// pictures into its textures.
    gpu: RefCell<Option<(wgpu::Device, wgpu::Queue)>>,
    watching: Rc<RefCell<Option<Watching>>>,
    timer: slint::Timer,
}

impl SourcePreview {
    /// The GPU the window draws with: the frames' textures are its
    /// textures, and its queue fills their pictures in.
    pub fn set_device(&self, device: wgpu::Device, queue: wgpu::Queue) {
        *self.gpu.borrow_mut() = Some((device, queue));
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
        let Some((device, queue)) = self.gpu.borrow().clone() else {
            return;
        };
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
            images: HashMap::new(),
            showing: None,
            previous: None,
        });
        let watching = self.watching.clone();
        self.timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || tick(&watching, &device, &queue, &show),
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

/// Show the newest frame, if a new one came.
fn tick(
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

/// The receiving thread: the newest frame into `newest`, until stopped.
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
