//! Browser sources: a web page rendered by WebKitGTK in an offscreen
//! window, snapshotted at the source's frame rate into BGRA frames.
//!
//! WebKitGTK needs a running GTK main loop, and one process gets one
//! GTK initialization - so [`page_thread`] is the daemon's one browser
//! thread, shared by every page. Pages are set up and taken apart
//! there as jobs; a capture only ever holds its id, the way macOS's
//! holds its page number. A page's sound, when the engine asks for it,
//! routes through `audio.js` (macOS's script, injected at document
//! start) into a script-message handler and on to the `AudioSink`.
//!
//! A page thread's main loop is why every page record is `Arc` and
//! `Mutex` rather than `Rc` and `RefCell`: a timer's closure needs to
//! be `Send` even though only one thread ever runs it.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use gtk::prelude::{ContainerExt as _, GtkWindowExt as _, WidgetExt as _, WidgetExtManual as _};
use java_script_core::ValueExt as _;
use webkit2gtk::glib;
use webkit2gtk::glib::translate::ToGlibPtr as _;
// The WebKit trait set: snapshot, loading state, page control.
use webkit2gtk::{
    SettingsExt as _, SnapshotOptions, SnapshotRegion, UserContentInjectedFrames,
    UserContentManagerExt as _, UserScriptInjectionTime, UserStyleLevel, WebView, WebViewExt as _,
};

use crate::{
    frames::{CpuPool, LinuxFrame},
    system::host_time,
};

/// How long a page's set-up may take on its thread, as on macOS.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// The script that captures a page's sound.
const AUDIO_SCRIPT: &str = include_str!("audio.js");

/// The message handler `audio.js` posts to.
const AUDIO_HANDLER: &str = "isocastAudio";

/// How far the page's audio may drift from the host clock before its
/// timestamps are reset to it.
const AUDIO_RESYNC: Duration = Duration::from_millis(150);

/// What the thread's boot says when GTK did not take off by itself.
const THREAD_ERROR: &str = "the browser thread did not start";

/// A job to the page thread.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// The daemon's one page context, up once the thread runs. Two
/// captures who both demand the thread's first start arrive one after
/// the other through [`page_thread`]'s mutex; the loser simply shares
/// the winner's context.
static CONTEXT: Mutex<Option<&'static glib::MainContext>> = Mutex::new(None);

/// Where the context finally lives, so it is 'static once in.
static CONTEXT_ONCE: OnceLock<glib::MainContext> = OnceLock::new();

/// The page thread's context, starting the thread on the first browser
/// source. It runs for the daemon's life after that; everything web
/// goes through [`post_job`].
fn page_thread() -> Result<&'static glib::MainContext> {
    let mut shared = CONTEXT.lock().unwrap();
    if let Some(found) = shared.as_ref() {
        return Ok(found);
    }
    // GTK answers once the loop is known to run, carrying its context
    // with it, so the first page never races the loop starting up.
    let (up, up_answer) = mpsc::sync_channel::<Result<glib::MainContext>>(1);

    std::thread::Builder::new()
        .name("isocast browser".into())
        .spawn(move || {
            // GTK on this thread, with the main context it owns alone -
            // the rest of the daemon keeps whatever contexts it runs.
            let started = gtk::init();
            if started.is_err() {
                let _ = up.send(Err(anyhow!("{THREAD_ERROR}: {started:?}")));
                return;
            }
            let context = glib::MainContext::new();
            let loop_context = context.clone();
            let up_reporter = up.clone();
            let alive = context
                .with_thread_default(move || {
                    let _ = up.send(Ok(loop_context.clone()));
                    // The context is this thread's default for
                    // everything the thread ever does, the loop running
                    // instead of the scope's end. (The scope never pops,
                    // and never needs to: the loop runs on.)
                    glib::MainLoop::new(Some(&loop_context), false).run();
                })
                .ok();
            if alive.is_none() {
                let _ = up_reporter.send(Err(anyhow!(
                    "{THREAD_ERROR}: its main context could not be held"
                )));
            }
        })
        .with_context(|| THREAD_ERROR)?;

    let context = match up_answer.recv_timeout(SETUP_TIMEOUT) {
        Ok(Ok(context)) => context,
        Ok(Err(the_reason)) => return Err(the_reason),
        Err(_) => bail!("{THREAD_ERROR} within 10 s"),
    };
    // The boot thread holds the context as thread default forever, so
    // it owns the loop; the caller has the shared handle just the same.
    let owned = CONTEXT_ONCE.get_or_init(|| context);
    *shared = Some(owned);
    Ok(owned)
}

/// Hand one `job` over to the page thread.
fn post_job(job: Job) -> Result<()> {
    page_thread()?.invoke(job);
    Ok(())
}

// ---- The engine's entry ---------------------------------------------

use isocast_engine::{
    AudioSink, BrowserControl, BrowserPage, CaptureFormat, FramePool as _, PageLocation,
    SAMPLE_RATE, Time, VideoSink,
};

/// Start rendering `page`'s web address into `sink`, answering the
/// engine's own size and rate. This runs on the engine's thread;
/// everything web belongs to the page thread.
pub fn capture_browser(
    page: &BrowserPage<'_>,
    sink: VideoSink<LinuxFrame>,
    audio: Option<AudioSink>,
) -> Result<(BrowserCapture, CaptureFormat, Arc<PageControl>)> {
    let (width, height, fps) = (page.width, page.height, page.fps);
    let location = match page.location {
        PageLocation::Url(url) => url.to_string(),
        PageLocation::File(path) => {
            let bytes = std::fs::canonicalize(path)
                .with_context(|| format!("the page file {} is not there", path.display()))?;
            let text = bytes
                .to_str()
                .with_context(|| format!("the path {} is not valid UTF-8", path.display()))?;
            format!("file://{text}")
        }
    };

    let id = NEXT_PAGE.fetch_add(1, Ordering::Relaxed);
    let pool = Mutex::new(Some(CpuPool::bgra(width, height)));
    let sink = Arc::new(sink);
    let (done, done_answer) = mpsc::sync_channel::<Result<()>>(1);
    let css = page.css.to_string();
    let audio = audio.map(AudioRx::new);
    post_job(Box::new(move || {
        let _ = done.send(set_up(
            id, location, css, width, height, fps, pool, sink, audio,
        ));
    }))?;
    match done_answer.recv_timeout(SETUP_TIMEOUT) {
        Ok(answer) => answer?,
        Err(_) => bail!("the page thread did not answer in 10 s while setting up a page"),
    }
    Ok((
        BrowserCapture { id },
        CaptureFormat { width, height, fps },
        Arc::new(PageControl { id }),
    ))
}

// The pages being rendered, on the page thread.
thread_local! {
    /// The pages' WebKit context, this thread's own and never dropped.
    /// The default context is a C++ static that `exit()` destroys on
    /// whichever thread exits, and WebKit aborts when its timers die off
    /// the thread that made them (a daemon quitting after a browser
    /// source: SIGABRT in `WTF::RunLoop::TimerBase`).
    static PAGE_CONTEXT: webkit2gtk::WebContext = webkit2gtk::WebContext::new();

    static PAGES: std::cell::RefCell<HashMap<u64, Page>> =
        std::cell::RefCell::new(HashMap::new());
}

static NEXT_PAGE: AtomicU64 = AtomicU64::new(1);

// ---- The page -------------------------------------------------------

/// A page on the thread: its window, view and snapshot timer, with the
/// macOS `Page`'s shapes.
struct Page {
    window: gtk::OffscreenWindow,
    web: WebView,
    /// The page's scripts and their message handler: the audio handler
    /// is unregistered when the page goes, so nothing more reaches its
    /// sink while WebKit takes the page down.
    manager: webkit2gtk::UserContentManager,
    /// Whether `audio.js` and its handler are installed.
    audio: bool,
    timer: Option<glib::SourceId>,
    /// The page thread's main context, which holds `timer`; its id
    /// removal looks only in a context's own sources, so drop finds it
    /// here rather than through `g_source_remove`, which searches the
    /// global default context and would warn.
    context: Option<glib::MainContext>,
    address: String,
    /// Whether the page is loaded, or unloaded while it isn't shown.
    loaded: Arc<AtomicBool>,
    /// An empty frame has been sent since the page was unloaded.
    cleared: Arc<AtomicBool>,
}

impl Drop for Page {
    fn drop(&mut self) {
        if self.audio {
            self.manager
                .unregister_script_message_handler(AUDIO_HANDLER);
        }
        if let (Some(context), Some(timer)) = (self.context.as_ref(), self.timer.take())
            && let Some(source) = context.find_source_by_id(&timer)
        {
            source.destroy();
        }
        // GTK's way to take a widget down; unsound off the widget's
        // thread, and this runs on the page's.
        unsafe { self.window.destroy() };
    }
}

/// Run `action` against the page of `id`, if it is there.
fn with_page(id: u64, action: impl FnOnce(&Page) + Send + 'static) {
    PAGES.with(|pages| {
        if let Some(page) = pages.borrow().get(&id) {
            action(page)
        }
    });
}

/// Set up one page (this runs on the page thread): take `pool` over it
/// (the guard against setting up twice), make an offscreen window with
/// the view sized and its address loading, and start the timer.
#[allow(clippy::too_many_arguments)]
fn set_up(
    id: u64,
    location: String,
    css: String,
    width: u32,
    height: u32,
    fps: u32,
    pool: Mutex<Option<CpuPool>>,
    sink: Arc<VideoSink<LinuxFrame>>,
    audio: Option<AudioRx>,
) -> Result<()> {
    let pool = Arc::new(Mutex::new(
        pool.lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow!("the page was set up twice"))?,
    ));

    // A local page may fetch the files next to it, as on macOS.
    let settings = webkit2gtk::Settings::builder()
        .allow_file_access_from_file_urls(true)
        .build();
    // With audio, a page may start its own sound without a click,
    // as on macOS.
    if audio.is_some() {
        settings.set_media_playback_requires_user_gesture(false);
    }
    // The custom CSS as the page's own stylesheet, applied whenever
    // the document runs (the CSS macOS adds as a user script).
    let manager = webkit2gtk::UserContentManager::new();
    if let Some(audio) = &audio {
        // The page's sound posts here (the macOS same: audio.js
        // injects at the document's start, posts base64 stereo).
        if !manager.register_script_message_handler(AUDIO_HANDLER) {
            bail!("the page's audio handler could not be registered");
        }
        let audio = audio.clone();
        manager.connect_script_message_received(Some(AUDIO_HANDLER), move |_, result| {
            audio.receive(result);
        });
        manager.add_script(&webkit2gtk::UserScript::new(
            AUDIO_SCRIPT,
            UserContentInjectedFrames::AllFrames,
            UserScriptInjectionTime::Start,
            &[],
            &[],
        ));
    }
    if !css.trim().is_empty() {
        manager.add_style_sheet(&webkit2gtk::UserStyleSheet::new(
            &css,
            UserContentInjectedFrames::AllFrames,
            UserStyleLevel::Author,
            &[],
            &[],
        ));
    }

    // WebKitGTK lets a page start sound only after a click unless its
    // autoplay policy says otherwise: a suspended AudioContext's clock
    // never runs, so the page would be silent and its animations tied to
    // it frozen. With its sound routed to the recorder (never aloud),
    // the page may play.
    let policies = webkit2gtk::WebsitePolicies::builder()
        .autoplay(if audio.is_some() {
            webkit2gtk::AutoplayPolicy::Allow
        } else {
            webkit2gtk::AutoplayPolicy::AllowWithoutSound
        })
        .build();
    let web = WebView::builder()
        .web_context(&PAGE_CONTEXT.with(Clone::clone))
        .website_policies(&policies)
        .settings(&settings)
        .user_content_manager(&manager)
        .build();
    // A page that fails to load says so, instead of staying unknown.
    web.connect_load_failed(move |_, _frame, _event, error| {
        isocast_diagnostics::diagnostic!("warn"; "browser page failed to load: {error:?}");
        false
    });
    // Transparent where the page does not paint.
    unsafe {
        webkit2gtk::ffi::webkit_web_view_set_background_color(
            web.to_glib_none().0,
            &gdk::ffi::GdkRGBA {
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                alpha: 0.0,
            },
        );
    }

    // Offscreen: realized but never mapped to the screen, while its
    // picture still gets drawn. WebKitGTK answers a never-realized
    // view's snapshot with an error, so it must be shown; a GTK
    // offscreen window's show never reaches the screen.
    let window = gtk::OffscreenWindow::new();
    window.set_default_size(width as i32, height as i32);
    web.set_size_request(width as i32, height as i32);
    window.add(&web);
    window.show_all();

    // Snapshot at the frame rate, skipping a tick while one is in
    // flight (as on macOS). An unloaded page leaves one empty frame.
    // The timer must be this thread's own main context, not glib's
    // global default, which a daemon needs nothing of. The timer's
    // closure is `Send`-bound, so no WebKit object goes in it: it looks
    // the page up by id in the thread's own map - same for the `Arc`
    // records, because only one thread ever runs any of it.
    let context = glib::MainContext::thread_default().expect("the page thread's own main context");
    let loaded = Arc::new(AtomicBool::new(true));
    let cleared = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicBool::new(false));
    let reported = Arc::new(AtomicBool::new(false));
    let (tick_loaded, tick_cleared) = (loaded.clone(), cleared.clone());
    let timer = glib::timeout_source_new(
        Duration::from_nanos(1_000_000_000u64 / u64::from(fps)),
        Some("isocast browser snapshot"),
        glib::Priority::default(),
        move || {
            PAGES.with(|pages| {
                if let Some(page) = pages.borrow().get(&id) {
                    tick(
                        &page.web,
                        &pool,
                        &sink,
                        width,
                        height,
                        busy.clone(),
                        tick_loaded.clone(),
                        tick_cleared.clone(),
                        reported.clone(),
                    );
                }
            });
            glib::ControlFlow::Continue
        },
    )
    .attach(Some(&context));

    let page = Page {
        manager,
        audio: audio.is_some(),
        window,
        timer: Some(timer),
        context: Some(context),
        web: web.clone(),
        address: location.clone(),
        loaded,
        cleared,
    };
    PAGES.with(|pages| pages.borrow_mut().insert(id, page));
    web.load_uri(&location);
    Ok(())
}

/// One snapshot tick, the macOS's shapes: skip a tick while one is in
/// flight, an unloaded page leaves exactly one empty frame, and a pool
/// without free frames (`None` from draw) drops the frame - the
/// consumer is behind.
#[allow(clippy::too_many_arguments)]
fn tick(
    web: &WebView,
    pool: &Arc<Mutex<CpuPool>>,
    sink: &Arc<VideoSink<LinuxFrame>>,
    width: u32,
    height: u32,
    busy: Arc<AtomicBool>,
    loaded: Arc<AtomicBool>,
    cleared: Arc<AtomicBool>,
    reported: Arc<AtomicBool>,
) -> glib::ControlFlow {
    if busy.load(Ordering::SeqCst) {
        return glib::ControlFlow::Continue;
    }
    if !loaded.load(Ordering::SeqCst) {
        if !cleared.swap(true, Ordering::SeqCst) {
            let drawn = pool
                .lock()
                .unwrap()
                .draw(&mut |pixels, _stride| pixels.fill(0));
            if let Ok(Some(frame)) = drawn {
                (sink.frame)(frame, host_time());
            }
        }
        return glib::ControlFlow::Continue;
    }
    if web.is_loading() {
        return glib::ControlFlow::Continue;
    }
    busy.store(true, Ordering::SeqCst);
    let (callback_pool, callback_sink, callback_reported) = (pool.clone(), sink.clone(), reported);
    web.snapshot(
        SnapshotRegion::Visible,
        SnapshotOptions::TRANSPARENT_BACKGROUND,
        Option::<&webkit2gtk::gio::Cancellable>::None,
        move |snapshot: Result<cairo::Surface, glib::Error>| {
            busy.store(false, Ordering::SeqCst);
            let drawn: Result<Option<LinuxFrame>> = match snapshot {
                Ok(surface) => draw(&callback_pool, surface, width, height),
                Err(err) => Err(err.into()),
            };
            match drawn {
                Ok(Some(frame)) => (callback_sink.frame)(frame, host_time()),
                // The pool is out of frames: the consumer is behind.
                Ok(None) => {}
                Err(err) => {
                    if !callback_reported.swap(true, Ordering::SeqCst) {
                        isocast_diagnostics::diagnostic!(
                            "warn";
                            "browser source snapshot failed: {err:#}"
                        );
                    }
                }
            }
        },
    );
    glib::ControlFlow::Continue
}

/// Draw a snapshot into a frame from `pool` at `width`×`height`, with
/// straight alpha. WebKitGTK answers in device pixels, so a scaled
/// window's snapshot arrives 2× (or 1.5×); a nearest-neighbour step
/// covers that for now.
fn draw(
    pool: &Arc<Mutex<CpuPool>>,
    surface: cairo::Surface,
    width: u32,
    height: u32,
) -> Result<Option<LinuxFrame>> {
    let image = match cairo::ImageSurface::try_from(surface) {
        Ok(image) => image,
        Err(err) => bail!("the page's snapshot could not be read: {err:?}"),
    };
    if image.format() != cairo::Format::ARgb32 {
        bail!("the page's snapshot is not premultiplied ARGB32");
    }
    let (src_w, src_h, src_stride) = (
        image.width() as usize,
        image.height() as usize,
        image.stride() as usize,
    );
    let mut readable = image;
    let src = readable.data().expect("cairo's surface bytes").to_vec();
    if src.is_empty() {
        bail!("the page's snapshot has no pixels");
    }
    let frame = pool.lock().unwrap().draw(&mut |pixels, stride| {
        for oy in 0..height as usize {
            let sy = if src_h == height as usize {
                oy
            } else {
                (oy * src_h) / height as usize
            };
            let src_row = &src[sy * src_stride..];
            let dst_row = &mut pixels[oy * stride..];
            for ox in 0..width as usize {
                let sx = if src_w == width as usize {
                    ox
                } else {
                    (ox * src_w) / width as usize
                };
                let pixel = &mut dst_row[ox * 4..][..4];
                pixel.copy_from_slice(&src_row[sx * 4..][..4]);
                // Cairo's premultiplied bytes into the engine's BGRA
                // straight alpha, the same rewrite macOS's draw does.
                let a = u32::from(pixel[3]);
                if a != 0 && a != 255 {
                    for color in &mut pixel[..3] {
                        *color = ((u32::from(*color) * 255 + a / 2) / a).min(255) as u8;
                    }
                }
            }
        }
    })?;
    Ok(frame)
}

// ---- Control and the capture ---------------------------------------

/// Control of a page from other threads.
pub struct PageControl {
    id: u64,
}

impl PageControl {
    fn control(&self, action: impl FnOnce(&Page) + Send + Sync + 'static) {
        let id = self.id;
        // The page thread is gone only with the daemon; nothing waits.
        let _ = post_job(Box::new(move || with_page(id, action)));
    }
}

impl BrowserControl for PageControl {
    /// Load the page again, bypassing the cache.
    fn refresh(&self) {
        self.control(|page| {
            if page.loaded.load(Ordering::SeqCst) {
                page.web.reload_bypass_cache();
            }
        });
    }

    /// Unload the page, leaving an empty frame, or load it again.
    fn set_loaded(&self, loaded: bool) {
        self.control(move |page| page.set_loaded(loaded));
    }
}

impl Page {
    /// The macOS's same load/unload: the page reloads, or empties out
    /// and sends one empty frame before the next.
    fn set_loaded(&self, loaded: bool) {
        if self.loaded.swap(loaded, Ordering::SeqCst) == loaded {
            return;
        }
        if loaded {
            self.web.load_uri(&self.address);
        } else {
            self.cleared.store(false, Ordering::SeqCst);
            self.web.load_html("", None);
        }
    }
}

/// A running browser source. Dropping it closes the page.
pub struct BrowserCapture {
    id: u64,
}

impl Drop for BrowserCapture {
    fn drop(&mut self) {
        // Whatever became of the engine's drive of this drop, the page
        // closes on the page thread itself; the thread is gone only
        // with the daemon, and then nothing waits.
        let id = self.id;
        let _ = post_job(Box::new(move || {
            PAGES.with(|pages| pages.borrow_mut().remove(&id));
        }));
    }
}

// ---- Page audio -------------------------------------------------------

/// A page's sound on its way to the engine: `audio.js`'s messages,
/// decoded, brought to [`SAMPLE_RATE`] and stamped against the host
/// clock, as macOS's `PageAudio` does. Made on the engine's thread and
/// used on the page thread, hence `Arc` and `Mutex`.
#[derive(Clone)]
struct AudioRx(Arc<AudioState>);

struct AudioState {
    sink: AudioSink,
    timing: Mutex<(Stamper, Resampler)>,
    /// An unreadable message has been said once.
    reported: AtomicBool,
}

impl AudioRx {
    fn new(sink: AudioSink) -> Self {
        Self(Arc::new(AudioState {
            sink,
            timing: Mutex::new((Stamper::default(), Resampler::default())),
            reported: AtomicBool::new(false),
        }))
    }

    /// One message from the page's handler.
    fn receive(&self, result: &webkit2gtk::JavascriptResult) {
        let now = host_time();
        let decoded = result
            .js_value()
            .filter(|value| value.is_string())
            .context("not a string")
            .and_then(|value| decode(&value.to_str()));
        match decoded {
            Ok((rate, samples)) => {
                let mut timing = self.0.timing.lock().unwrap();
                let samples = timing.1.run(rate, samples);
                let frames = samples.len() / 2;
                if frames > 0 {
                    let at = timing.0.stamp(frames, now);
                    drop(timing);
                    (self.0.sink.samples)(&samples, at);
                }
            }
            Err(err) => {
                if !self.0.reported.swap(true, Ordering::SeqCst) {
                    isocast_diagnostics::diagnostic!("warn";
                        "isocastd: a browser source sent unreadable audio: {err:#}"
                    );
                }
            }
        }
    }
}

/// A message from `audio.js`: `RATE:BASE64`, the base64 of interleaved
/// stereo 32-bit floats.
fn decode(message: &str) -> Result<(u32, Vec<f32>)> {
    let (rate, data) = message.split_once(':').context("no sample rate")?;
    let rate: u32 = rate.parse().context("bad sample rate")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
    let samples = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    Ok((rate, samples))
}

/// Timestamps for audio that arrives in uneven bursts: each chunk follows
/// the last unless that has drifted from when it arrived by more than
/// [`AUDIO_RESYNC`].
#[derive(Default)]
struct Stamper {
    next: Option<Time>,
}

impl Stamper {
    /// When the first of `frames` frames that have just arrived was made.
    fn stamp(&mut self, frames: usize, now: Time) -> Time {
        let made =
            now - Duration::from_nanos(frames as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE));
        let limit = AUDIO_RESYNC.as_nanos() as i64;
        let at = match self.next {
            Some(next) if (next.as_nanos() - made.as_nanos()).abs() < limit => next,
            _ => made,
        };
        self.next = Some(at.add_ticks(frames as u64, SAMPLE_RATE));
        at
    }
}

/// Linear resampling of interleaved stereo to [`SAMPLE_RATE`], for pages
/// whose capture context runs at another rate.
#[derive(Default)]
struct Resampler {
    /// Position of the next output frame, in input frames after `last`.
    position: f64,
    last: [f32; 2],
}

impl Resampler {
    fn run(&mut self, rate: u32, samples: Vec<f32>) -> Vec<f32> {
        let input = samples.as_chunks::<2>().0;
        if rate == SAMPLE_RATE || rate == 0 || input.is_empty() {
            return samples;
        }
        let step = f64::from(rate) / f64::from(SAMPLE_RATE);
        let last = self.last;
        // Frame -1 is the previous chunk's last.
        let frame = |i: isize| if i < 0 { last } else { input[i as usize] };
        let mut out = Vec::with_capacity((input.len() as f64 / step) as usize * 2 + 2);
        while self.position < (input.len() - 1) as f64 {
            let i = self.position.floor();
            let t = (self.position - i) as f32;
            let (a, b) = (frame(i as isize), frame(i as isize + 1));
            out.extend([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            self.position += step;
        }
        self.position -= input.len() as f64;
        self.last = input[input.len() - 1];
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_decode_to_samples() {
        let samples = [0.5f32, -0.25, 1.0, 0.0];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let message = format!(
            "44100:{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        assert_eq!(decode(&message).unwrap(), (44100, samples.to_vec()));
        assert!(decode("no rate").is_err());
    }

    #[test]
    fn bursts_are_stamped_back_to_back_until_they_drift() {
        let ms = |n: i64| Time::from_nanos(n * 1_000_000);
        let mut stamper = Stamper::default();
        // 480 frames (10 ms) arriving at 100 ms: made at 90 ms.
        assert_eq!(stamper.stamp(480, ms(100)), ms(90));
        // The next arrives 30 ms late but follows on.
        assert_eq!(stamper.stamp(480, ms(140)), ms(100));
        // One 500 ms later starts again from when it arrived.
        assert_eq!(stamper.stamp(480, ms(640)), ms(630));
    }

    #[test]
    fn resampling_is_continuous_across_chunks() {
        let mut resampler = Resampler::default();
        // A ramp at 24 kHz comes out at 48 kHz with points in between.
        let ramp: Vec<f32> = (0..200).flat_map(|i| [i as f32, i as f32]).collect();
        let mut out = Vec::new();
        for chunk in ramp.chunks(50) {
            out.extend(resampler.run(24_000, chunk.to_vec()));
        }
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert!((396..=400).contains(&left.len()), "{} frames", left.len());
        for pair in left.windows(2) {
            assert!((pair[1] - pair[0] - 0.5).abs() < 1e-4, "{pair:?}");
        }
        // 48 kHz passes through untouched.
        assert_eq!(resampler.run(48_000, vec![1.0, 2.0]), vec![1.0, 2.0]);
    }
}
