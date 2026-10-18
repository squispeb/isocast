//! Asynchronous native H.264/HEVC from D3D11 textures. Four pictures at most
//! remain leased to NVENC; encoded bytes, never raw pixels, cross to the CPU.

use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_void};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{EncodedVideo, LiveVideoSpec, Time, VideoEncoder, VideoTrackSpec};
use isocast_protocol::Codec;
use parking_lot::{Condvar, Mutex};
use windows::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};
use windows::core::Interface;

use crate::encoder::Parameters;
use crate::h264::MAX_PACKET;
use crate::native::Event;
use crate::{Frame, GraphicsDevice, PixelFormat, VideoEncoding};

// Matches native/nvenc.cpp: asynchronous output needs 4 + B-frame count.
// B-frames and lookahead are disabled for this encoder.
const IN_FLIGHT: usize = 4;
const STALL_WAIT: Duration = Duration::from_millis(400);
const FINISH_WAIT: Duration = Duration::from_secs(5);
const READY_PACKETS: usize = 2 * IN_FLIGHT;
const BUSY_RETRY: Duration = Duration::from_millis(2);

/// One output that will need its own encoder. Packet-sharing outputs must
/// already have been coalesced by the caller, before planning or opening any
/// files/encoders. IDs are stable engine output IDs, not display names.
#[derive(Debug, Clone)]
pub struct NvencBudgetOutput {
    pub id: String,
    pub video: VideoTrackSpec,
    pub iso: bool,
}

/// A pre-open estimate, not a reservation of GPU capacity against other apps.
/// The caller must report the selected presets before applying them. Codec,
/// dimensions, frame rate and quantizer are never changed by this plan.
/// Serialize plan-and-open across this process's recording coordinators;
/// two concurrently prepared plans do not reserve capacity from each other.
#[derive(Debug, Clone)]
pub struct NvencBudgetPlan {
    pub presets: std::collections::BTreeMap<String, u32>,
    pub estimated_load: f64,
    pub calibration_key: String,
    pub calibrated: bool,
}

/// Per-driver throughput in 1080p-equivalent frames/s, for P1/P3/P5.
/// Reference values are NVIDIA SDK13's *low-latency* single-engine table,
/// not the very different HEVC HQ/P3 figure. They are conservative estimates,
/// not measurements of every picture/quantizer. Keep 15% scheduling headroom.
#[derive(Debug, Clone)]
pub struct NvencBudgetProfile {
    key: String,
    h264: [f64; 3],
    hevc: [f64; 3],
    calibrated: bool,
}

impl NvencBudgetProfile {
    pub fn reference(device: &GraphicsDevice) -> Result<Self> {
        let (key, vendor, model) = budget_identity(device)?;
        if (vendor, model) != (0x10de, 0x2f04) {
            bail!(
                "encoder.budget: no qualified NVENC engine-count/profile for adapter {key}; calibration required"
            );
        }
        Ok(Self {
            key,
            h264: [977.0, 718.0, 323.0],
            hevc: [1134.0, 529.0, 506.0],
            calibrated: false,
        })
    }

    /// Cache key includes PCI subsystem/revision and the actual UMD version.
    /// A driver update invalidates measured values; never key by GPU name.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Apply a completed calibration from the caller's per-driver cache.
    /// Values must use this same LL/single-pass/no-B/no-lookahead policy and
    /// P1/P3/P5 order. The calibration runner owns content/clock qualification;
    /// this API does not relabel reference estimates as measured results.
    pub fn calibrated(mut self, key: &str, h264: [f64; 3], hevc: [f64; 3]) -> Result<Self> {
        if key != self.key {
            bail!("encoder.budget.calibration: adapter/driver changed; recalibrate");
        }
        for rates in [h264, hevc] {
            if rates.iter().any(|rate| !rate.is_finite() || *rate <= 0.0) {
                bail!(
                    "encoder.budget.calibration: P1/P3/P5 capacities must be finite and positive"
                );
            }
        }
        // Preserve each labelled measurement, including small inversions.
        // Preset order is not a throughput guarantee for every workload.
        self.h264 = h264;
        self.hevc = hevc;
        self.calibrated = true;
        Ok(self)
    }

    pub fn plan(
        &self,
        device: &GraphicsDevice,
        outputs: &[NvencBudgetOutput],
        allow_lighter_program: bool,
    ) -> Result<NvencBudgetPlan> {
        if budget_identity(device)?.0 != self.key {
            bail!("encoder.budget: adapter/driver changed since profiling; replan");
        }
        if !outputs.is_empty() && NvencEncoder::active_sessions() != 0 {
            bail!(
                "encoder.budget: existing NVENC workload is not included in this batch; plan all outputs before opening any encoder"
            );
        }
        // A deliberate bound on this planner, not NVIDIA's licensing limit.
        // In particular, the qualified 5070/617.42 admits 12 sessions, not the
        // older SDK note's eight. Native open still validates driver capacity.
        if outputs.len() > 12 {
            bail!(
                "encoder.budget.outputs: this planner supports at most 12 outputs per batch; the driver may admit fewer, including sessions used by other apps"
            );
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut costs = Vec::with_capacity(outputs.len());
        for output in outputs {
            if output.id.is_empty() || !ids.insert(&output.id) {
                bail!("encoder.budget.outputs.id: IDs must be nonempty and unique");
            }
            let spec = &output.video;
            if spec.width == 0
                || spec.height == 0
                || !spec.width.is_multiple_of(2)
                || !spec.height.is_multiple_of(2)
                || !(1..=240).contains(&spec.fps)
            {
                bail!(
                    "encoder.budget.outputs.{}: positive even dimensions and fps 1..=240 required",
                    output.id
                );
            }
            if spec.encoder.effective_bit_depth() != 8 {
                bail!(
                    "encoder.budget.outputs.{}.bit_depth: 10-bit throughput is not qualified by this 8-bit profile",
                    output.id
                );
            }
            let rates = match spec.encoder.codec {
                Codec::H264 => self.h264,
                Codec::Hevc => self.hevc,
                _ => bail!(
                    "encoder.budget.outputs.{}.codec: valid NVENC profile codecs are h264 and hevc",
                    output.id
                ),
            };
            let pixels = u64::from(spec.width)
                .checked_mul(u64::from(spec.height))
                .and_then(|area| area.checked_mul(u64::from(spec.fps)))
                .context("encoder.budget: pixel-rate overflow")? as f64
                / (1920.0 * 1080.0);
            costs.push(rates.map(|rate| pixels / rate));
        }
        let mut isos: Vec<_> = outputs
            .iter()
            .enumerate()
            .filter(|(index, output)| output.iso && costs[*index][0] < costs[*index][1])
            .map(|(index, _)| index)
            .collect();
        // Save the most engine time first, with stable input order for ties.
        isos.sort_by(|&a, &b| (costs[b][1] - costs[b][0]).total_cmp(&(costs[a][1] - costs[a][0])));
        for program in if allow_lighter_program {
            &[2usize, 1, 0][..]
        } else {
            &[2usize][..]
        } {
            let mut selected: Vec<_> = outputs
                .iter()
                .map(|output| if output.iso { 1 } else { *program })
                .collect();
            for next in 0..=isos.len() {
                let load: f64 = costs
                    .iter()
                    .zip(&selected)
                    .map(|(cost, &preset)| cost[preset])
                    .sum();
                if load <= 0.85 {
                    return Ok(NvencBudgetPlan {
                        presets: outputs
                            .iter()
                            .zip(&selected)
                            .map(|(output, &preset)| (output.id.clone(), [1, 3, 5][preset]))
                            .collect(),
                        estimated_load: load,
                        calibration_key: self.key.clone(),
                        calibrated: self.calibrated,
                    });
                }
                if let Some(&iso) = isos.get(next) {
                    selected[iso] = 0;
                }
            }
        }
        bail!(
            "encoder.budget: requested outputs exceed 85% of the single-NVENC estimate; reduce outputs/resolution/fps or explicitly allow lighter program presets (codec/quality were not changed)"
        )
    }
}

fn budget_identity(device: &GraphicsDevice) -> Result<(String, u32, u32)> {
    let dxgi: IDXGIDevice = device.raw().cast()?;
    let adapter = unsafe { dxgi.GetAdapter() }?;
    let desc = unsafe { adapter.GetDesc() }?;
    // IDXGIDevice queries the UMD package version; do not pass an ID3D11 IID,
    // which CheckInterfaceSupport deliberately does not support.
    let driver = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }
        .context("encoder.budget: cannot identify the display driver version")?;
    Ok((
        format!(
            "{:04x}:{:04x}:{:08x}:{:02x}:{driver:016x}",
            desc.VendorId, desc.DeviceId, desc.SubSysId, desc.Revision
        ),
        desc.VendorId,
        desc.DeviceId,
    ))
}

#[repr(C)]
struct Config {
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    constant_bitrate: u32,
    keyframe_interval: u32,
    codec: u32,
    input_depth: u32,
    output_depth: u32,
    quality_qp: u32,
    input_format: u32,
    preset: u32,
}

#[repr(C)]
#[derive(Default)]
struct Packet {
    data: *const u8,
    pts: u64,
    size: u32,
    keyframe: u32,
}

const _: () = assert!(std::mem::size_of::<Config>() == 48);
const _: () = assert!(std::mem::size_of::<Packet>() == 24);

unsafe extern "C" {
    fn isocast_nvenc_open(
        device: *mut c_void,
        spec: *const Config,
        error: *mut c_char,
        size: usize,
    ) -> *mut c_void;
    fn isocast_nvenc_error(encoder: *mut c_void) -> *const c_char;
    fn isocast_nvenc_sessions() -> u32;
    fn isocast_nvenc_headers(encoder: *mut c_void, size: *mut u32) -> *const u8;
    fn isocast_nvenc_submit(
        encoder: *mut c_void,
        texture: *mut c_void,
        pts: u64,
        keyframe: u32,
    ) -> i32;
    fn isocast_nvenc_wait_event(encoder: *mut c_void, kind: *mut u32) -> *mut c_void;
    fn isocast_nvenc_event_ready(encoder: *mut c_void, kind: u32) -> i32;
    fn isocast_nvenc_lock(encoder: *mut c_void, packet: *mut Packet) -> i32;
    fn isocast_nvenc_unlock(encoder: *mut c_void) -> i32;
    fn isocast_nvenc_flush(encoder: *mut c_void) -> i32;
    fn isocast_nvenc_set_rate(encoder: *mut c_void, fps: u32, bitrate: u32) -> i32;
    fn isocast_nvenc_close(encoder: *mut c_void) -> i32;
}

struct Pending {
    _frame: Frame,
    pts: Time,
    submitted: Instant,
}

struct Ready {
    packet: EncodedVideo,
    submitted: Instant,
    completed: Duration,
}

struct Native {
    raw: NonNull<c_void>,
    parameters: Parameters,
}

// SAFETY: every native call, including destruction, holds Session::native.
// The completion worker only try-locks this gate; a blocked submit on one
// session must not prevent completing another session on the same adapter.
unsafe impl Send for Native {}

impl Native {
    fn check(&self, status: i32) -> Result<()> {
        if status == 0 {
            return Ok(());
        }
        let error = unsafe { CStr::from_ptr(isocast_nvenc_error(self.raw.as_ptr())) };
        bail!(
            "encoder.nvenc: {} (status {status})",
            error.to_string_lossy()
        )
    }

    fn decode(&mut self, packet: &Packet, pts: Time) -> Result<EncodedVideo> {
        if packet.pts != pts.as_nanos() as u64 {
            bail!("NVENC reordered or changed a submitted timestamp");
        }
        if packet.data.is_null() || packet.size == 0 || packet.size as usize > MAX_PACKET {
            bail!("NVENC returned invalid packet size {}", packet.size);
        }
        let source = unsafe { std::slice::from_raw_parts(packet.data, packet.size as usize) };
        let decoded = self
            .parameters
            .packet(source, pts)?
            .context("NVENC packet contains no picture")?;
        if decoded.keyframe != (packet.keyframe != 0) {
            bail!("NVENC keyframe flag disagrees with its picture");
        }
        Ok(decoded)
    }
}

#[derive(Default)]
struct State {
    pending: VecDeque<Pending>,
    // Retain an attempted input even if submission fails with uncertain native
    // ownership. In particular this must survive a future direct-input path.
    submitting: Option<Pending>,
    ready: VecDeque<Ready>,
    signal: Option<u32>,
    retry: Option<Instant>,
    output_ready: bool,
    flushed: bool,
    detached: bool,
    failed: Option<String>,
    stats: EncodeStats,
    completion_latency: Duration,
}

impl State {
    fn healthy(&self) -> Result<()> {
        if let Some(error) = &self.failed {
            bail!("NVENC encoder failed: {error}");
        }
        Ok(())
    }

    fn take_ready(&mut self) -> Option<EncodedVideo> {
        let ready = self.ready.pop_front()?;
        // Keep the original observation metric: it includes caller polling
        // delay. Completion latency is separate, for the SAME returned packet.
        self.stats.last_receive_latency = ready.submitted.elapsed();
        self.completion_latency = ready.completed;
        Some(ready.packet)
    }
}

struct Session {
    // Lock order: native -> state. No driver call or packet parsing holds
    // state, so receive/finish timeouts can still observe a stuck native call.
    native: Mutex<Native>,
    state: Mutex<State>,
    changed: Condvar,
    detach: AtomicBool,
    quarantined: AtomicBool,
    _device: GraphicsDevice,
}

impl Session {
    fn fail(self: &Arc<Self>, worker: &CompletionWorker, error: String) {
        self.state.lock().failed.get_or_insert(error);
        worker.failed.store(true, Ordering::Release);
        // Never free native resources, frame leases or their device after an
        // uncertain failure, including a driver call still running at timeout.
        if !self.quarantined.swap(true, Ordering::AcqRel) {
            std::mem::forget(self.clone());
        }
        self.changed.notify_all();
        worker.wake.signal();
    }
}

#[derive(Default)]
struct Registry {
    workers: Mutex<Vec<Arc<CompletionWorker>>>,
    changed: Condvar,
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

struct CompletionWorker {
    luid: (u32, i32),
    sessions: Mutex<Vec<Arc<Session>>>,
    wake: Event,
    stopping: AtomicBool,
    failed: AtomicBool,
    exited: Mutex<bool>,
    changed: Condvar,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl CompletionWorker {
    fn register(luid: (u32, i32), session: Arc<Session>) -> Result<Arc<Self>> {
        let registry = registry();
        let mut workers = registry.workers.lock();
        loop {
            if let Some(worker) = workers.iter().find(|worker| worker.luid == luid) {
                if worker.failed.load(Ordering::Acquire) {
                    bail!("NVENC adapter {luid:?} has quarantined resources; restart the process");
                }
                if worker.stopping.load(Ordering::Acquire) {
                    registry.changed.wait(&mut workers);
                    continue;
                }
                let mut sessions = worker.sessions.lock();
                // A fatal worker can retire its last session and exit while
                // registration waits for this lock. Never attach to that
                // exited worker; the first check alone cannot close the race.
                if worker.failed.load(Ordering::Acquire) {
                    bail!("NVENC adapter {luid:?} has quarantined resources; restart the process");
                }
                // One control handle and one head event/session. This is NOT
                // a driver licensing limit (5070/617.42 supports 12 sessions).
                if sessions.len() == 63 {
                    bail!("NVENC completion wait set supports at most 63 sessions per adapter");
                }
                sessions.push(session);
                worker.wake.signal();
                return Ok(worker.clone());
            }
            let worker = Arc::new(Self {
                luid,
                sessions: Mutex::new(vec![session]),
                wake: Event::new()?,
                stopping: AtomicBool::new(false),
                failed: AtomicBool::new(false),
                exited: Mutex::new(false),
                changed: Condvar::new(),
                thread: Mutex::new(None),
            });
            let run = worker.clone();
            let thread = std::thread::Builder::new()
                .name("nvenc-completion".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if let Err(error) = crate::native::video_thread() {
                            eprintln!("NVENC completion thread priority: {error:#}");
                        }
                        run.run()
                    }));
                    if !matches!(result, Ok(Ok(()))) {
                        let error = match result {
                            Ok(Err(error)) => format!("{error:#}"),
                            _ => "NVENC completion worker panicked".into(),
                        };
                        run.failed.store(true, Ordering::Release);
                        for session in run.sessions.lock().iter() {
                            session.fail(&run, error.clone());
                        }
                    }
                    *run.exited.lock() = true;
                    run.changed.notify_all();
                    for session in run.sessions.lock().iter() {
                        session.changed.notify_all();
                    }
                })?;
            *worker.thread.lock() = Some(thread);
            workers.push(worker.clone());
            return Ok(worker);
        }
    }

    fn run(&self) -> Result<()> {
        let mut rotate = 0;
        loop {
            // No prior wait snapshot is alive here. A detach acknowledgement
            // permits the caller, not this shared worker, to destroy resources.
            let mut sessions = self.sessions.lock();
            sessions.retain(|session| {
                if !session.detach.load(Ordering::Acquire) {
                    return true;
                }
                session.state.lock().detached = true;
                session.changed.notify_all();
                false
            });
            if sessions.is_empty()
                && (self.stopping.load(Ordering::Acquire) || self.failed.load(Ordering::Acquire))
            {
                return Ok(());
            }
            let mut snapshot = sessions.clone();
            drop(sessions);
            if !snapshot.is_empty() {
                let len = snapshot.len();
                snapshot.rotate_left(rotate % len);
                rotate = (rotate + 1) % len;
            }
            let mut handles = Vec::new();
            let mut owners = Vec::new();
            let mut retry: Option<Instant> = None;
            let mut progress = false;
            for session in &snapshot {
                let (signal, output_ready, due) = {
                    let state = session.state.lock();
                    if state.failed.is_some() || session.detach.load(Ordering::Acquire) {
                        continue;
                    }
                    (state.signal, state.output_ready, state.retry)
                };
                // All caller-side native gate releases signal wake. Excluding
                // a busy session cannot lose its event or require polling.
                let Some(mut native) = session.native.try_lock() else {
                    continue;
                };
                let result = (|| -> Result<()> {
                    if let Some(kind) = signal {
                        let status =
                            unsafe { isocast_nvenc_event_ready(native.raw.as_ptr(), kind) };
                        native.check(status)?;
                        let mut state = session.state.lock();
                        state.signal = None;
                        if kind == 2 {
                            state.flushed = true;
                            session.changed.notify_all();
                            return Ok(());
                        }
                        state.output_ready = true;
                    }
                    if signal == Some(1) || output_ready {
                        if let Some(due) = due.filter(|due| *due > Instant::now()) {
                            retry = Some(retry.map_or(due, |old| old.min(due)));
                            return Ok(());
                        }
                        let mut packet = Packet::default();
                        let status =
                            unsafe { isocast_nvenc_lock(native.raw.as_ptr(), &mut packet) };
                        if status == -1 {
                            let due = Instant::now() + BUSY_RETRY;
                            session.state.lock().retry = Some(due);
                            retry = Some(retry.map_or(due, |old| old.min(due)));
                            return Ok(());
                        }
                        native.check(status)?;
                        let pts = session
                            .state
                            .lock()
                            .pending
                            .front()
                            .map(|pending| pending.pts);
                        let decoded = pts
                            .context("NVENC output has no pending input")
                            .and_then(|pts| native.decode(&packet, pts));
                        // Unlock even on validation failure. Keep the input
                        // lease until BOTH unlock and unmap have succeeded.
                        let status = unsafe { isocast_nvenc_unlock(native.raw.as_ptr()) };
                        native.check(status)?;
                        let mut state = session.state.lock();
                        let pending = state
                            .pending
                            .pop_front()
                            .context("NVENC output has no pending input")?;
                        state.output_ready = false;
                        state.retry = None;
                        match decoded {
                            Ok(packet) if state.failed.is_none() => {
                                state.stats.completed += 1;
                                state.ready.push_back(Ready {
                                    packet,
                                    submitted: pending.submitted,
                                    completed: pending.submitted.elapsed(),
                                });
                            }
                            Ok(_) => {} // A timed-out caller has quarantined this session.
                            Err(error) => {
                                state.stats.dropped += 1;
                                return Err(error);
                            }
                        }
                        progress = true;
                        session.changed.notify_all();
                    }
                    let mut kind = 0;
                    let event = unsafe { isocast_nvenc_wait_event(native.raw.as_ptr(), &mut kind) };
                    if !event.is_null() {
                        handles.push(HANDLE(event));
                        owners.push((session.clone(), kind));
                    }
                    Ok(())
                })();
                drop(native);
                session.changed.notify_all();
                if let Err(error) = result {
                    session.fail(self, format!("{error:#}"));
                }
            }
            // Do not throw away a consumed auto-reset event: it remains in
            // State::signal until its session's native gate can be acquired.
            // Successful work may expose another already-signalled head.
            if progress {
                continue;
            }
            let timeout = retry.map_or(INFINITE, |due| {
                wait_millis(due.saturating_duration_since(Instant::now()))
            });
            // Windows chooses the lowest signalled index. Put control last
            // so repeated admissions cannot starve picture completions.
            // Membership/retirement is checked on every pass regardless.
            handles.push(self.wake.handle());
            let result = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
            if result == WAIT_FAILED {
                return Err(windows::core::Error::from_thread().into());
            }
            if result == WAIT_TIMEOUT || result.0 == WAIT_OBJECT_0.0 + owners.len() as u32 {
                continue;
            }
            let index = result
                .0
                .checked_sub(WAIT_OBJECT_0.0)
                .and_then(|index| owners.get(index as usize))
                .context("unexpected NVENC completion wait result")?;
            index.0.state.lock().signal = Some(index.1);
        }
    }

    fn detach(self: &Arc<Self>, session: &Arc<Session>, deadline: Instant) -> bool {
        session.detach.store(true, Ordering::Release);
        self.wake.signal();
        let mut state = session.state.lock();
        while !state.detached && !*self.exited.lock() {
            if session.changed.wait_until(&mut state, deadline).timed_out() && !state.detached {
                drop(state);
                session.fail(
                    self,
                    "NVENC completion worker did not detach within 5 seconds".into(),
                );
                return false;
            }
        }
        true
    }

    fn stop_if_idle(self: &Arc<Self>) {
        let registry = registry();
        {
            let _workers = registry.workers.lock();
            if !self.sessions.lock().is_empty() || self.stopping.swap(true, Ordering::AcqRel) {
                return;
            }
        }
        self.wake.signal();
        let deadline = Instant::now() + FINISH_WAIT;
        let mut exited = self.exited.lock();
        while !*exited {
            if self.changed.wait_until(&mut exited, deadline).timed_out() && !*exited {
                self.failed.store(true, Ordering::Release);
                registry.changed.notify_all();
                return;
            }
        }
        drop(exited);
        if let Some(thread) = self.thread.lock().take() {
            let _ = thread.join();
        }
        let mut workers = registry.workers.lock();
        if !self.failed.load(Ordering::Acquire) {
            workers.retain(|worker| !Arc::ptr_eq(worker, self));
        }
        registry.changed.notify_all();
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EncodeStats {
    pub accepted: u64,
    pub completed: u64,
    pub dropped: u64,
    pub in_flight: usize,
    pub ready_packets: usize,
    /// Time inside the driver submission, including its input synchronization.
    pub last_submit_latency: Duration,
    /// Submission to receive, including any delay before the caller polled.
    /// This is not a hardware-only encoder timestamp.
    pub last_receive_latency: Duration,
}

/// H.264 High by default, or HEVC Main/Main10; SDR Rec.709 limited range,
/// P5 low-latency, single pass, spatial AQ8, no temporal AQ/lookahead/B-frames.
/// Shared input is copied on
/// GPU into one of four private slots: NVENC forbids client access while
/// its input is mapped. Other consumers can still use the original Frame.
pub struct NvencEncoder {
    session: Arc<Session>,
    worker: Arc<CompletionWorker>,
    device: GraphicsDevice,
    size: (u32, u32),
    low_latency: bool,
    recording: bool,
    input: PixelFormat,
    preset: u32,
    last_pts: Option<Time>,
    finishing: bool,
    finish_deadline: Option<Instant>,
    finished: bool,
    flush_submitted: bool,
}

impl NvencEncoder {
    pub fn new(device: &GraphicsDevice, spec: &LiveVideoSpec) -> Result<Self> {
        Self::with_format(device, spec, VideoEncoding::H264, PixelFormat::Bgra8)
    }

    /// HEVC output has an hvcC decoder config and no H.264 `parameter_sets`.
    /// The caller must select a matching HEVC mux track, never the live FLV path.
    /// NV12 input keeps its qualified limited-range Rec.709 codes. Main10
    /// output may expand eight-bit input, but cannot add source precision.
    pub fn with_format(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        encoding: VideoEncoding,
        input: PixelFormat,
    ) -> Result<Self> {
        Self::open(device, spec, encoding, input, None, 5)
    }

    /// Recording at a native base QP (0..=51), adjusted by spatial AQ8.
    /// This is neither lossless at QP 0 nor equivalent to macOS quality.
    /// Ignores `spec.bitrate_kbps` and `spec.constant_bitrate`; changes to
    /// recording settings, including fps, require opening a new encoder.
    pub fn with_quantizer(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        encoding: VideoEncoding,
        input: PixelFormat,
        qp: u32,
    ) -> Result<Self> {
        Self::with_quantizer_preset(device, spec, encoding, input, qp, 5)
    }

    /// Constant-QP recording with an explicit NVIDIA P1..=P7 speed/quality
    /// preset. All presets retain single-pass low-latency tuning, spatial
    /// AQ8 and no B-frames/lookahead. Select before opening; never silently
    /// degrade a running recording or change its codec/quantizer.
    pub fn with_quantizer_preset(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        encoding: VideoEncoding,
        input: PixelFormat,
        qp: u32,
        preset: u32,
    ) -> Result<Self> {
        Self::open(device, spec, encoding, input, Some(qp), preset)
    }

    pub fn preset(&self) -> u32 {
        self.preset
    }

    fn open(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        encoding: VideoEncoding,
        input: PixelFormat,
        quality_qp: Option<u32>,
        preset: u32,
    ) -> Result<Self> {
        if !(1..=7).contains(&preset) {
            bail!("video.preset: NVENC valid presets are P1..=P7 (1..=7)");
        }
        encoding.validate(input)?;
        let bitrate = if let Some(qp) = quality_qp {
            if qp > 51 {
                bail!("video.quantizer: NVENC valid range is 0..=51");
            }
            validate_fps(spec.fps)?;
            0
        } else {
            bitrate(spec.fps, spec.bitrate_kbps)?
        };
        if spec.width == 0
            || spec.height == 0
            || !spec.width.is_multiple_of(2)
            || !spec.height.is_multiple_of(2)
        {
            bail!("video.size: 4:2:0 requires positive even width and height");
        }
        if spec.keyframe_interval == 0 {
            bail!("video.keyframe_interval: must be at least 1");
        }
        let config = Config {
            width: spec.width,
            height: spec.height,
            fps: spec.fps,
            bitrate,
            constant_bitrate: u32::from(quality_qp.is_none() && spec.constant_bitrate),
            keyframe_interval: spec.keyframe_interval,
            codec: u32::from(encoding != VideoEncoding::H264),
            input_depth: if input == PixelFormat::Rgba10 { 10 } else { 8 },
            output_depth: u32::from(encoding.bit_depth()),
            quality_qp: quality_qp.unwrap_or(u32::MAX),
            // Matches InputFormat in native/nvenc.cpp; storage is not bit depth.
            input_format: match input {
                PixelFormat::Bgra8 => 0,
                PixelFormat::Rgba10 => 1,
                PixelFormat::Nv12 => 2,
            },
            preset,
        };
        let mut error = [0 as c_char; 512];
        let native = unsafe {
            isocast_nvenc_open(
                device.raw().as_raw(),
                &config,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let native = NonNull::new(native).ok_or_else(|| {
            anyhow!(
                "encoder.nvenc: {}",
                unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
            )
        })?;
        let mut parameters = Parameters::new(encoding);
        let mut size = 0;
        let data = unsafe { isocast_nvenc_headers(native.as_ptr(), &mut size) };
        let headers = (|| -> Result<()> {
            if data.is_null() || size == 0 || size > 4096 {
                bail!("NVENC returned invalid decoder configuration size");
            }
            parameters.headers(unsafe { std::slice::from_raw_parts(data, size as usize) })
        })();
        if let Err(error) = headers {
            if unsafe { isocast_nvenc_close(native.as_ptr()) } != 0 {
                std::mem::forget(device.clone());
            }
            return Err(error);
        }
        let session = Arc::new(Session {
            native: Mutex::new(Native {
                raw: native,
                parameters,
            }),
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            detach: AtomicBool::new(false),
            quarantined: AtomicBool::new(false),
            _device: device.clone(),
        });
        let worker = match CompletionWorker::register(device.adapter_luid(), session.clone()) {
            Ok(worker) => worker,
            Err(error) => {
                if unsafe { isocast_nvenc_close(native.as_ptr()) } != 0 {
                    std::mem::forget(session);
                }
                return Err(error);
            }
        };
        Ok(Self {
            session,
            worker,
            device: device.clone(),
            size: (spec.width, spec.height),
            low_latency: spec.low_latency,
            recording: quality_qp.is_some(),
            input,
            preset,
            last_pts: None,
            finishing: false,
            finish_deadline: None,
            finished: false,
            flush_submitted: false,
        })
    }

    /// Sessions opened by this process, including any retained after a failed
    /// teardown. Not a guessed driver-wide session limit or availability claim.
    pub fn active_sessions() -> u32 {
        unsafe { isocast_nvenc_sessions() }
    }

    pub fn stats(&self) -> EncodeStats {
        let state = self.session.state.lock();
        EncodeStats {
            in_flight: state.pending.len(),
            ready_packets: state.ready.len(),
            ..state.stats
        }
    }

    /// Submission through validated output, unlock and unmap, for the most
    /// recently received packet. Unlike last_receive_latency, excludes time
    /// spent waiting for the caller to receive an already-completed packet.
    pub fn completion_latency(&self) -> Duration {
        self.session.state.lock().completion_latency
    }

    /// avcC or hvcC for the encoded-packet muxer. No B-frame reordering.
    pub fn decoder_config(&self) -> Result<Vec<u8>> {
        let native = self.session.native.lock();
        let result = native.parameters.config();
        drop(native);
        self.worker.wake.signal();
        result
    }

    /// Try admission without waiting for an output slot. Native mapping and
    /// submission can wait on the driver, so call on the encoder's worker.
    /// False means NOT accepted; a caller may retry, so it is not counted as
    /// a dropped picture here.
    pub fn try_encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<bool> {
        self.session.state.lock().healthy()?;
        if self.finishing {
            bail!("NVENC encoder has already begun finishing");
        }
        if self.last_pts.is_some_and(|last| pts <= last) {
            bail!("video.pts: must increase between accepted frames");
        }
        let timestamp = u64::try_from(pts.as_nanos()).context("video.pts: must be nonnegative")?;
        if self.size != (frame.width(), frame.height()) {
            bail!(
                "video.size: encoder is {}x{}, frame is {}x{}",
                self.size.0,
                self.size.1,
                frame.width(),
                frame.height()
            );
        }
        if !self.device.owns(frame) {
            bail!("video.device: NVENC input must belong to its D3D11 device");
        }
        if frame.pixel_format() != self.input {
            bail!(
                "video.input: NVENC requires {:?}, got {:?}",
                self.input,
                frame.pixel_format()
            );
        }
        let native = self.session.native.lock();
        let mut state = self.session.state.lock();
        state.healthy()?;
        if state.pending.len() == IN_FLIGHT
            || state.pending.len() + state.ready.len() == READY_PACKETS
        {
            drop(state);
            drop(native);
            self.worker.wake.signal();
            return Ok(false);
        }
        let submitted = Instant::now();
        state.submitting = Some(Pending {
            _frame: frame.clone(),
            pts,
            submitted,
        });
        drop(state);
        let status = unsafe {
            isocast_nvenc_submit(
                native.raw.as_ptr(),
                frame.texture().as_raw(),
                timestamp,
                u32::from(keyframe),
            )
        };
        let result = if status == -1 {
            Ok(())
        } else {
            native.check(status)
        };
        let mut state = self.session.state.lock();
        state.stats.last_submit_latency = submitted.elapsed();
        if result.is_ok() {
            let pending = state
                .submitting
                .take()
                .expect("reserved before native submission");
            if status == 0 {
                state.pending.push_back(pending);
                state.stats.accepted += 1;
                self.last_pts = Some(pts);
            }
        }
        drop(state);
        drop(native);
        self.session.changed.notify_all();
        self.worker.wake.signal();
        if let Err(error) = result {
            self.session.fail(&self.worker, format!("{error:#}"));
            return Err(error);
        }
        if status == -1 {
            return Ok(false);
        }
        Ok(true)
    }

    /// Flush and return every remaining compressed picture, in order. Only a
    /// successful flush permits destroying the input/output GPU resources.
    /// On failure, valid packets remain available through `receive`, followed
    /// by the sticky encoder error once that prefix has been consumed.
    pub fn finish(&mut self) -> Result<Vec<EncodedVideo>> {
        if let Err(error) = self.finish_native() {
            self.session.fail(&self.worker, format!("{error:#}"));
            return Err(error);
        }
        let mut state = self.session.state.lock();
        let mut packets = Vec::with_capacity(state.ready.len());
        while let Some(packet) = state.take_ready() {
            packets.push(packet);
        }
        Ok(packets)
    }

    fn finish_native(&mut self) -> Result<()> {
        if self.finished {
            return self.session.state.lock().healthy();
        }
        self.finishing = true;
        // Drop retries share this deadline, never adding a second five-second
        // wait after a timed-out finish. Busy flushes are transient, not loss.
        let deadline = *self
            .finish_deadline
            .get_or_insert_with(|| Instant::now() + FINISH_WAIT);
        let mut retry = Instant::now();
        loop {
            let mut state = self.session.state.lock();
            state.healthy()?;
            if state.pending.is_empty() && state.flushed {
                self.finished = true;
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("NVENC did not finish within 5 seconds");
            }
            if !self.flush_submitted && Instant::now() >= retry {
                drop(state);
                if let Some(native) = self.session.native.try_lock() {
                    let status = unsafe { isocast_nvenc_flush(native.raw.as_ptr()) };
                    let result = if status == -1 {
                        Ok(())
                    } else {
                        native.check(status)
                    };
                    drop(native);
                    self.worker.wake.signal();
                    result?;
                    self.flush_submitted = status == 0;
                }
                // Also bounds a rare race acquiring the gate after a notify.
                retry = Instant::now() + BUSY_RETRY;
                continue;
            }
            let until = if self.flush_submitted {
                deadline
            } else {
                deadline.min(retry)
            };
            self.session.changed.wait_until(&mut state, until);
        }
    }
}

impl VideoEncoder for NvencEncoder {
    type Frame = Frame;

    fn encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<()> {
        if self.try_encode(frame, pts, keyframe)? {
            return Ok(());
        }
        if !self.low_latency {
            let deadline = Instant::now() + STALL_WAIT;
            let mut state = self.session.state.lock();
            while state.pending.len() == IN_FLIGHT
                && state.pending.len() + state.ready.len() < READY_PACKETS
            {
                state.healthy()?;
                if self
                    .session
                    .changed
                    .wait_until(&mut state, deadline)
                    .timed_out()
                {
                    break;
                }
            }
            drop(state);
            if self.try_encode(frame, pts, keyframe)? {
                return Ok(());
            }
        }
        self.session.state.lock().stats.dropped += 1;
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        self.receive_timeout(Duration::ZERO)
    }

    fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<EncodedVideo>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("NVENC receive timeout too large")?;
        let mut state = self.session.state.lock();
        loop {
            if let Some(packet) = state.take_ready() {
                return Ok(Some(packet));
            }
            state.healthy()?;
            if state.pending.is_empty() || Instant::now() >= deadline {
                return Ok(None);
            }
            self.session.changed.wait_until(&mut state, deadline);
        }
    }

    fn dropped(&self) -> u64 {
        self.session.state.lock().stats.dropped
    }

    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) -> Result<()> {
        if self.recording {
            bail!("encoder.nvenc: recording settings cannot change; open a new encoder");
        }
        self.session.state.lock().healthy()?;
        if self.finishing {
            bail!("NVENC encoder has already begun finishing");
        }
        let bits = bitrate(fps, bitrate_kbps)?;
        let native = self.session.native.lock();
        let status = unsafe { isocast_nvenc_set_rate(native.raw.as_ptr(), fps, bits) };
        let result = native.check(status);
        drop(native);
        self.worker.wake.signal();
        if let Err(error) = &result {
            self.session.fail(&self.worker, format!("{error:#}"));
        }
        result
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("NVENC teardown: {error:#}");
        }
        // A timed-out finish must not add another five seconds in Drop.
        let deadline = if self.finished {
            Instant::now() + FINISH_WAIT
        } else {
            self.finish_deadline.unwrap_or_else(Instant::now)
        };
        if !self.worker.detach(&self.session, deadline) {
            return;
        }
        if !self.session.quarantined.load(Ordering::Acquire) {
            let native = self.session.native.lock();
            let status = unsafe { isocast_nvenc_close(native.raw.as_ptr()) };
            if let Err(error) = native.check(status) {
                eprintln!("NVENC teardown: {error:#}; driver resources retained");
                self.session.fail(&self.worker, format!("{error:#}"));
            }
        }
        self.worker.stop_if_idle();
    }
}

fn validate_fps(fps: u32) -> Result<()> {
    if !(1..=240).contains(&fps) {
        bail!("video.fps: valid range is 1..=240");
    }
    Ok(())
}

fn bitrate(fps: u32, kbps: u32) -> Result<u32> {
    validate_fps(fps)?;
    kbps.checked_mul(1000)
        .filter(|value| *value > 0)
        .context("video.bitrate_kbps: must be positive and fit in 32-bit bits/second")
}

fn wait_millis(duration: Duration) -> u32 {
    // Round UP: truncating a future sub-millisecond deadline makes a hot loop.
    duration
        .as_nanos()
        .div_ceil(1_000_000)
        .min(u128::from(u32::MAX - 1)) as u32
}
