//! A window, or an app's windows, captured straight from the compositor.
//!
//! This process is a plain Wayland client of three standard protocols:
//! `ext-foreign-toplevel-list-v1` says which windows exist (app id, title
//! and a stable identifier), `ext-image-capture-source-v1` turns one into
//! something capturable, and `ext-image-copy-capture-v1` copies its picture
//! into shared memory we own. There is no portal and no picker: the
//! compositor offers the three to any client, and the compositor renders a
//! window on its own, so overlap does not show in the picture. That makes
//! it the same "desktop-independent window" macOS captures, and it is
//! something a person never has to answer, so it works unattended.
//!
//! A window is found by app id and (part of) title, as on macOS, and
//! followed: while its toplevel lives the capture stays on it, and when
//! it closes the capture waits for a matching one to appear. An app is a
//! display-sized canvas on which each of its windows on that display is
//! drawn where the compositor has it (positions come from Hyprland's IPC,
//! which the protocols do not carry).
//!
//! Pictures arrive when the window changes, not on a clock, so a session
//! is asked for the next one `1/fps` after the last was asked for; the
//! engine keeps the newest picture and repeats it while nothing changes,
//! as it does for the portal's screens. The size a take records at is the
//! window's when the capture opens (rounded to even): a window that is
//! resized later is scaled to fit into it (window_pixels.rs).

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{CaptureFormat, FramePool, Platform, ScreenTarget, VideoSink};
use isocast_protocol::devices::Display;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop, event_created_child,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::{
    self, ExtForeignToplevelHandleV1,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::{
    self, ExtForeignToplevelListV1,
};
use wayland_protocols::ext::image_capture_source::v1::client::ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    self, ExtImageCopyCaptureFrameV1, FailureReason,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{
    ExtImageCopyCaptureManagerV1, Options,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    self, ExtImageCopyCaptureSessionV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{
    self, ZwpLinuxBufferParamsV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;

use isocast_engine::gpu::{FrameImporter, Gpu};

use crate::dmabuf;
use crate::frames::{CpuPool, LinuxFrame};
use crate::gpu::VulkanImporter;
use crate::hyprland::{self, Client, Monitor};
use crate::window_pixels::{self as pixels, Layout, Picture, Rect};
use crate::{LinuxPlatform, system};

/// How long the compositor has to deliver the first picture. The first
/// capture of a session is answered at once; a compositor that renders
/// nothing (its monitor asleep) is the case this bounds.
const FIRST_PICTURE_WAIT: Duration = Duration::from_secs(6);
/// The most a loop sleeps before it looks at the window set and the
/// compositor again.
const LOOP_TICK: Duration = Duration::from_millis(250);
/// How often the window set (and where the windows are) is re-read.
const RECONCILE_EVERY: Duration = Duration::from_millis(250);
/// No picture at all for this long is looked into.
const STALL_AFTER: Duration = Duration::from_secs(5);
/// Pictures the pool was too busy to take are reported at most this often.
const DROP_REPORT: Duration = Duration::from_secs(5);
/// A window that keeps failing to copy is given up on after this many
/// failures in a row (the capture goes on with the others, or waits).
const MAX_FAILURES: u32 = 5;
/// More windows than this are not composited (a runaway match).
const MAX_LAYERS: usize = 16;

// ---------------------------------------------------------------------------
// Which windows match.
// ---------------------------------------------------------------------------

/// Whether a window's app id matches `query`: exactly, or containing it,
/// ignoring case (macOS: the app's name or bundle id, or part of the name).
fn app_matches(app_id: &str, query: &str) -> bool {
    let (app_id, query) = (app_id.to_lowercase(), query.to_lowercase());
    app_id == query || app_id.contains(&query)
}

/// Whether a window's title contains `query`, ignoring case.
fn title_matches(title: &str, query: Option<&str>) -> bool {
    query.is_none_or(|query| title.to_lowercase().contains(&query.to_lowercase()))
}

/// What a toplevel says about itself.
#[derive(Debug, Default, Clone)]
struct Info {
    app_id: String,
    title: String,
    identifier: String,
    /// The compositor has sent a complete description (`done`).
    done: bool,
    closed: bool,
}

/// A window as the matcher sees it: its toplevel's description and, when
/// Hyprland could be asked, its client.
#[derive(Debug, Clone)]
struct Candidate {
    info: Info,
    client: Option<Client>,
}

/// The client a toplevel is: by Hyprland's stable id (the toplevel's
/// `identifier`), else the first one with the same class and title.
fn client_for<'a>(info: &Info, clients: &'a [Client]) -> Option<&'a Client> {
    clients
        .iter()
        .find(|client| {
            !info.identifier.is_empty() && client.stable_id.eq_ignore_ascii_case(&info.identifier)
        })
        .or_else(|| {
            clients
                .iter()
                .find(|client| client.class == info.app_id && client.title == info.title)
        })
}

/// The window a `Window` source means among `candidates` (indices into
/// it): the largest match, as on macOS; a tie goes to the one focused
/// most recently, then to the one listed first, so the choice is the same
/// every time.
fn best(candidates: &[Candidate], app: &str, title: Option<&str>) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| app_matches(&c.info.app_id, app) && title_matches(&c.info.title, title))
        .max_by_key(|(index, c)| {
            let (area, focus) = c.client.as_ref().map_or((0, i64::MIN), |client| {
                (
                    i64::from(client.size.0) * i64::from(client.size.1),
                    -client.focus_history,
                )
            });
            (area, focus, std::cmp::Reverse(*index))
        })
        .map(|(index, _)| index)
}

/// "app — title" for each window, sorted: what a mismatch lists as the
/// choices.
fn choices(infos: &[Info]) -> String {
    let mut listed: Vec<String> = infos
        .iter()
        .filter(|info| !info.app_id.is_empty())
        .map(|info| match info.title.as_str() {
            "" => info.app_id.clone(),
            title => format!("{} — {title}", info.app_id),
        })
        .collect();
    listed.sort();
    listed.dedup();
    if listed.is_empty() {
        "none".to_string()
    } else {
        listed.join(", ")
    }
}

// ---------------------------------------------------------------------------
// The Wayland side.
// ---------------------------------------------------------------------------

/// What the compositor settled about one session's buffers.
#[derive(Debug, Default, Clone)]
struct Constraints {
    size: Option<(u32, u32)>,
    formats: Vec<u32>,
    /// DMA-BUF formats and the layouts (modifiers) offered for each.
    dmabuf: Vec<(u32, Vec<u64>)>,
    /// The compositor's main device (`dev_t`) for DMA-BUFs.
    device: Option<u64>,
}

/// How one frame ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ready,
    Failed(Failure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// The session has stopped.
    Stopped,
    /// Our buffer no longer matches the session's constraints.
    Constraints,
    Unknown,
}

/// Shared memory the compositor writes a picture into.
struct Mapping {
    pointer: *mut u8,
    length: usize,
}

impl Mapping {
    fn new(length: usize) -> Result<(Self, OwnedFd)> {
        // SAFETY: plain libc calls; the descriptor is owned right away and
        // the mapping is unmapped by `Drop`.
        unsafe {
            let fd = libc::memfd_create(c"isocast-window".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                bail!("memfd_create: {}", std::io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(fd);
            if libc::ftruncate(fd.as_raw_fd(), length as libc::off_t) != 0 {
                bail!("ftruncate: {}", std::io::Error::last_os_error());
            }
            let pointer = libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            );
            if pointer == libc::MAP_FAILED {
                bail!("mmap: {}", std::io::Error::last_os_error());
            }
            Ok((
                Self {
                    pointer: pointer.cast(),
                    length,
                },
                fd,
            ))
        }
    }

    /// The bytes. Only read while no capture is writing them.
    fn bytes(&self) -> &[u8] {
        // SAFETY: `pointer` maps `length` bytes until `Drop`.
        unsafe { std::slice::from_raw_parts(self.pointer, self.length) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping `new` made, unmapped once.
        unsafe {
            libc::munmap(self.pointer.cast(), self.length);
        }
    }
}

/// One picture's worth of shared memory, as a `wl_buffer`.
struct ShmBuffer {
    pool: WlShmPool,
    buffer: WlBuffer,
    map: Mapping,
    width: u32,
    height: u32,
}

impl ShmBuffer {
    fn new(
        shm: &WlShm,
        qh: &QueueHandle<State>,
        width: u32,
        height: u32,
        format: wl_shm::Format,
    ) -> Result<Self> {
        let stride = width as usize * 4;
        let length = stride * height as usize;
        let (map, fd) = Mapping::new(length)?;
        let pool = shm.create_pool(fd.as_fd(), length as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );
        Ok(Self {
            pool,
            buffer,
            map,
            width,
            height,
        })
    }

    fn stride(&self) -> usize {
        self.width as usize * 4
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

/// One picture's worth of GPU memory the compositor renders into: a GBM
/// buffer, as a `wl_buffer` for the compositor and as a texture of the
/// engine's GPU for us (dmabuf.rs).
struct DmaBuffer {
    buffer: WlBuffer,
    texture: wgpu::Texture,
    /// The copy out of it still on the GPU: the compositor must not
    /// render the next picture into it before that is done.
    busy: Option<wgpu::SubmissionIndex>,
    width: u32,
    height: u32,
    /// Freed after the `wl_buffer` and the texture are.
    _bo: dmabuf::Buffer,
}

impl DmaBuffer {
    fn new(
        gpu: &Gpu,
        dmabuf_global: &ZwpLinuxDmabufV1,
        qh: &QueueHandle<State>,
        allocator: &dmabuf::Allocator,
        (width, height): (u32, u32),
        modifiers: &[u64],
    ) -> Result<Self> {
        let bo = allocator.allocate(width, height, dmabuf::ARGB8888, modifiers)?;
        let texture = dmabuf::import(
            gpu,
            bo.fd()?.as_fd(),
            width,
            height,
            bo.modifier,
            bo.stride,
            bo.offset,
        )?;
        let params = dmabuf_global.create_params(qh, ());
        let fd = bo.fd()?;
        params.add(
            fd.as_fd(),
            0,
            bo.offset,
            bo.stride,
            (bo.modifier >> 32) as u32,
            bo.modifier as u32,
        );
        let buffer = params.create_immed(
            width as i32,
            height as i32,
            dmabuf::ARGB8888,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
            (),
        );
        params.destroy();
        Ok(Self {
            buffer,
            texture,
            busy: None,
            width,
            height,
            _bo: bo,
        })
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

/// One window being captured.
struct Layer {
    id: u64,
    handle: ExtForeignToplevelHandleV1,
    session: ExtImageCopyCaptureSessionV1,
    /// Constraint events of the batch being received.
    pending: Constraints,
    /// The last complete batch.
    constraints: Option<Constraints>,
    /// A batch arrived since the buffers were made.
    changed: bool,
    stopped: bool,
    /// The frame being captured, and the buffer it writes to.
    frame: Option<(ExtImageCopyCaptureFrameV1, usize)>,
    outcome: Option<Outcome>,
    /// Two buffers: one holds the newest complete picture while the
    /// compositor writes the next into the other.
    buffers: Vec<ShmBuffer>,
    /// Or two GPU buffers (a window source whose picture is the take's
    /// size, when the compositor and the GPU can share them).
    dma: Vec<DmaBuffer>,
    layout: Layout,
    /// Which buffer holds the newest complete picture.
    latest: Option<usize>,
    /// A picture arrived that has not been drawn to the canvas yet.
    fresh: bool,
    requested: Option<Instant>,
    failures: u32,
}

impl Layer {
    /// How long until the next picture is due to be asked for: `None`
    /// while one is being captured (or there is nowhere to put it yet).
    fn due_in(&self, now: Instant, interval: Duration) -> Option<Duration> {
        if self.frame.is_some() || (self.buffers.is_empty() && self.dma.is_empty()) {
            return None;
        }
        Some(self.requested.map_or(Duration::ZERO, |at| {
            (at + interval).saturating_duration_since(now)
        }))
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        if let Some((frame, _)) = self.frame.take() {
            frame.destroy();
        }
        self.session.destroy();
    }
}

/// What the dispatch callbacks write into.
struct State {
    toplevels: Vec<ExtForeignToplevelHandleV1>,
    /// A toplevel appeared, changed or closed.
    changed: bool,
    /// The compositor stopped offering toplevels altogether.
    finished: bool,
    layers: Vec<Layer>,
    /// The size a probe's throwaway session reported.
    probed: Option<(u32, u32)>,
}

/// A layer's id and where its window sits (x, y, monitor), if known.
type Placed = (u64, Option<(i32, i32, i64)>);

/// The id of a probe's session, which belongs to no layer.
const PROBE_ID: u64 = 0;

impl State {
    fn layer(&mut self, id: u64) -> Option<&mut Layer> {
        self.layers.iter_mut().find(|layer| layer.id == id)
    }
}

fn info_of(handle: &ExtForeignToplevelHandleV1) -> Info {
    handle
        .data::<Mutex<Info>>()
        .map(|info| info.lock().expect("toplevel info poisoned").clone())
        .unwrap_or_default()
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } => {
                state.toplevels.push(toplevel);
                state.changed = true;
            }
            ext_foreign_toplevel_list_v1::Event::Finished => state.finished = true,
            _ => {}
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, Mutex::new(Info::default())),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, Mutex<Info>> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        info: &Mutex<Info>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut info = info.lock().expect("toplevel info poisoned");
        match event {
            ext_foreign_toplevel_handle_v1::Event::Closed => info.closed = true,
            ext_foreign_toplevel_handle_v1::Event::Done => info.done = true,
            ext_foreign_toplevel_handle_v1::Event::Title { title } => info.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => info.app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                info.identifier = identifier;
            }
            _ => {}
        }
        state.changed = true;
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, u64> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if *id == PROBE_ID {
            if let ext_image_copy_capture_session_v1::Event::BufferSize { width, height } = event {
                state.probed = Some((width, height));
            }
            return;
        }
        let Some(layer) = state.layer(*id) else {
            return;
        };
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                layer.pending.size = Some((width, height));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                if let WEnum::Value(format) = format {
                    layer.pending.formats.push(format as u32);
                } else if let WEnum::Unknown(code) = format {
                    layer.pending.formats.push(code);
                }
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                let modifiers = modifiers
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|bytes| u64::from_ne_bytes(*bytes))
                    .collect();
                layer.pending.dmabuf.push((format, modifiers));
            }
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                layer.pending.device = device
                    .get(..8)
                    .map(|bytes| u64::from_ne_bytes(bytes.try_into().expect("8 bytes")));
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                let pending = std::mem::take(&mut layer.pending);
                // A later batch only needs the parts that changed.
                let previous = layer.constraints.take().unwrap_or_default();
                layer.constraints = Some(Constraints {
                    size: pending.size.or(previous.size),
                    formats: if pending.formats.is_empty() {
                        previous.formats
                    } else {
                        pending.formats
                    },
                    dmabuf: if pending.dmabuf.is_empty() {
                        previous.dmabuf
                    } else {
                        pending.dmabuf
                    },
                    device: pending.device.or(previous.device),
                });
                layer.changed = true;
            }
            ext_image_copy_capture_session_v1::Event::Stopped => layer.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, u64> for State {
    fn event(
        state: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(layer) = state.layer(*id) else {
            return;
        };
        // Only the frame in flight counts: one we gave up on may still
        // be answered.
        if layer
            .frame
            .as_ref()
            .is_none_or(|(current, _)| current != frame)
        {
            return;
        }
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => layer.outcome = Some(Outcome::Ready),
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                layer.outcome = Some(Outcome::Failed(match reason {
                    WEnum::Value(FailureReason::Stopped) => Failure::Stopped,
                    WEnum::Value(FailureReason::BufferConstraints) => Failure::Constraints,
                    _ => Failure::Unknown,
                }));
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlShm);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ExtForeignToplevelImageCaptureSourceManagerV1);
delegate_noop!(State: ExtImageCaptureSourceV1);
delegate_noop!(State: ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore ZwpLinuxDmabufV1);
delegate_noop!(State: ignore ZwpLinuxBufferParamsV1);

/// The globals a capture needs.
struct Managers {
    /// Kept alive: its events are how windows appear and close.
    _list: ExtForeignToplevelListV1,
    source: ExtForeignToplevelImageCaptureSourceManagerV1,
    copy: ExtImageCopyCaptureManagerV1,
    shm: WlShm,
    /// For GPU buffers; a compositor without it gets shared memory.
    dmabuf: Option<ZwpLinuxDmabufV1>,
}

/// Connect to the compositor and bind what window capture needs. A
/// compositor without the protocols is named, with what to do.
fn connect() -> Result<(Connection, EventQueue<State>, Managers, State)> {
    let conn = Connection::connect_to_env().context(
        "connecting to the Wayland compositor (the daemon needs WAYLAND_DISPLAY and \
         XDG_RUNTIME_DIR from the desktop session: start it from there)",
    )?;
    let (globals, queue) = registry_queue_init::<State>(&conn)
        .context("reading the compositor's list of protocols")?;
    let qh = queue.handle();
    let missing = |name: &str| {
        let offered: Vec<String> = globals
            .contents()
            .clone_list()
            .into_iter()
            .map(|global| global.interface)
            .filter(|interface| interface.starts_with("ext_") || interface == "wl_shm")
            .collect();
        anyhow!(
            "this compositor does not offer {name}, which capturing a window needs (it offers: {})",
            offered.join(", ")
        )
    };
    let list = globals
        .bind::<ExtForeignToplevelListV1, _, _>(&qh, 1..=1, ())
        .map_err(|_| missing("ext_foreign_toplevel_list_v1"))?;
    let source = globals
        .bind::<ExtForeignToplevelImageCaptureSourceManagerV1, _, _>(&qh, 1..=1, ())
        .map_err(|_| missing("ext_foreign_toplevel_image_capture_source_manager_v1"))?;
    let copy = globals
        .bind::<ExtImageCopyCaptureManagerV1, _, _>(&qh, 1..=1, ())
        .map_err(|_| missing("ext_image_copy_capture_manager_v1"))?;
    let shm = globals
        .bind::<WlShm, _, _>(&qh, 1..=1, ())
        .map_err(|_| missing("wl_shm"))?;
    let dmabuf = globals.bind::<ZwpLinuxDmabufV1, _, _>(&qh, 3..=4, ()).ok();
    let state = State {
        toplevels: Vec::new(),
        changed: true,
        finished: false,
        layers: Vec::new(),
        probed: None,
    };
    Ok((
        conn,
        queue,
        Managers {
            _list: list,
            source,
            copy,
            shm,
            dmabuf,
        },
        state,
    ))
}

/// A short-lived connection that has listed the open windows: what a check
/// needs to answer without capturing.
struct Listing {
    _conn: Connection,
    queue: EventQueue<State>,
    managers: Managers,
    state: State,
}

impl Listing {
    fn open() -> Result<Self> {
        let (conn, mut queue, managers, mut state) = connect()?;
        // The toplevels, then their descriptions.
        queue.roundtrip(&mut state).context("listing windows")?;
        queue.roundtrip(&mut state).context("listing windows")?;
        Ok(Self {
            _conn: conn,
            queue,
            managers,
            state,
        })
    }

    /// The open windows with their toplevels' indices.
    fn windows(&self) -> Vec<(usize, Info)> {
        self.state
            .toplevels
            .iter()
            .enumerate()
            .map(|(index, handle)| (index, info_of(handle)))
            .filter(|(_, info)| info.done && !info.closed)
            .collect()
    }

    /// The size the compositor gives a window's picture, from a throwaway
    /// capture session (which needs neither Hyprland nor a frame).
    fn picture_size(&mut self, toplevel: usize) -> Option<(u32, u32)> {
        let qh = self.queue.handle();
        let source =
            self.managers
                .source
                .create_source(self.state.toplevels.get(toplevel)?, &qh, ());
        let session = self
            .managers
            .copy
            .create_session(&source, Options::empty(), &qh, PROBE_ID);
        source.destroy();
        self.state.probed = None;
        let answered = self.queue.roundtrip(&mut self.state).is_ok();
        session.destroy();
        let _ = self.queue.flush();
        self.state.probed.take().filter(|_| answered)
    }
}

// ---------------------------------------------------------------------------
// What a source names.
// ---------------------------------------------------------------------------

/// What to follow.
#[derive(Debug, Clone)]
enum Target {
    Window {
        app: String,
        title: Option<String>,
    },
    /// Every window of the app on one display.
    Application {
        app: String,
        display: Display,
    },
}

impl Target {
    fn parse(target: ScreenTarget<'_>) -> Result<Self> {
        Ok(match target {
            ScreenTarget::Window { app, title } => Target::Window {
                app: app.to_string(),
                title: title.map(str::to_string),
            },
            ScreenTarget::Application { app, display } => Target::Application {
                app: app.to_string(),
                display: system::find_display(display)?,
            },
            ScreenTarget::Display(_) => bail!("a display is not a window"),
        })
    }

    /// The error for a source that matches nothing now.
    fn nothing_found(&self, infos: &[Info]) -> anyhow::Error {
        match self {
            Target::Window { app, title } => anyhow!(
                "no window of {app:?}{} is open; windows: {}",
                title
                    .as_ref()
                    .map_or(String::new(), |t| format!(" titled {t:?}")),
                choices(infos)
            ),
            Target::Application { app, display } => anyhow!(
                "no window of {app:?} is open on display {:?} (id {}); windows: {}",
                display.name,
                display.id,
                choices(infos)
            ),
        }
    }
}

/// What a source check can say without capturing: which window it would
/// follow and the size the take would have.
pub(crate) fn probe(target: ScreenTarget<'_>, fps: u32) -> Result<isocast_engine::Probe> {
    let target = Target::parse(target)?;
    let mut listing = Listing::open()?;
    let live = listing.windows();
    let infos: Vec<Info> = live.iter().map(|(_, info)| info.clone()).collect();
    let clients = hyprland::clients().unwrap_or_default();
    let monitors = hyprland::monitors().unwrap_or_default();
    let candidates: Vec<Candidate> = infos
        .iter()
        .map(|info| Candidate {
            client: client_for(info, &clients).cloned(),
            info: info.clone(),
        })
        .collect();
    match &target {
        Target::Window { app, title } => {
            let Some(found) = best(&candidates, app, title.as_deref()) else {
                return Err(target.nothing_found(&infos));
            };
            let candidate = &candidates[found];
            // The compositor's own size for the picture; failing that,
            // Hyprland's, which is in layout units where the picture is in
            // pixels. With neither, the size is not said rather than
            // guessed.
            let size = listing.picture_size(live[found].0).or_else(|| {
                let client = candidate.client.as_ref()?;
                let scale = monitors
                    .iter()
                    .find(|m| m.id == client.monitor)
                    .map_or(1.0, |monitor| monitor.scale);
                Some((
                    (f64::from(client.size.0) * scale).round() as u32,
                    (f64::from(client.size.1) * scale).round() as u32,
                ))
            });
            Ok(isocast_engine::Probe {
                device: match candidate.info.title.as_str() {
                    "" => candidate.info.app_id.clone(),
                    title => format!("{} — {title}", candidate.info.app_id),
                },
                video: size.map(|(width, height)| CaptureFormat {
                    width: pixels::even(width),
                    height: pixels::even(height),
                    fps,
                }),
                channels: None,
            })
        }
        Target::Application { app, display } => {
            let on_display = windows_on(&candidates, app, display, &monitors);
            if on_display.is_empty() {
                return Err(target.nothing_found(&infos));
            }
            Ok(isocast_engine::Probe {
                device: format!("{app} on {}", display.name),
                video: Some(CaptureFormat {
                    width: pixels::even(display.width),
                    height: pixels::even(display.height),
                    fps,
                }),
                channels: None,
            })
        }
    }
}

/// The candidates that are windows of `app` on `display` (by Hyprland's
/// placement; a window it does not know is not placed anywhere).
fn windows_on(
    candidates: &[Candidate],
    app: &str,
    display: &Display,
    monitors: &[Monitor],
) -> Vec<usize> {
    candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| app_matches(&c.info.app_id, app))
        .filter(|(_, c)| {
            // On the display's screen now: on the workspace it shows (or
            // the special one open over it), as macOS leaves other
            // spaces' windows out.
            c.client.as_ref().is_some_and(|client| {
                monitors.iter().any(|m| {
                    m.id == client.monitor
                        && m.name == display.id
                        && (client.workspace == m.shown.0 || client.workspace == m.shown.1)
                })
            })
        })
        .map(|(index, _)| index)
        .collect()
}

// ---------------------------------------------------------------------------
// The capture.
// ---------------------------------------------------------------------------

/// A running window capture. Dropping it wakes the capture's loop and
/// joins its thread.
pub(crate) struct WindowCapture {
    wake: OwnedFd,
    stopping: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for WindowCapture {
    fn drop(&mut self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let one = 1u64.to_ne_bytes();
        // SAFETY: an eventfd takes an 8-byte write; the descriptor is open.
        unsafe {
            libc::write(self.wake.as_raw_fd(), one.as_ptr().cast(), one.len());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start capturing the window or app `target` names, and wait for its
/// first picture: the format a take records at is that picture's size.
pub(crate) fn capture(
    platform: LinuxPlatform,
    target: ScreenTarget<'_>,
    fps: u32,
    cursor: bool,
    sink: VideoSink<LinuxFrame>,
) -> Result<(WindowCapture, CaptureFormat)> {
    let target = Target::parse(target)?;
    // SAFETY: a fresh eventfd, owned from here on.
    let wake = unsafe {
        let fd = libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK);
        if fd < 0 {
            bail!("eventfd: {}", std::io::Error::last_os_error());
        }
        OwnedFd::from_raw_fd(fd)
    };
    let watch = wake
        .try_clone()
        .context("duplicating the wake descriptor")?;
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (report, reported) = mpsc::channel();
    let thread = isocast_diagnostics::spawn(
        "window capture",
        {
            let stopping = stopping.clone();
            move || run(platform, target, fps, cursor, sink, watch, stopping, report)
        },
        |why| isocast_diagnostics::diagnostic!("error"; "the window capture thread stopped: {why}"),
    )
    .context("starting the window capture thread")?;
    let capture = WindowCapture {
        wake,
        stopping,
        thread: Some(thread),
    };
    // The loop reports once it has a picture; its own bound is shorter.
    match reported.recv_timeout(FIRST_PICTURE_WAIT + Duration::from_secs(4)) {
        Ok(Ok(format)) => Ok((capture, format)),
        Ok(Err(err)) => {
            drop(capture);
            Err(err)
        }
        Err(RecvTimeoutError::Timeout) => {
            drop(capture);
            bail!("the compositor did not answer the window capture in time")
        }
        Err(RecvTimeoutError::Disconnected) => {
            drop(capture);
            bail!("the window capture's thread stopped before it had a picture")
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    platform: LinuxPlatform,
    target: Target,
    fps: u32,
    cursor: bool,
    sink: VideoSink<LinuxFrame>,
    wake: OwnedFd,
    stopping: Arc<std::sync::atomic::AtomicBool>,
    report: Sender<Result<CaptureFormat>>,
) {
    let mut started = false;
    let result = (|| -> Result<()> {
        let mut follower = Follower::new(platform, target, fps, cursor, sink, wake, stopping)?;
        let format = follower.first_picture()?;
        started = true;
        let _ = report.send(Ok(format));
        follower.follow()
    })();
    if let Err(err) = result {
        if started {
            // The sink is the follower's, moved in; it reported nothing
            // for an error it returned. The failure after a start is the
            // capture ending.
            isocast_diagnostics::diagnostic!("error"; "window capture ended: {err:#}");
        } else {
            let _ = report.send(Err(err));
        }
    }
}

/// The loop's state.
struct Follower {
    platform: LinuxPlatform,
    target: Target,
    cursor: bool,
    interval: Duration,
    fps: u32,
    sink: VideoSink<LinuxFrame>,
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    managers: Managers,
    wake: OwnedFd,
    stopping: Arc<std::sync::atomic::AtomicBool>,
    /// The size a take records at, once known.
    canvas: Option<(u32, u32)>,
    pool: Option<CpuPool>,
    next_layer: u64,
    last_reconcile: Option<Instant>,
    last_output: Option<Instant>,
    last_picture: Instant,
    clients: Vec<Client>,
    monitors: Vec<Monitor>,
    waiting_reported: bool,
    stall_reported: bool,
    unsupported_reported: bool,
    drops: DropReport,
    /// The engine's GPU, for pictures the compositor renders into GPU
    /// buffers (opened with the first one).
    gpu: Option<(Gpu, VulkanImporter)>,
    /// Why GPU buffers are not used, once they failed: the capture keeps
    /// shared memory from then on.
    dma_refused: Option<String>,
    /// Where each layer's window was last seen (an app's canvas): a change
    /// means the canvas is out of date even if no picture is new.
    placement: Vec<Placed>,
    /// The canvas must be drawn again although no picture is new: a window
    /// of the app left, arrived or moved.
    redraw: bool,
}

impl Follower {
    fn new(
        platform: LinuxPlatform,
        target: Target,
        fps: u32,
        cursor: bool,
        sink: VideoSink<LinuxFrame>,
        wake: OwnedFd,
        stopping: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Self> {
        let (conn, queue, managers, state) = connect()?;
        let qh = queue.handle();
        let canvas = match &target {
            Target::Window { .. } => None,
            Target::Application { display, .. } => {
                Some((pixels::even(display.width), pixels::even(display.height)))
            }
        };
        let mut follower = Self {
            platform,
            target,
            cursor,
            interval: Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1))),
            fps,
            sink,
            conn,
            queue,
            qh,
            state,
            managers,
            wake,
            stopping,
            canvas,
            pool: canvas.map(|(w, h)| CpuPool::bgra(w, h)),
            next_layer: 1,
            last_reconcile: None,
            last_output: None,
            last_picture: Instant::now(),
            clients: Vec::new(),
            monitors: Vec::new(),
            waiting_reported: false,
            stall_reported: false,
            unsupported_reported: false,
            drops: DropReport::default(),
            gpu: None,
            dma_refused: std::env::var_os("ISOCAST_WINDOW_SHM")
                .map(|_| "ISOCAST_WINDOW_SHM is set".to_string()),
            placement: Vec::new(),
            redraw: false,
        };
        // The toplevels, then their descriptions.
        follower
            .queue
            .roundtrip(&mut follower.state)
            .context("listing windows")?;
        follower
            .queue
            .roundtrip(&mut follower.state)
            .context("listing windows")?;
        Ok(follower)
    }

    fn is_stopping(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait for the first picture and answer with the take's format.
    fn first_picture(&mut self) -> Result<CaptureFormat> {
        // What the open can fail with names the windows there are.
        self.refresh_hyprland();
        let infos: Vec<Info> = self
            .live_infos()
            .into_iter()
            .map(|(_, info)| info)
            .collect();
        if matches!(self.target, Target::Application { .. })
            && self.clients.is_empty()
            && !infos.is_empty()
        {
            bail!(
                "capturing an app's windows needs to know where they are, which Hyprland's IPC \
                 says: it is not answering (HYPRLAND_INSTANCE_SIGNATURE unset or stale?)"
            );
        }
        self.reconcile()?;
        if self.state.layers.is_empty() {
            return Err(self.target.nothing_found(&infos));
        }
        let deadline = Instant::now() + FIRST_PICTURE_WAIT;
        while self.last_output.is_none() {
            if self.is_stopping() {
                bail!("the capture was stopped before it had a picture");
            }
            if Instant::now() >= deadline {
                let asleep = self.monitors.iter().any(|monitor| !monitor.awake);
                bail!(
                    "the compositor delivered no picture of the window within {} s{}",
                    FIRST_PICTURE_WAIT.as_secs(),
                    if asleep {
                        ": a monitor is asleep (DPMS off), and a compositor renders nothing \
                         while its monitors sleep; wake it and try again"
                    } else {
                        " (is the window on a workspace the compositor renders?)"
                    }
                );
            }
            self.step(LOOP_TICK.min(Duration::from_millis(50)))?;
        }
        let (width, height) = self.canvas.context("the capture has no size yet")?;
        // Which window a source follows, said once per open: a source that
        // shows "the wrong window" is then told apart from one whose setup
        // names that window (a stored collection edited after its file).
        let followed: Vec<String> = self
            .state
            .layers
            .iter()
            .map(|layer| {
                let info = info_of(&layer.handle);
                format!("{} — {}", info.app_id, info.title)
            })
            .collect();
        isocast_diagnostics::diagnostic!(
            "info"; "window: capturing {} at {width}x{height} ({} fps asked, cursor {})",
            followed.join(", "),
            self.fps,
            if self.cursor { "painted" } else { "left out" }
        );
        Ok(CaptureFormat {
            width,
            height,
            fps: self.fps,
        })
    }

    /// Run until stopped, or until the compositor is gone.
    fn follow(&mut self) -> Result<()> {
        while !self.is_stopping() {
            if let Err(err) = self.step(LOOP_TICK) {
                if self.is_stopping() {
                    return Ok(());
                }
                (self.sink.stopped)(format!("the window capture stopped: {err:#}"));
                return Err(err);
            }
        }
        Ok(())
    }

    /// One turn: serve what the compositor said, send what is due, and
    /// wait (up to `limit`) for more.
    fn step(&mut self, limit: Duration) -> Result<()> {
        self.queue
            .dispatch_pending(&mut self.state)
            .context("the compositor ended the connection or reported a protocol error")?;
        if self.state.finished {
            bail!(
                "the compositor stopped offering windows (ext_foreign_toplevel_list_v1 finished)"
            );
        }
        let now = Instant::now();
        if self.state.changed
            || self
                .last_reconcile
                .is_none_or(|at| now.duration_since(at) >= RECONCILE_EVERY)
        {
            self.refresh_hyprland();
            self.reconcile()?;
        }
        self.serve_layers()?;
        self.compose()?;
        self.watch_for_stall();
        let wait = self.next_wake().min(limit);
        self.flush()?;
        self.wait_for_events(wait)
    }

    fn flush(&mut self) -> Result<()> {
        match self.conn.flush() {
            Ok(()) => Ok(()),
            Err(wayland_client::backend::WaylandError::Io(err))
                if err.kind() == std::io::ErrorKind::WouldBlock =>
            {
                Ok(())
            }
            Err(err) => Err(anyhow!("sending to the compositor failed: {err}")),
        }
    }

    /// Sleep until the compositor speaks, `wait` passes, or the capture is
    /// stopped.
    fn wait_for_events(&mut self, wait: Duration) -> Result<()> {
        let Some(guard) = self.queue.prepare_read() else {
            // Events are already queued.
            return Ok(());
        };
        let mut fds = [
            libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let millis = wait.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: two valid pollfds for the call's duration.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, millis.max(1)) };
        if ready > 0 && fds[0].revents != 0 {
            match guard.read() {
                Ok(_) => {}
                // Another thread of this process drained the socket first.
                Err(wayland_client::backend::WaylandError::Io(err))
                    if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => bail!("reading from the compositor failed: {err}"),
            }
        }
        Ok(())
    }

    /// Re-read where the windows are. An answer that does not come leaves
    /// what was known: a take does not end for one failed question.
    fn refresh_hyprland(&mut self) {
        if let Some(clients) = hyprland::clients() {
            self.clients = clients;
        }
        if let Some(monitors) = hyprland::monitors() {
            self.monitors = monitors;
        }
    }

    /// The open windows with their toplevels' indices.
    fn live_infos(&self) -> Vec<(usize, Info)> {
        self.state
            .toplevels
            .iter()
            .enumerate()
            .map(|(index, handle)| (index, info_of(handle)))
            .filter(|(_, info)| info.done && !info.closed)
            .collect()
    }

    /// Make the windows being captured the windows the source names now.
    fn reconcile(&mut self) -> Result<()> {
        self.last_reconcile = Some(Instant::now());
        self.state.changed = false;
        // Closed toplevels are done with.
        self.state.toplevels.retain(|handle| {
            let closed = info_of(handle).closed;
            if closed {
                handle.destroy();
            }
            !closed
        });
        let live = self.live_infos();
        let candidates: Vec<Candidate> = live
            .iter()
            .map(|(_, info)| Candidate {
                client: client_for(info, &self.clients).cloned(),
                info: info.clone(),
            })
            .collect();
        // Which of `live` should be captured.
        let wanted: Vec<usize> = match &self.target {
            Target::Window { app, title } => {
                // The window being followed is kept while it lives, even
                // if its title has changed since it was found.
                let followed = self.state.layers.iter().find_map(|layer| {
                    live.iter()
                        .position(|(index, _)| self.state.toplevels[*index] == layer.handle)
                });
                match followed {
                    Some(position) => vec![position],
                    None => best(&candidates, app, title.as_deref())
                        .into_iter()
                        .collect(),
                }
            }
            Target::Application { app, display } => {
                windows_on(&candidates, app, display, &self.monitors)
            }
        };
        let wanted: Vec<usize> = wanted.into_iter().take(MAX_LAYERS).collect();
        // Drop the layers whose window is no longer wanted.
        let kept: Vec<ExtForeignToplevelHandleV1> = wanted
            .iter()
            .map(|position| self.state.toplevels[live[*position].0].clone())
            .collect();
        let before = self.state.layers.len();
        self.state
            .layers
            .retain(|layer| !layer.stopped && kept.contains(&layer.handle));
        let lost = before - self.state.layers.len();
        // Start the ones that have no layer yet.
        for handle in kept {
            if self.state.layers.iter().any(|layer| layer.handle == handle) {
                continue;
            }
            let layer = self.start_layer(handle);
            self.state.layers.push(layer);
        }
        if self.state.layers.is_empty() {
            if !self.waiting_reported && self.last_output.is_some() {
                self.waiting_reported = true;
                let infos: Vec<Info> = live.iter().map(|(_, info)| info.clone()).collect();
                isocast_diagnostics::diagnostic!(
                    "warn"; "window: {}; {} until one appears",
                    self.target.nothing_found(&infos),
                    match self.target {
                        Target::Window { .. } => "keeping the last picture",
                        Target::Application { .. } => "showing an empty canvas",
                    }
                );
            }
        } else {
            if self.waiting_reported {
                isocast_diagnostics::diagnostic!("info"; "window: found again, capturing resumed");
            }
            self.waiting_reported = false;
            if lost > 0 {
                isocast_diagnostics::debug!("window: {lost} window(s) no longer captured");
            }
        }
        if matches!(self.target, Target::Application { .. }) && self.state.layers.len() > 1 {
            isocast_diagnostics::debug!(
                "window: compositing {} windows of the app",
                self.state.layers.len()
            );
        }
        if matches!(self.target, Target::Application { .. }) {
            let placement: Vec<Placed> = self
                .state
                .layers
                .iter()
                .map(|layer| {
                    let client = client_for(&info_of(&layer.handle), &self.clients);
                    (
                        layer.id,
                        client.map(|client| (client.at.0, client.at.1, client.monitor)),
                    )
                })
                .collect();
            if placement != self.placement {
                self.placement = placement;
                // Before the first picture there is nothing to be out of
                // date: the first pictures draw the canvas.
                self.redraw = self.last_output.is_some();
            }
        }
        Ok(())
    }

    fn start_layer(&mut self, handle: ExtForeignToplevelHandleV1) -> Layer {
        let id = self.next_layer;
        self.next_layer += 1;
        let source = self.managers.source.create_source(&handle, &self.qh, ());
        let options = if self.cursor {
            Options::PaintCursors
        } else {
            Options::empty()
        };
        let session = self
            .managers
            .copy
            .create_session(&source, options, &self.qh, id);
        source.destroy();
        Layer {
            id,
            handle,
            session,
            pending: Constraints::default(),
            constraints: None,
            changed: false,
            stopped: false,
            frame: None,
            outcome: None,
            buffers: Vec::new(),
            dma: Vec::new(),
            layout: pixels::FORMATS[0].1,
            latest: None,
            fresh: false,
            requested: None,
            failures: 0,
        }
    }

    /// Per window: take in what the last frame brought, rebuild buffers
    /// the constraints outgrew, and ask for the next picture when it is
    /// time.
    fn serve_layers(&mut self) -> Result<()> {
        let now = Instant::now();
        let interval = self.interval;
        let whole = matches!(self.target, Target::Window { .. });
        let mut gave_up = false;
        let mut got_picture = false;
        for layer in &mut self.state.layers {
            if layer.stopped {
                continue;
            }
            if let Some(outcome) = layer.outcome.take() {
                let (frame, slot) = layer.frame.take().expect("an outcome without a frame");
                frame.destroy();
                match outcome {
                    Outcome::Ready => {
                        layer.latest = Some(slot);
                        layer.fresh = true;
                        layer.failures = 0;
                        got_picture = true;
                    }
                    Outcome::Failed(Failure::Stopped) => layer.stopped = true,
                    Outcome::Failed(Failure::Constraints) => {
                        // New constraints are on their way (or here).
                        layer.changed = true;
                    }
                    Outcome::Failed(Failure::Unknown) if !layer.dma.is_empty() => {
                        // The compositor would not render into our GPU
                        // buffers: shared memory from here on.
                        refuse_dma(
                            &mut self.dma_refused,
                            "the compositor failed to copy into a GPU buffer".into(),
                        );
                        layer.changed = true;
                    }
                    Outcome::Failed(Failure::Unknown) => {
                        layer.failures += 1;
                        if layer.failures >= MAX_FAILURES {
                            layer.stopped = true;
                            gave_up = true;
                        }
                    }
                }
            }
            if layer.stopped {
                continue;
            }
            if layer.changed {
                layer.changed = false;
                let Some(constraints) = layer.constraints.clone() else {
                    continue;
                };
                let Some((width, height)) = constraints.size.filter(|(w, h)| *w > 0 && *h > 0)
                else {
                    continue;
                };
                let Some((code, layout)) = pixels::choose_format(&constraints.formats) else {
                    if !self.unsupported_reported {
                        self.unsupported_reported = true;
                        bail!(
                            "the compositor offers the window's picture only in shared-memory \
                             formats {:?}, none of which this build copies from (it takes \
                             ARGB8888, XRGB8888, ABGR8888 and XBGR8888); DMA-BUF import is not \
                             done (L.4)",
                            constraints.formats
                        );
                    }
                    continue;
                };
                // GPU buffers for a window picture that is the take's
                // size (or a pixel more, cut as the CPU path cuts it);
                // anything that needs scaling or compositing is drawn on
                // the CPU from shared memory.
                let canvas = self
                    .canvas
                    .unwrap_or((pixels::even(width), pixels::even(height)));
                let fits = width.checked_sub(canvas.0).is_some_and(|d| d <= 1)
                    && height.checked_sub(canvas.1).is_some_and(|d| d <= 1);
                let modifiers = constraints
                    .dmabuf
                    .iter()
                    .find(|(format, _)| *format == dmabuf::ARGB8888)
                    .map(|(_, modifiers)| modifiers.clone());
                let want_dma = whole
                    && fits
                    && self.dma_refused.is_none()
                    && self.managers.dmabuf.is_some()
                    && modifiers.is_some();
                let rebuild = if want_dma {
                    layer.dma.first().map(|b| (b.width, b.height)) != Some((width, height))
                } else {
                    !layer.dma.is_empty()
                        || layer.buffers.is_empty()
                        || layer
                            .buffers
                            .first()
                            .is_some_and(|b| (b.width, b.height) != (width, height))
                        || layer.layout != layout
                };
                if rebuild {
                    // The frame being captured wrote into what goes away.
                    if let Some((frame, _)) = layer.frame.take() {
                        frame.destroy();
                    }
                    layer.outcome = None;
                    layer.latest = None;
                    layer.fresh = false;
                    layer.layout = layout;
                    let format = wl_shm::Format::try_from(code)
                        .map_err(|_| anyhow!("an unknown shared-memory format {code:#x}"))?;
                    layer.buffers.clear();
                    layer.dma.clear();
                    if want_dma {
                        match dma_buffers(
                            &mut self.gpu,
                            &self.managers,
                            &self.qh,
                            &constraints,
                            (width, height),
                            modifiers.as_deref().unwrap_or_default(),
                        ) {
                            Ok(buffers) => {
                                layer.dma = buffers;
                                isocast_diagnostics::diagnostic!(
                                    "info"; "window: the compositor renders the picture into \
                                     GPU buffers ({width}x{height})"
                                );
                            }
                            Err(err) => refuse_dma(&mut self.dma_refused, format!("{err:#}")),
                        }
                    }
                    if layer.dma.is_empty() {
                        for _ in 0..2 {
                            layer.buffers.push(
                                ShmBuffer::new(&self.managers.shm, &self.qh, width, height, format)
                                    .context("allocating a buffer for the window's picture")?,
                            );
                        }
                    }
                    if self.canvas.is_none() {
                        // The first window sets the size of the take.
                        let size = (pixels::even(width), pixels::even(height));
                        self.canvas = Some(size);
                        self.pool = Some(CpuPool::bgra(size.0, size.1));
                    }
                }
            }
            if layer.due_in(now, interval) == Some(Duration::ZERO) {
                let slot = layer.latest.map_or(0, |latest| 1 - latest);
                let frame = layer.session.create_frame(&self.qh, layer.id);
                if let Some(buffer) = layer.dma.get_mut(slot) {
                    // Our copy out of this buffer has to be done before
                    // the compositor renders the next picture into it
                    // (it is long done in practice: one frame ago).
                    if let (Some(busy), Some((gpu, _))) = (buffer.busy.take(), self.gpu.as_ref()) {
                        let _ = gpu.device.poll(wgpu::PollType::Wait {
                            submission_index: Some(busy),
                            timeout: Some(Duration::from_secs(1)),
                        });
                    }
                    frame.attach_buffer(&buffer.buffer);
                    frame.damage_buffer(0, 0, buffer.width as i32, buffer.height as i32);
                } else {
                    let buffer = &layer.buffers[slot];
                    frame.attach_buffer(&buffer.buffer);
                    frame.damage_buffer(0, 0, buffer.width as i32, buffer.height as i32);
                }
                frame.capture();
                layer.frame = Some((frame, slot));
                layer.requested = Some(now);
            }
        }
        if got_picture {
            self.last_picture = now;
            self.stall_reported = false;
        }
        if gave_up {
            isocast_diagnostics::diagnostic!(
                "warn"; "window: the compositor kept failing to copy a window's picture; \
                that window is dropped from the capture until it is found again"
            );
        }
        Ok(())
    }

    /// Draw the windows whose pictures are new onto the canvas, and hand
    /// the result on.
    fn compose(&mut self) -> Result<()> {
        if !self.redraw && !self.state.layers.iter().any(|layer| layer.fresh) {
            return Ok(());
        }
        let now = Instant::now();
        // Pictures are asked for no faster than the ask, so this only
        // holds back several windows' pictures landing at once.
        if self
            .last_output
            .is_some_and(|at| now.duration_since(at) < self.interval / 2)
        {
            return Ok(());
        }
        let (Some(canvas), Some(pool)) = (self.canvas, self.pool.as_mut()) else {
            return Ok(());
        };
        // A window in GPU buffers: its picture is one GPU copy into the
        // frame's texture, and the CPU never sees it unless a consumer
        // reads it back.
        if matches!(self.target, Target::Window { .. })
            && let Some(layer) = self
                .state
                .layers
                .iter_mut()
                .find(|layer| !layer.dma.is_empty() && layer.latest.is_some())
            && let Some((gpu, importer)) = self.gpu.as_mut()
        {
            let latest = layer.latest.expect("checked above");
            let taken = pool
                .take()
                .context("taking a frame for the window's picture")?;
            self.last_output = Some(now);
            for layer in &mut self.state.layers {
                layer.fresh = false;
            }
            let Some(frame) = taken else {
                self.drops.count();
                return Ok(());
            };
            let layer = self
                .state
                .layers
                .iter_mut()
                .find(|layer| !layer.dma.is_empty())
                .expect("found above");
            let source = &mut layer.dma[latest];
            let target = importer
                .target(&frame)
                .context("the window's frame on the GPU")?;
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            encoder.copy_texture_to_texture(
                source.texture.as_image_copy(),
                target.as_image_copy(),
                wgpu::Extent3d {
                    width: canvas.0,
                    height: canvas.1,
                    depth_or_array_layers: 1,
                },
            );
            source.busy = Some(gpu.queue.submit([encoder.finish()]));
            // The texels are the picture; the CPU bytes are not.
            frame.set_gpu_synced(frame.cpu_version());
            self.drops.reset();
            self.redraw = false;
            (self.sink.frame)(frame, self.platform.now());
            return Ok(());
        }
        // Back to front: tiled windows under floating ones, the focused
        // one on top.
        let mut order: Vec<usize> = (0..self.state.layers.len())
            .filter(|index| self.state.layers[*index].latest.is_some())
            .collect();
        let placed: Vec<Option<Client>> = self
            .state
            .layers
            .iter()
            .map(|layer| client_for(&info_of(&layer.handle), &self.clients).cloned())
            .collect();
        order.sort_by_key(|index| {
            placed[*index].as_ref().map_or((false, 0), |client| {
                (client.floating, -client.focus_history)
            })
        });
        let whole = matches!(self.target, Target::Window { .. });
        let drawn: Vec<(Rect, Picture<'_>)> = order
            .iter()
            .filter_map(|index| {
                let layer = &self.state.layers[*index];
                let buffer = &layer.buffers[layer.latest?];
                let picture = Picture {
                    bytes: buffer.map.bytes(),
                    stride: buffer.stride(),
                    width: buffer.width,
                    height: buffer.height,
                    layout: layer.layout,
                };
                let rect = if whole {
                    pixels::fit((buffer.width, buffer.height), canvas)
                } else {
                    let client = placed[*index].as_ref()?;
                    let monitor = self.monitors.iter().find(|m| m.id == client.monitor)?;
                    Rect {
                        x: (f64::from(client.at.0 - monitor.x) * monitor.scale).round() as i32,
                        y: (f64::from(client.at.1 - monitor.y) * monitor.scale).round() as i32,
                        w: buffer.width,
                        h: buffer.height,
                    }
                };
                Some((rect, picture))
            })
            .collect();
        let covers = |rect: &Rect| {
            rect.x <= 0
                && rect.y <= 0
                && rect.x + rect.w as i32 >= canvas.0 as i32
                && rect.y + rect.h as i32 >= canvas.1 as i32
        };
        let opaque_cover = drawn.len() == 1 && covers(&drawn[0].0) && !drawn[0].1.layout.alpha;
        // A window drawn at exactly the canvas's size replaces every pixel
        // (its alpha is copied, not blended), so clearing first is 14 MB
        // a frame of writes for nothing at 2528x1374: memory traffic that
        // showed as missed frames in the compositor.
        let replaces_all = whole
            && drawn.len() == 1
            && drawn[0].0
                == Rect {
                    x: 0,
                    y: 0,
                    w: canvas.0,
                    h: canvas.1,
                };
        let taken = pool.draw(&mut |bytes, stride| {
            // Reused frames hold an older picture: what a window does not
            // cover starts clear (transparent), unless one opaque picture
            // covers all of it.
            if !opaque_cover && !replaces_all {
                bytes.fill(0);
            }
            for (rect, picture) in &drawn {
                pixels::blit(bytes, stride, canvas, *rect, picture, !whole);
            }
        });
        drop(drawn);
        match taken {
            Ok(Some(frame)) => {
                self.drops.reset();
                self.last_output = Some(now);
                self.redraw = false;
                for layer in &mut self.state.layers {
                    layer.fresh = false;
                }
                (self.sink.frame)(frame, self.platform.now());
            }
            // Every frame is still downstream: the next picture brings
            // another chance.
            Ok(None) => {
                self.drops.count();
                self.last_output = Some(now);
                for layer in &mut self.state.layers {
                    layer.fresh = false;
                }
            }
            Err(err) => return Err(err.context("drawing the window's picture")),
        }
        Ok(())
    }

    /// When the loop next has something to do by itself.
    fn next_wake(&self) -> Duration {
        let now = Instant::now();
        let mut wake = LOOP_TICK;
        for layer in &self.state.layers {
            if let Some(due) = layer.due_in(now, self.interval) {
                wake = wake.min(due);
            }
        }
        if (self.redraw || self.state.layers.iter().any(|layer| layer.fresh))
            && let Some(at) = self.last_output
        {
            wake = wake.min((at + self.interval / 2).saturating_duration_since(now));
        }
        wake
    }

    /// A compositor that sends nothing for a while is either looking at an
    /// unchanging window (normal: it answers a capture when the window
    /// changes) or rendering nothing at all (a sleeping monitor): only the
    /// second is worth saying.
    fn watch_for_stall(&mut self) {
        if self.stall_reported
            || self.state.layers.is_empty()
            || self.last_picture.elapsed() < STALL_AFTER
        {
            return;
        }
        if self.monitors.iter().any(|monitor| !monitor.awake) {
            self.stall_reported = true;
            isocast_diagnostics::diagnostic!(
                "warn"; "window: no picture for {} s and a monitor is asleep (DPMS off): the \
                compositor renders nothing while it sleeps, so the window is frozen on its last \
                picture until the monitor wakes",
                self.last_picture.elapsed().as_secs()
            );
        }
    }
}

/// Two GPU buffers for a `size` window picture, on the engine's GPU
/// (opened the first time).
fn dma_buffers(
    gpu: &mut Option<(Gpu, VulkanImporter)>,
    managers: &Managers,
    qh: &QueueHandle<State>,
    constraints: &Constraints,
    size: (u32, u32),
    modifiers: &[u64],
) -> Result<Vec<DmaBuffer>> {
    let dmabuf_global = managers
        .dmabuf
        .as_ref()
        .context("the compositor offers no zwp_linux_dmabuf_v1")?;
    if gpu.is_none() {
        let opened = Gpu::new().context("opening the GPU for window pictures")?;
        if !dmabuf::importable(&opened) {
            bail!("the GPU's Vulkan driver cannot import DMA-BUFs");
        }
        let importer = VulkanImporter::new(&opened)?;
        *gpu = Some((opened, importer));
    }
    let (gpu, _) = gpu.as_ref().expect("opened above");
    let allocator = dmabuf::Allocator::for_device(constraints.device)?;
    (0..2)
        .map(|_| DmaBuffer::new(gpu, dmabuf_global, qh, allocator, size, modifiers))
        .collect()
}

/// GPU buffers are given up for this capture, said once.
fn refuse_dma(refused: &mut Option<String>, why: String) {
    if refused.is_none() {
        isocast_diagnostics::diagnostic!(
            "info"; "window: copying the picture through shared memory ({why})"
        );
        *refused = Some(why);
    }
}

/// How many pictures the pool was too full to take, reported rarely.
#[derive(Default)]
struct DropReport {
    dropped: u64,
    since: Option<Instant>,
}

impl DropReport {
    fn count(&mut self) {
        self.dropped += 1;
        let since = *self.since.get_or_insert_with(Instant::now);
        let elapsed = since.elapsed();
        if elapsed >= DROP_REPORT {
            isocast_diagnostics::diagnostic!(
                "warn"; "window: dropped {} picture(s) in {:.0} s: every frame in the pool is \
                still downstream",
                self.dropped,
                elapsed.as_secs_f64()
            );
            self.since = Some(Instant::now());
            self.dropped = 0;
        }
    }

    fn reset(&mut self) {
        self.since = None;
        self.dropped = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(app_id: &str, title: &str) -> Info {
        Info {
            app_id: app_id.into(),
            title: title.into(),
            identifier: format!("id-{app_id}-{title}"),
            done: true,
            closed: false,
        }
    }

    fn client(class: &str, title: &str, size: (i32, i32), focus: i64) -> Client {
        Client {
            stable_id: format!("id-{class}-{title}"),
            class: class.into(),
            title: title.into(),
            at: (0, 0),
            size,
            monitor: 1,
            workspace: 1,
            focus_history: focus,
            floating: false,
        }
    }

    fn candidates(windows: &[(&str, &str, (i32, i32), i64)]) -> Vec<Candidate> {
        let clients: Vec<Client> = windows
            .iter()
            .map(|(class, title, size, focus)| client(class, title, *size, *focus))
            .collect();
        windows
            .iter()
            .map(|(class, title, ..)| {
                let info = info(class, title);
                Candidate {
                    client: client_for(&info, &clients).cloned(),
                    info,
                }
            })
            .collect()
    }

    #[test]
    fn apps_match_by_name_or_part_of_it_ignoring_case() {
        assert!(app_matches("foot", "foot"));
        assert!(app_matches("com.mitchellh.ghostty", "GHOSTTY"));
        assert!(!app_matches("foot", "kitty"));
        assert!(title_matches("isocast test", Some("TEST")));
        assert!(title_matches("anything", None));
        assert!(!title_matches("one", Some("two")));
    }

    #[test]
    fn the_largest_match_wins_then_the_most_recently_focused() {
        let all = candidates(&[
            ("foot", "small", (400, 300), 2),
            ("foot", "big", (1000, 800), 3),
            ("other", "bigger", (2000, 2000), 0),
            ("foot", "same size, older focus", (1000, 800), 5),
        ]);
        assert_eq!(best(&all, "foot", None), Some(1));
        assert_eq!(best(&all, "foot", Some("small")), Some(0));
        assert_eq!(best(&all, "foot", Some("nope")), None);
        assert_eq!(best(&all, "OTHER", None), Some(2));
    }

    #[test]
    fn a_window_hyprland_does_not_know_still_matches() {
        let info = info("foot", "x");
        let all = vec![Candidate { info, client: None }];
        assert_eq!(best(&all, "foot", None), Some(0));
    }

    #[test]
    fn choices_are_sorted_and_unique() {
        let infos = vec![
            info("foot", "b"),
            info("foot", "a"),
            info("foot", "a"),
            info("", "x"),
        ];
        assert_eq!(choices(&infos), "foot — a, foot — b");
        assert_eq!(choices(&[]), "none");
    }

    #[test]
    fn an_apps_windows_are_those_on_the_workspaces_the_display_shows() {
        let monitor = Monitor {
            id: 1,
            name: "DP-3".into(),
            width: 2560,
            height: 1440,
            x: 0,
            y: 0,
            scale: 1.0,
            shown: (1, -98),
            awake: true,
        };
        let display = Display {
            id: "DP-3".into(),
            name: "Main".into(),
            width: 2560,
            height: 1440,
            main: true,
            built_in: false,
            refresh_rate: 60.0,
        };
        let mut all = candidates(&[
            ("foot", "shown", (10, 10), 0),
            ("foot", "other workspace", (10, 10), 1),
            ("foot", "special workspace over it", (10, 10), 2),
            ("kitty", "another app", (10, 10), 3),
        ]);
        let place = |candidate: &mut Candidate, workspace: i64| {
            candidate.client.as_mut().unwrap().workspace = workspace;
        };
        place(&mut all[1], 2);
        place(&mut all[2], -98);
        assert_eq!(
            windows_on(&all, "foot", &display, std::slice::from_ref(&monitor)),
            [0, 2]
        );
        // A window Hyprland does not know is not placed anywhere.
        all[0].client = None;
        assert_eq!(windows_on(&all, "foot", &display, &[monitor]), [2]);
    }

    #[test]
    fn clients_join_toplevels_by_id_then_by_class_and_title() {
        let mut one = client("foot", "a", (1, 1), 0);
        one.stable_id = "abc".into();
        let two = client("foot", "b", (2, 2), 1);
        let clients = vec![one, two];
        let by_id = Info {
            identifier: "ABC".into(),
            ..info("foot", "renamed since")
        };
        assert_eq!(client_for(&by_id, &clients).unwrap().title, "a");
        let by_name = Info {
            identifier: "unknown".into(),
            ..info("foot", "b")
        };
        assert_eq!(client_for(&by_name, &clients).unwrap().title, "b");
        assert!(client_for(&info("foot", "c"), &clients).is_none());
    }
}
