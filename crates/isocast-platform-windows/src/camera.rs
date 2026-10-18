//! Native Media Foundation camera samples, copied GPU-to-GPU into owned NV12
//! frames. No DirectShow, CPU-buffer fallback, or timestamp rebasing.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{CameraRequest, CaptureFormat, Time, VideoSink};
use isocast_protocol::Canvas;
use parking_lot::Mutex;
use windows::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device5, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::WaitForMultipleObjects;
use windows::core::{GUID, HRESULT, Interface, Ref, implement};

use crate::clock;
use crate::frames::{Frame, GraphicsDevice, TexturePool};
use crate::mf::Runtime;
use crate::native::{Apartment, Event};

const MAX_DEVICES: u32 = 256;
const MAX_FORMATS: u32 = 512;
const IN_FLIGHT: usize = 8;
const START_WAIT: Duration = Duration::from_secs(5);
const COPY_WAIT: Duration = Duration::from_secs(5);
// A failed copy signal or a hung GPU must not let retries accumulate retained
// camera sources forever. Existing captures also stop requesting new samples.
static QUARANTINED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraFormat {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Clone, Debug)]
pub struct CameraDevice {
    /// Exact MF video-capture symbolic link, not the friendly name.
    pub id: String,
    pub name: String,
    /// Qualified native NV12, progressive, limited-range BT.709 integer modes.
    /// Metadata alone does not guarantee that the driver delivers GPU samples.
    pub formats: Vec<CameraFormat>,
}

type Startup = std::result::Result<(CameraDevice, CaptureFormat), String>;

#[derive(Clone, Copy, Default, Debug)]
pub struct CameraCaptureStats {
    /// Samples received by the native callback, including rejected samples.
    pub received_frames: u64,
    /// Frame callbacks that returned normally.
    pub delivered_frames: u64,
    pub pool_drops: u64,
    pub discontinuities: u64,
    pub timestamp_errors: u64,
    pub format_changes: u64,
    /// Timestamp intervals over 1.5 frame periods, NOT an exact loss count.
    pub gap_events: u64,
    pub stream_ticks: u64,
}

#[derive(Default)]
struct Counters {
    received_frames: AtomicU64,
    delivered_frames: AtomicU64,
    pool_drops: AtomicU64,
    discontinuities: AtomicU64,
    timestamp_errors: AtomicU64,
    format_changes: AtomicU64,
    gap_events: AtomicU64,
    stream_ticks: AtomicU64,
}

struct State {
    stop: Event,
    wake: Event,
    stopping: AtomicBool,
    mailbox: Mutex<Mailbox>,
    counts: Counters,
}

#[derive(Default)]
struct Mailbox {
    messages: VecDeque<Message>,
    error: Option<String>,
    closed: bool,
}

enum Message {
    Sample {
        status: HRESULT,
        stream: u32,
        flags: u32,
        sample: Option<IMFSample>,
        arrived: Time,
    },
    Event(IMFMediaEvent),
}

// SAFETY: MF callbacks and the capture worker are in the process MTA pinned
// by Apartment. These COM references move only inside that apartment, under
// the mailbox mutex; none is exposed to the caller or a user callback.
unsafe impl Send for Message {}

impl State {
    fn post(&self, message: Message) {
        let mut mailbox = self.mailbox.lock();
        if mailbox.closed || self.stopping.load(Ordering::Acquire) {
            return;
        }
        if mailbox.messages.len() == IN_FLIGHT {
            mailbox
                .error
                .get_or_insert_with(|| "MF camera callback mailbox overflow".into());
        } else {
            mailbox.messages.push_back(message);
        }
        self.wake.signal();
    }

    fn next(&self) -> Result<Option<Message>> {
        let mut mailbox = self.mailbox.lock();
        if let Some(error) = mailbox.error.take() {
            bail!("{error}");
        }
        Ok(mailbox.messages.pop_front())
    }

    fn close(&self) {
        let messages = {
            let mut mailbox = self.mailbox.lock();
            mailbox.closed = true;
            std::mem::take(&mut mailbox.messages)
        };
        // Release native samples/events on the worker without the mailbox
        // lock held: a final COM release may itself run native callbacks.
        drop(messages);
    }

    fn wait(&self, milliseconds: u32) -> Result<()> {
        let result = unsafe {
            WaitForMultipleObjects(
                &[self.stop.handle(), self.wake.handle()],
                false,
                milliseconds,
            )
        };
        if result == WAIT_FAILED {
            return Err(windows::core::Error::from_thread()).context("MF camera wait");
        }
        if result != WAIT_OBJECT_0 && result.0 != WAIT_OBJECT_0.0 + 1 && result != WAIT_TIMEOUT {
            bail!("unexpected MF camera wait result: {}", result.0);
        }
        Ok(())
    }
}

#[implement(IMFSourceReaderCallback)]
struct Callback {
    state: Arc<State>,
}

impl IMFSourceReaderCallback_Impl for Callback_Impl {
    fn OnReadSample(
        &self,
        status: HRESULT,
        stream: u32,
        flags: u32,
        _: i64,
        sample: Ref<IMFSample>,
    ) -> windows::core::Result<()> {
        if sample.as_ref().is_some() {
            self.state
                .counts
                .received_frames
                .fetch_add(1, Ordering::Relaxed);
        }
        self.state.post(Message::Sample {
            status,
            stream,
            flags,
            sample: sample.as_ref().cloned(),
            arrived: clock::now(),
        });
        Ok(())
    }

    fn OnEvent(&self, _: u32, event: Ref<IMFMediaEvent>) -> windows::core::Result<()> {
        if let Some(event) = event.as_ref() {
            self.state.post(Message::Event(event.clone()));
        } else {
            let mut mailbox = self.state.mailbox.lock();
            mailbox
                .error
                .get_or_insert_with(|| "MF camera returned a null event".into());
            self.state.wake.signal();
        }
        Ok(())
    }

    fn OnFlush(&self, _: u32) -> windows::core::Result<()> {
        let mut mailbox = self.state.mailbox.lock();
        if !mailbox.closed && !self.state.stopping.load(Ordering::Acquire) {
            mailbox
                .error
                .get_or_insert_with(|| "MF camera flushed unexpectedly".into());
            self.state.wake.signal();
        }
        Ok(())
    }
}

/// Drop signals and joins the worker, including native shutdown. Native calls
/// and user callbacks can block; there is no hard wall-clock stop guarantee.
/// Self-drop from a sink callback requests stop; cleanup is deferred until
/// that callback returns, rather than attempting to join its own thread.
pub struct CameraCapture {
    state: Arc<State>,
    worker: Option<JoinHandle<()>>,
    device: CameraDevice,
}

impl CameraCapture {
    pub fn device(&self) -> &CameraDevice {
        &self.device
    }

    pub fn stats(&self) -> CameraCaptureStats {
        let counts = &self.state.counts;
        CameraCaptureStats {
            received_frames: counts.received_frames.load(Ordering::Relaxed),
            delivered_frames: counts.delivered_frames.load(Ordering::Relaxed),
            pool_drops: counts.pool_drops.load(Ordering::Relaxed),
            discontinuities: counts.discontinuities.load(Ordering::Relaxed),
            timestamp_errors: counts.timestamp_errors.load(Ordering::Relaxed),
            format_changes: counts.format_changes.load(Ordering::Relaxed),
            gap_events: counts.gap_events.load(Ordering::Relaxed),
            stream_ticks: counts.stream_ticks.load(Ordering::Relaxed),
        }
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::Release);
        self.state.stop.signal();
        if let Some(worker) = self.worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

/// Enumerate identities and native modes on a private, joined COM thread.
/// Bad devices keep their identity with empty formats and a diagnostic; they
/// cannot prevent listing another camera. No ReadSample is issued here.
pub fn camera_devices() -> Result<Vec<CameraDevice>> {
    thread::Builder::new()
        .name("Windows camera devices".into())
        .spawn(|| {
            let result = (|| -> Result<_> {
                let _apartment = Apartment::new()?;
                let _runtime = Runtime::new()?;
                let mut devices = Vec::new();
                for identity in identities()? {
                    let mut device = identity.device.clone();
                    let formats = (|| -> Result<_> {
                        let source = Source::open(identity.activation)?;
                        let reader =
                            unsafe { MFCreateSourceReaderFromMediaSource(&source.raw, None) }?;
                        Ok(
                            native_formats(&reader, MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)?
                                .into_iter()
                                .map(|mode| mode.format)
                                .collect(),
                        )
                    })();
                    match formats {
                        Ok(formats) => device.formats = formats,
                        Err(error) => eprintln!(
                            "Windows camera devices: {:?} ({}): {error:#}",
                            device.name, device.id
                        ),
                    }
                    devices.push(device);
                }
                Ok(devices)
            })();
            result.map_err(|error| format!("Windows camera devices: {error:#}"))
        })?
        .join()
        .map_err(|_| anyhow!("Windows camera enumeration panicked"))?
        .map_err(anyhow::Error::msg)
}

pub fn capture_camera(
    graphics: &GraphicsDevice,
    device: &str,
    request: CameraRequest,
    canvas: &Canvas,
    sink: VideoSink<Frame>,
    thread_start: fn(),
) -> Result<(CameraCapture, CaptureFormat)> {
    if request.fps.is_some_and(|fps| !(1..=240).contains(&fps)) {
        bail!("camera.fps: valid integer range is 1..=240");
    }
    if request.size.is_some_and(|(width, height)| {
        width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2)
    }) {
        bail!("camera.size: NV12 requires positive even width and height");
    }
    check_quarantine()?;
    let state = Arc::new(State {
        stop: Event::new()?,
        wake: Event::new()?,
        stopping: AtomicBool::new(false),
        mailbox: Mutex::new(Mailbox::default()),
        counts: Counters::default(),
    });
    let (started, ready) = mpsc::sync_channel(1);
    let query = device.to_owned();
    let canvas = canvas.clone();
    let worker = {
        let graphics = graphics.clone();
        let state = state.clone();
        thread::Builder::new()
            .name("Windows camera capture".into())
            .spawn(move || {
                let mut running = false;
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let _apartment = Apartment::new()?;
                        let _runtime = Runtime::new()?;
                        thread_start();
                        let identity = resolve(&query)?;
                        let mut capture =
                            Session::new(graphics, identity, request, &canvas, state.clone())?;
                        capture.run(&sink, &started, &mut running)
                    }));
                // Also covers failures after callback registration but before
                // Session construction. No queued COM reference may be left
                // for the caller's apartment to release.
                state.close();
                let error = match result {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => format!("Windows camera capture: {error:#}"),
                    Err(_) => "Windows camera capture worker panicked".into(),
                };
                if !running {
                    let _ = started.send(Err(error));
                } else if !state.stopping.load(Ordering::Acquire) {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        (sink.stopped)(error)
                    }));
                }
            })?
    };
    match ready.recv_timeout(Duration::from_secs(8)) {
        Ok(Ok((device, format))) => Ok((
            CameraCapture {
                state,
                worker: Some(worker),
                device,
            },
            format,
        )),
        result => {
            state.stopping.store(true, Ordering::Release);
            state.stop.signal();
            // Native activation/shutdown cannot be forcibly interrupted. Join
            // even on timeout; never detach an OS-open worker on each retry.
            let _ = worker.join();
            match result {
                Ok(Err(error)) => bail!("{error}"),
                Err(error) => Err(error).context("Windows camera did not start in time"),
                _ => unreachable!(),
            }
        }
    }
}

struct Activation(IMFActivate);

impl Drop for Activation {
    fn drop(&mut self) {
        shutdown_result(unsafe { self.0.ShutdownObject() }, "activation shutdown");
    }
}

struct Source {
    raw: IMFMediaSource,
    _activation: Activation,
}

impl Source {
    fn open(activation: IMFActivate) -> Result<Self> {
        let activation = Activation(activation);
        let raw = unsafe { activation.0.ActivateObject() }.context("MF camera activation")?;
        Ok(Self {
            raw,
            _activation: activation,
        })
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        shutdown_result(unsafe { self.raw.Shutdown() }, "source shutdown");
    }
}

fn shutdown_result(result: windows::core::Result<()>, operation: &str) {
    if let Err(error) = result
        && error.code() != MF_E_SHUTDOWN
    {
        eprintln!("Windows camera {operation}: {error}");
    }
}

struct DeviceArray {
    items: *mut Option<IMFActivate>,
    count: u32,
}

impl Drop for DeviceArray {
    fn drop(&mut self) {
        if !self.items.is_null() {
            // Even a rejected oversized list owns all returned COM references.
            for index in 0..self.count as usize {
                drop(unsafe { (*self.items.add(index)).take() });
            }
            unsafe { CoTaskMemFree(Some(self.items.cast())) };
        }
    }
}

struct Identity {
    activation: IMFActivate,
    device: CameraDevice,
}

fn attributes() -> Result<IMFAttributes> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, 4) }?;
    attributes.context("MF returned no attributes")
}

fn attribute_string(attributes: &IMFAttributes, key: &GUID) -> Result<String> {
    let length = unsafe { attributes.GetStringLength(key) }?;
    if length == 0 || length > 32768 {
        bail!("MF camera identity length {length} is invalid");
    }
    let mut value = vec![0; length as usize + 1];
    let mut actual = 0;
    unsafe { attributes.GetString(key, &mut value, Some(&mut actual)) }?;
    if actual > length || value[..actual as usize].contains(&0) {
        bail!("MF camera identity changed or contains NUL");
    }
    Ok(String::from_utf16(&value[..actual as usize])?)
}

fn identities() -> Result<Vec<Identity>> {
    let attributes = attributes()?;
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
    }?;
    let mut list = DeviceArray {
        items: std::ptr::null_mut(),
        count: 0,
    };
    unsafe { MFEnumDeviceSources(&attributes, &mut list.items, &mut list.count) }?;
    if list.count > MAX_DEVICES || (list.count != 0 && list.items.is_null()) {
        bail!(
            "MF camera enumeration returned an invalid list ({} devices; limit {MAX_DEVICES})",
            list.count
        );
    }
    let mut devices = Vec::new();
    for index in 0..list.count as usize {
        let identity = (|| -> Result<_> {
            let activation =
                unsafe { (*list.items.add(index)).take() }.context("null camera activation")?;
            let id = attribute_string(
                &activation,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            )?;
            let name = attribute_string(&activation, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)?;
            Ok(Identity {
                activation,
                device: CameraDevice {
                    id,
                    name,
                    formats: Vec::new(),
                },
            })
        })();
        match identity {
            Ok(identity) => devices.push(identity),
            Err(error) => eprintln!("Windows camera devices: skipping identity {index}: {error:#}"),
        }
    }
    Ok(devices)
}

fn resolve(query: &str) -> Result<Identity> {
    let mut devices = identities()?;
    if let Some(index) = devices
        .iter()
        .position(|identity| identity.device.id == query)
    {
        return Ok(devices.remove(index));
    }
    let lower = query.to_lowercase();
    let exact = devices
        .iter()
        .any(|identity| identity.device.name.to_lowercase() == lower);
    let matches: Vec<_> = devices
        .iter()
        .enumerate()
        .filter(|(_, identity)| {
            !query.is_empty()
                && !query.contains('\0')
                && !query.eq_ignore_ascii_case("default")
                && if exact {
                    identity.device.name.to_lowercase() == lower
                } else {
                    identity.device.name.to_lowercase().contains(&lower)
                }
        })
        .map(|(index, _)| index)
        .collect();
    if matches.len() == 1 {
        return Ok(devices.remove(matches[0]));
    }
    let choices = devices
        .iter()
        .map(|identity| format!("{} ({})", identity.device.name, identity.device.id))
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "camera.device: {query:?} matched {} cameras; use an exact symbolic link, exact name or unambiguous name substring; valid choices: {}",
        matches.len(),
        if choices.is_empty() {
            "(none)"
        } else {
            &choices
        }
    );
}

struct NativeFormat {
    raw: IMFMediaType,
    format: CameraFormat,
}

fn optional_u32(attributes: &IMFAttributes, key: &GUID) -> Result<Option<u32>> {
    match unsafe { attributes.GetUINT32(key) } {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn qualified_format(media_type: &IMFMediaType) -> Result<CameraFormat> {
    unsafe {
        if media_type.GetGUID(&MF_MT_MAJOR_TYPE)? != MFMediaType_Video
            || media_type.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_NV12
        {
            bail!("camera.format: only native NV12 video is qualified; no decoder or CPU fallback");
        }
        let size = media_type.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let width = (size >> 32) as u32;
        let height = size as u32;
        let rate = media_type.GetUINT64(&MF_MT_FRAME_RATE)?;
        let numerator = (rate >> 32) as u32;
        let denominator = rate as u32;
        if width == 0
            || height == 0
            || width > 16384
            || height > 16384
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
        {
            bail!("camera.size: unsupported native NV12 size {width}x{height}");
        }
        if denominator == 0
            || !numerator.is_multiple_of(denominator)
            || !(1..=240).contains(&(numerator / denominator))
        {
            bail!(
                "camera.fps: native rate {numerator}/{denominator}; only exact integers 1..=240 are qualified"
            );
        }
        for (key, expected, label) in [
            (
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0 as u32,
                "progressive interlace mode",
            ),
            (
                &MF_MT_YUV_MATRIX,
                MFVideoTransferMatrix_BT709.0 as u32,
                "BT.709 matrix",
            ),
            (
                &MF_MT_VIDEO_NOMINAL_RANGE,
                MFNominalRange_16_235.0 as u32,
                "16..235 nominal range",
            ),
            (
                &MF_MT_VIDEO_PRIMARIES,
                MFVideoPrimaries_BT709.0 as u32,
                "BT.709 primaries",
            ),
        ] {
            let value = optional_u32(media_type, key)?;
            if value != Some(expected) {
                bail!("camera.format: requires {label} ({expected}), got {value:?}");
            }
        }
        // The qualified 4K S driver omits transfer-function metadata. Do not
        // invent a missing attribute, but reject explicit HDR/non-709 values.
        if let Some(transfer) = optional_u32(media_type, &MF_MT_TRANSFER_FUNCTION)?
            && transfer != MFVideoTransFunc_709.0 as u32
        {
            bail!(
                "camera.format: transfer function {transfer}; only BT.709 or unspecified is qualified"
            );
        }
        Ok(CameraFormat {
            width,
            height,
            fps: numerator / denominator,
        })
    }
}

fn native_formats(reader: &IMFSourceReader, stream: u32) -> Result<Vec<NativeFormat>> {
    let mut formats = Vec::new();
    let mut rejected = 0;
    let mut last_rejection = String::new();
    for index in 0..MAX_FORMATS {
        let raw = match unsafe { reader.GetNativeMediaType(stream, index) } {
            Ok(raw) => raw,
            Err(error) if error.code() == MF_E_NO_MORE_TYPES => {
                if formats.is_empty() {
                    bail!(
                        "camera.format: no qualified native modes ({rejected} rejected): {last_rejection}"
                    );
                }
                return Ok(formats);
            }
            Err(error) => return Err(error).context("enumerate native MF camera modes"),
        };
        match qualified_format(&raw) {
            Ok(format)
                if !formats
                    .iter()
                    .any(|mode: &NativeFormat| mode.format == format) =>
            {
                formats.push(NativeFormat { raw, format });
            }
            Ok(_) => {}
            Err(error) => {
                rejected += 1;
                last_rejection = format!("{error:#}");
            }
        }
    }
    bail!("camera.format: native enumeration did not end within {MAX_FORMATS} entries")
}

fn select_format(
    formats: &[NativeFormat],
    request: CameraRequest,
    canvas: &Canvas,
) -> Result<usize> {
    let wanted = request.size.unwrap_or((canvas.width, canvas.height));
    let fps = request.fps.unwrap_or(canvas.fps);
    // Exact requested fields are constraints. Unspecified fields prefer the
    // nearest canvas dimensions, then rate; stable native order breaks ties.
    let selected = formats
        .iter()
        .enumerate()
        .filter(|(_, mode)| {
            request
                .size
                .is_none_or(|size| size == (mode.format.width, mode.format.height))
                && request.fps.is_none_or(|fps| fps == mode.format.fps)
        })
        .min_by_key(|(_, mode)| {
            (
                u64::from(mode.format.width.abs_diff(wanted.0))
                    + u64::from(mode.format.height.abs_diff(wanted.1)),
                mode.format.fps.abs_diff(fps),
            )
        });
    if let Some((index, _)) = selected {
        return Ok(index);
    }
    let choices = formats
        .iter()
        .map(|mode| {
            format!(
                "{}x{}@{}",
                mode.format.width, mode.format.height, mode.format.fps
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "camera.size/fps: request {request:?} has no exact native match; qualified choices: {choices}"
    )
}

fn video_stream(source: &IMFMediaSource) -> Result<u32> {
    let presentation = unsafe { source.CreatePresentationDescriptor() }?;
    let count = unsafe { presentation.GetStreamDescriptorCount() }?;
    if count > MAX_DEVICES {
        bail!("MF camera exposes too many streams: {count}");
    }
    for index in 0..count {
        let mut selected = Default::default();
        let mut descriptor = None;
        unsafe { presentation.GetStreamDescriptorByIndex(index, &mut selected, &mut descriptor) }?;
        let descriptor = descriptor.context("MF camera returned no stream descriptor")?;
        if unsafe { descriptor.GetMediaTypeHandler()?.GetMajorType() }? == MFMediaType_Video {
            return Ok(index);
        }
    }
    bail!("MF camera exposes no video stream")
}

struct Pending {
    frame: Frame,
    _sample: IMFSample,
    submitted: Instant,
}

struct Resources {
    // Reader releases before source shutdown on a normal drop. The manager,
    // source and texture pool remain together if a GPU copy is quarantined.
    reader: IMFSourceReader,
    _source: Source,
    _manager: IMFDXGIDeviceManager,
    textures: TexturePool,
    graphics: GraphicsDevice,
    pending: VecDeque<Pending>,
    uncertain: Option<IMFSample>,
}

impl Resources {
    fn reap(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let completed = self.graphics.completed()?;
        while self
            .pending
            .front()
            .is_some_and(|pending| pending.frame.ready_value() <= completed)
        {
            self.pending.pop_front();
        }
        Ok(())
    }
}

struct Session {
    resources: Option<Resources>,
    state: Arc<State>,
    device: CameraDevice,
    format: CameraFormat,
    stream: u32,
    previous: Option<Time>,
}

impl Session {
    fn new(
        graphics: GraphicsDevice,
        identity: Identity,
        request: CameraRequest,
        canvas: &Canvas,
        state: Arc<State>,
    ) -> Result<Self> {
        check_quarantine()?;
        // Resolution has finished before this sole device is activated.
        let source = Source::open(identity.activation).with_context(|| {
            format!(
                "camera.device: {:?} ({})",
                identity.device.name, identity.device.id
            )
        })?;
        let stream = video_stream(&source.raw)?;
        let mut token = 0;
        let mut manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }?;
        let manager = manager.context("MF returned no camera DXGI manager")?;
        unsafe { manager.ResetDevice(graphics.raw(), token) }?;
        let callback: IMFSourceReaderCallback = Callback {
            state: state.clone(),
        }
        .into();
        let attributes = attributes()?;
        unsafe {
            attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)?;
            attributes.SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback)?;
            attributes.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)?;
            attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 0)?;
        }
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source.raw, &attributes) }
            .context("create GPU MF camera Source Reader")?;
        unsafe {
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(stream, true)?;
        }
        let formats = native_formats(&reader, stream)?;
        let selected = &formats[select_format(&formats, request, canvas)?];
        unsafe { reader.SetCurrentMediaType(stream, None, &selected.raw) }
            .context("set exact native MF camera mode")?;
        let current = qualified_format(&unsafe { reader.GetCurrentMediaType(stream) }?)?;
        if current != selected.format {
            bail!(
                "camera.format: requested {:?}, driver selected {current:?}",
                selected.format
            );
        }
        let mut device = identity.device;
        device.formats = formats.into_iter().map(|mode| mode.format).collect();
        Ok(Self {
            resources: Some(Resources {
                reader,
                _source: source,
                _manager: manager,
                textures: TexturePool::new(graphics.clone()),
                graphics,
                pending: VecDeque::with_capacity(IN_FLIGHT),
                uncertain: None,
            }),
            state,
            device,
            format: current,
            stream,
            previous: None,
        })
    }

    fn run(
        &mut self,
        sink: &VideoSink<Frame>,
        started: &mpsc::SyncSender<Startup>,
        running: &mut bool,
    ) -> Result<()> {
        let deadline = Instant::now() + START_WAIT;
        let mut outstanding = false;
        loop {
            if self.state.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            check_quarantine()?;
            if !*running && Instant::now() >= deadline {
                bail!("MF camera delivered no qualified GPU frame within 5 seconds");
            }
            if let Some(message) = self.state.next()? {
                match message {
                    Message::Event(event) => {
                        unsafe { event.GetStatus()?.ok() }.context("MF camera source event")?;
                        if unsafe { event.GetType() }? == MEError.0 as u32 {
                            bail!("MF camera source reported MEError");
                        }
                    }
                    Message::Sample {
                        status,
                        stream,
                        flags,
                        sample,
                        arrived,
                    } => {
                        status.ok().context("MF camera ReadSample callback")?;
                        if !outstanding || stream != self.stream {
                            bail!(
                                "MF camera returned an unsolicited sample or wrong stream {stream}"
                            );
                        }
                        outstanding = false;
                        self.check_flags(flags, *running)?;
                        if let Some(sample) = sample {
                            let at = self.timestamp(&sample, arrived).inspect_err(|_| {
                                self.state
                                    .counts
                                    .timestamp_errors
                                    .fetch_add(1, Ordering::Relaxed);
                            })?;
                            if let Some(frame) = self.copy(sample)? {
                                if !*running {
                                    let format = CaptureFormat {
                                        width: self.format.width,
                                        height: self.format.height,
                                        fps: self.format.fps,
                                    };
                                    if started.send(Ok((self.device.clone(), format))).is_err() {
                                        return Ok(());
                                    }
                                    *running = true;
                                }
                                if self.state.stopping.load(Ordering::Acquire) {
                                    return Ok(());
                                }
                                (sink.frame)(frame, at);
                                self.state
                                    .counts
                                    .delivered_frames
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                continue;
            }
            let resources = self.resources.as_mut().expect("open camera");
            resources.reap()?;
            if resources
                .pending
                .front()
                .is_some_and(|pending| pending.submitted.elapsed() >= COPY_WAIT)
            {
                bail!("MF camera GPU copy did not complete within 5 seconds");
            }
            if !outstanding && resources.pending.len() < IN_FLIGHT {
                // One request at a time; no ReadSample is issued by callbacks.
                outstanding = true;
                unsafe {
                    resources
                        .reader
                        .ReadSample(self.stream, 0, None, None, None, None)
                }
                .context("MF camera ReadSample")?;
            }
            // A full copy ring is the only steady-state CPU pressure wait.
            // Otherwise the callback event wakes the worker immediately.
            self.state.wait(if resources.pending.len() == IN_FLIGHT {
                10
            } else {
                100
            })?;
        }
    }

    fn check_flags(&self, flags: u32, running: bool) -> Result<()> {
        if flags & MF_SOURCE_READERF_STREAMTICK.0 as u32 != 0 {
            self.state
                .counts
                .stream_ticks
                .fetch_add(1, Ordering::Relaxed);
        }
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            bail!("MF camera Source Reader error flag");
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            bail!("MF camera source ended");
        }
        let changes = MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32
            | MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0 as u32
            | MF_SOURCE_READERF_NEWSTREAM.0 as u32
            | MF_SOURCE_READERF_ALLEFFECTSREMOVED.0 as u32;
        if flags & changes != 0 {
            self.state
                .counts
                .format_changes
                .fetch_add(1, Ordering::Relaxed);
            let reader = &self.resources.as_ref().expect("open camera").reader;
            let current = qualified_format(&unsafe { reader.GetCurrentMediaType(self.stream) }?)?;
            if running
                || current != self.format
                || flags
                    & (MF_SOURCE_READERF_NEWSTREAM.0 as u32
                        | MF_SOURCE_READERF_ALLEFFECTSREMOVED.0 as u32)
                    != 0
            {
                bail!(
                    "camera.format changed (flags {flags:#x}, current {current:?}); reopen explicitly"
                );
            }
        }
        Ok(())
    }

    fn timestamp(&mut self, sample: &IMFSample, arrived: Time) -> Result<Time> {
        if optional_u32(sample, &MFSampleExtension_Discontinuity)?.unwrap_or(0) != 0 {
            self.state
                .counts
                .discontinuities
                .fetch_add(1, Ordering::Relaxed);
        }
        // DeviceTimestamp is absolute QPC in 100 ns units. SampleTime may be
        // stream-relative; arrival is only a future-timestamp sanity check.
        let ticks = unsafe { sample.GetUINT64(&MFSampleExtension_DeviceTimestamp) }
            .context("camera.timestamp: missing QPC device timestamp; no arrival-time fallback")?;
        let nanos = ticks
            .checked_mul(100)
            .and_then(|nanos| i64::try_from(nanos).ok())
            .context("camera.timestamp: QPC100ns overflow")?;
        let at = Time::from_nanos(nanos);
        if ticks == 0
            || self.previous.is_some_and(|previous| at <= previous)
            || nanos > arrived.as_nanos().saturating_add(100_000_000)
        {
            bail!(
                "camera.timestamp: zero, non-advancing or future QPC device timestamp {ticks} (arrival {}ns)",
                arrived.as_nanos()
            );
        }
        if self.previous.is_some_and(|previous| {
            nanos - previous.as_nanos() > 1_500_000_000 / i64::from(self.format.fps)
        }) {
            self.state.counts.gap_events.fetch_add(1, Ordering::Relaxed);
        }
        self.previous = Some(at);
        Ok(at)
    }

    fn copy(&mut self, sample: IMFSample) -> Result<Option<Frame>> {
        if optional_u32(&sample, &MFSampleExtension_Interlaced)?.unwrap_or(0) != 0 {
            bail!("camera.format: interlaced sample in a progressive stream");
        }
        let buffers = unsafe { sample.GetBufferCount() }?;
        if buffers != 1 {
            bail!("camera.frame: requires exactly one DXGI buffer, got {buffers}");
        }
        let buffer = unsafe { sample.GetBufferByIndex(0) }?;
        let dxgi: IMFDXGIBuffer = buffer
            .cast()
            .context("camera.frame: CPU/non-DXGI buffers are not supported")?;
        let mut raw = std::ptr::null_mut();
        unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }?;
        if raw.is_null() {
            bail!("camera.frame: null D3D11 texture");
        }
        // GetResource transferred this owned COM reference to the caller.
        let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
        let subresource = unsafe { dxgi.GetSubresourceIndex() }?;
        let owner: ID3D11Device5 = unsafe { texture.GetDevice() }?.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let resources = self.resources.as_mut().expect("open camera");
        if owner != *resources.graphics.raw()
            || desc.Format != DXGI_FORMAT_NV12
            || desc.Usage != D3D11_USAGE_DEFAULT
            || desc.CPUAccessFlags != 0
            || desc.MipLevels != 1
            || desc.ArraySize == 0
            || subresource >= desc.ArraySize
            || desc.SampleDesc.Count != 1
            || desc.SampleDesc.Quality != 0
            || desc.Width < self.format.width
            || desc.Height < self.format.height
            || !desc.Width.is_multiple_of(2)
            || !desc.Height.is_multiple_of(2)
        {
            bail!(
                "camera.frame: requires same-device, GPU-only default-usage NV12 with a valid array subresource and dimensions; got {desc:?}, subresource {subresource}"
            );
        }
        // A failing pool call may already have queued CopySubresourceRegion
        // without a successful fence signal. Keep its source even on Err.
        resources.uncertain = Some(sample);
        let frame = resources.textures.copy_subresource(
            &texture,
            subresource,
            self.format.width,
            self.format.height,
        )?;
        let sample = resources.uncertain.take().expect("copy source retained");
        if let Some(frame) = &frame {
            resources.pending.push_back(Pending {
                frame: frame.clone(),
                _sample: sample,
                submitted: Instant::now(),
            });
        } else {
            self.state.counts.pool_drops.fetch_add(1, Ordering::Relaxed);
        }
        Ok(frame)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.state.close();
        let Some(mut resources) = self.resources.take() else {
            return;
        };
        shutdown_result(
            unsafe {
                resources
                    .reader
                    .Flush(MF_SOURCE_READER_ALL_STREAMS.0 as u32)
            },
            "reader flush",
        );
        let deadline = Instant::now() + COPY_WAIT;
        let drained = (|| -> Result<()> {
            if resources.uncertain.is_some() {
                bail!("copy submission failed without a trustworthy completion fence");
            }
            loop {
                resources.reap()?;
                if resources.pending.is_empty() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    bail!("GPU copies did not finish within 5 seconds");
                }
                // Teardown only: no event registration can outlive this wait.
                thread::sleep(Duration::from_millis(1));
            }
        })();
        if let Err(error) = drained {
            QUARANTINED.store(true, Ordering::Release);
            // Flush cancels the reader request; Stop additionally stops the
            // producer. Do not destroy it or its outstanding sample storage
            // while a queued GPU copy might still be reading that storage.
            shutdown_result(
                unsafe { resources._source.raw.Stop() },
                "quarantined source stop",
            );
            eprintln!(
                "Windows camera teardown: {error:#}; retaining source, samples, frames and device until process exit; camera capture disabled"
            );
            // No detached cleanup thread and no use-after-recycle. This is a
            // fatal GPU quarantine, not a reusable stop/reopen path.
            std::mem::forget(resources);
        }
    }
}

fn check_quarantine() -> Result<()> {
    if QUARANTINED.load(Ordering::Acquire) {
        bail!(
            "camera GPU resources were quarantined after a failed copy; restart the process before reopening capture"
        );
    }
    Ok(())
}
