//! The virtual camera (L.6e): a v4l2loopback device the engine's program
//! (or any other target) is written to, so browsers, conferencing apps
//! and OBS list it as a webcam.
//!
//! The engine hands this sink one BGRA frame per tick (the same
//! `CameraSink` the macOS CoreMediaIO extension implements). The sink
//! reads the frame back from the GPU if it is still there, converts it to
//! YUYV (the format every webcam consumer takes) and `write(2)`s it into
//! the loopback's output side, which copies it and returns without
//! waiting for a reader.
//!
//! Needs the `v4l2loopback` kernel module, which only root can load.
//! Without it the start is refused by name, with the commands that
//! install and load it. Everything here is raw V4L2 ioctls, like
//! camera.rs; no crate.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::Time;
use isocast_engine::record::CameraSink;

use crate::frames::LinuxFrame;

/// What `status().transport` reads while this sink runs.
const TRANSPORT: &str = "v4l2loopback-yuyv";

/// The card label the install command gives the device; preferred over
/// any other loopback device when several exist.
const PREFERRED_CARD: &str = "Isocast Camera";

/// `ISOCAST_VCAM_DEVICE=/dev/videoN` names the device and skips the
/// search, for tests and for machines with several loopback devices.
const DEVICE_ENV: &str = "ISOCAST_VCAM_DEVICE";

/// The kernel's `v4l2loopback` driver name, as QUERYCAP reports it.
const LOOPBACK_DRIVER: &str = "v4l2 loopback";

const INSTALL_HINT: &str = "install: sudo pacman -S v4l2loopback-dkms; load: sudo modprobe \
     v4l2loopback devices=1 video_nr=10 card_label=\"Isocast Camera\" exclusive_caps=1";

/// The colour space the frames are tagged with: the limited-range BT.601
/// ("SMPTE 170M") matrix every webcam consumer assumes. Changing to 709
/// means changing `COLORSPACE`, `YCBCR_ENC` and the coefficients in
/// `luma`/`chroma` together.
const COLORSPACE: u32 = 1; // V4L2_COLORSPACE_SMPTE170M
const YCBCR_ENC: u32 = 1; // V4L2_YCBCR_ENC_601
const QUANTIZATION: u32 = 2; // V4L2_QUANTIZATION_LIM_RANGE

// ---------------------------------------------------------------------------
// The sink
// ---------------------------------------------------------------------------

/// A v4l2loopback output device, configured for one size and written one
/// YUYV frame at a time. Dropping it closes the descriptor, which is how
/// consumers see the stream stop and how the device releases its format.
/// Unlike macOS, which swaps in a placeholder card, readers keep showing the
/// last frame after a stop (the module's `timeout` option could supply one).
struct V4l2Sink {
    fd: OwnedFd,
    width: u32,
    height: u32,
    /// The converted frame, reused: `width * height * 2` bytes.
    frame: Vec<u8>,
    path: PathBuf,
}

impl V4l2Sink {
    /// A sink over an already-configured descriptor. Nothing here asks the
    /// kernel anything, so tests can use a memfd.
    fn with_fd(fd: OwnedFd, width: u32, height: u32, path: PathBuf) -> Self {
        Self {
            fd,
            width,
            height,
            frame: vec![0; yuyv_len(width, height)],
            path,
        }
    }
}

impl CameraSink<LinuxFrame> for V4l2Sink {
    fn send(&mut self, frame: &LinuxFrame, _pts: Time) -> Result<bool> {
        if frame.size() != (self.width, self.height) {
            bail!(
                "virtual_camera: the frame is {}x{} but {} was opened for {}x{}",
                frame.size().0,
                frame.size().1,
                self.path.display(),
                self.width,
                self.height
            );
        }
        // The compositor drew into the slot's textures; catch the CPU bytes
        // up before reading them, as the writer does.
        crate::gpu::read_back(frame)?;
        let (bytes, stride) = frame.plane(0)?;
        bgra_to_yuyv(
            &bytes,
            stride,
            self.width as usize,
            self.height as usize,
            &mut self.frame,
        );
        match write_whole(self.fd.as_raw_fd(), &self.frame) {
            Ok(sent) => Ok(sent),
            Err(err) => {
                Err(err).with_context(|| format!("virtual_camera: {}", self.path.display()))
            }
        }
    }

    fn transport(&self) -> &'static str {
        TRANSPORT
    }
}

/// One `write(2)` of a whole frame. The loopback copies a frame per
/// write and never waits for a reader; a short write or `EAGAIN` means it
/// could not take this frame, which the engine counts as dropped.
fn write_whole(fd: RawFd, frame: &[u8]) -> std::io::Result<bool> {
    loop {
        // SAFETY: `frame` is a live slice for the whole call and `fd` an
        // open descriptor the caller owns; write(2) only reads the bytes.
        let written = unsafe { libc::write(fd, frame.as_ptr().cast(), frame.len()) };
        if written < 0 {
            let err = std::io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => Ok(false),
                _ => Err(err),
            };
        }
        return Ok(written as usize == frame.len());
    }
}

/// Open the loopback device and configure it for `width` x `height` at
/// `fps`: the sink the engine's virtual camera worker writes to.
pub(crate) fn open(width: u32, height: u32, fps: u32) -> Result<Box<dyn CameraSink<LinuxFrame>>> {
    let candidates = match std::env::var_os(DEVICE_ENV) {
        Some(path) if !path.is_empty() => Candidates {
            usable: vec![PathBuf::from(path)],
            held: Vec::new(),
            unopened: Vec::new(),
        },
        _ => find_devices(),
    };
    if candidates.usable.is_empty() {
        return Err(no_device(&candidates, module_loaded()));
    }
    let mut busy = Vec::new();
    let mut last_error = None;
    for path in &candidates.usable {
        match configure(path, width, height, fps) {
            Ok(fd) => {
                return Ok(Box::new(V4l2Sink::with_fd(fd, width, height, path.clone())));
            }
            Err(Refusal::Busy(message)) => busy.push(message),
            Err(Refusal::Other(err)) => last_error = Some(err),
        }
    }
    match (busy.is_empty(), last_error) {
        (false, _) => bail!("{}", busy.join("; ")),
        (true, Some(err)) => Err(err),
        (true, None) => unreachable!("usable was not empty"),
    }
}

// ---------------------------------------------------------------------------
// Finding the device
// ---------------------------------------------------------------------------

/// What QUERYCAP said about one video node.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Card {
    path: PathBuf,
    driver: String,
    card: String,
    /// The node's own capabilities (`device_caps` when the driver sets it).
    caps: u32,
}

/// The loopback devices on the machine.
#[derive(Debug, Default, PartialEq, Eq)]
struct Candidates {
    /// Loopback devices a writer may open, best first.
    usable: Vec<PathBuf>,
    /// Loopback devices that advertise capture only: with
    /// `exclusive_caps=1` that is how a device that already has a writer
    /// looks.
    held: Vec<PathBuf>,
    /// Nodes that could not be opened, with the OS's reason.
    unopened: Vec<String>,
}

/// Scan `/sys/class/video4linux` and ask each node what it is.
fn find_devices() -> Candidates {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/sys/class/video4linux")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.starts_with("video")
                .then(|| Path::new("/dev").join(name))
        })
        .collect();
    nodes.sort_by_key(|path| {
        let number = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("video"))
            .and_then(|digits| digits.parse::<u32>().ok());
        (number, path.clone())
    });
    let mut cards = Vec::new();
    let mut unopened = Vec::new();
    for path in nodes {
        let fd = match open_node(&path) {
            Ok(fd) => fd,
            Err(err) => {
                unopened.push(format!("{}: {err}", path.display()));
                continue;
            }
        };
        let mut capability = Capability::default();
        if ioctl(fd.as_raw_fd(), VIDIOC_QUERYCAP, &mut capability).is_ok() {
            cards.push(Card {
                path,
                driver: cstr(&capability.driver),
                card: cstr(&capability.card),
                caps: capability.node_caps(),
            });
        }
    }
    let mut found = choose(&cards);
    found.unopened = unopened;
    found
}

/// Pick the loopback devices a writer can use from what the nodes
/// reported. A device is a loopback by its driver's name; its capabilities
/// only say whether it can be written now (OUTPUT) or already has a
/// writer (CAPTURE only, under `exclusive_caps=1`).
fn choose(cards: &[Card]) -> Candidates {
    let mut usable: Vec<&Card> = Vec::new();
    let mut held = Vec::new();
    for card in cards.iter().filter(|card| card.driver == LOOPBACK_DRIVER) {
        if card.caps & CAP_VIDEO_OUTPUT != 0 {
            usable.push(card);
        } else if card.caps & CAP_VIDEO_CAPTURE != 0 {
            held.push(card.path.clone());
        }
    }
    // Stable: the preferred card first, the rest in node order.
    usable.sort_by_key(|card| card.card != PREFERRED_CARD);
    Candidates {
        usable: usable.into_iter().map(|card| card.path.clone()).collect(),
        held,
        unopened: Vec::new(),
    }
}

/// Whether the `v4l2loopback` module is loaded (or built in).
fn module_loaded() -> bool {
    Path::new("/sys/module/v4l2loopback").exists()
}

/// The refusal for a machine with no device a writer can open. It says
/// which of three things is wrong: the module is not loaded, every device
/// already has a writer, or the module is loaded but no node could be
/// opened.
fn no_device(found: &Candidates, module_loaded: bool) -> anyhow::Error {
    if !module_loaded && found.held.is_empty() {
        return anyhow!(
            "virtual_camera: no v4l2loopback device found; the v4l2loopback kernel module is \
             not loaded ({INSTALL_HINT})"
        );
    }
    if !found.held.is_empty() {
        let held: Vec<String> = found
            .held
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        return anyhow!(
            "virtual_camera: every v4l2loopback device already has a writer ({}); stop the \
             program writing to it, or load more devices (modprobe v4l2loopback devices=2)",
            held.join(", ")
        );
    }
    let detail = if found.unopened.is_empty() {
        "the module is loaded but no device has a writable (output) side; load it with \
         exclusive_caps=1 and devices=1 or more"
            .to_string()
    } else {
        format!(
            "the module is loaded but these nodes could not be opened (are you in the video \
             group?): {}",
            found.unopened.join("; ")
        )
    };
    anyhow!("virtual_camera: no usable v4l2loopback device found; {detail}")
}

// ---------------------------------------------------------------------------
// Configuring the device
// ---------------------------------------------------------------------------

/// Why a device would not start.
enum Refusal {
    /// Another program holds it, or a consumer locked a different format.
    Busy(String),
    Other(anyhow::Error),
}

/// Open `path`, set `width` x `height` YUYV at `fps` on its output side
/// and check the kernel kept it.
fn configure(path: &Path, width: u32, height: u32, fps: u32) -> Result<OwnedFd, Refusal> {
    let fd = open_node(path).map_err(|err| {
        Refusal::Other(err.context(format!(
            "virtual_camera: could not open {} (are you in the video group?)",
            path.display()
        )))
    })?;
    let raw = fd.as_raw_fd();
    let mut capability = Capability::default();
    ioctl(raw, VIDIOC_QUERYCAP, &mut capability).map_err(|err| {
        Refusal::Other(anyhow!(
            "virtual_camera: {}: QUERYCAP: {err}",
            path.display()
        ))
    })?;
    if cstr(&capability.driver) != LOOPBACK_DRIVER
        && std::env::var_os(DEVICE_ENV).is_none_or(|value| value.is_empty())
    {
        return Err(Refusal::Other(anyhow!(
            "virtual_camera: {} is a {} device, not a v4l2loopback one",
            path.display(),
            cstr(&capability.driver)
        )));
    }

    let mut format = Format::yuyv(width, height);
    if let Err(err) = ioctl(raw, VIDIOC_S_FMT, &mut format) {
        return Err(match err.raw_os_error() {
            Some(libc::EBUSY) => Refusal::Busy(format!(
                "virtual_camera: {} is busy: another program is writing to it, or a camera \
                 application has it open at a different size (stop it, or load another \
                 device with modprobe v4l2loopback devices=2)",
                path.display()
            )),
            Some(libc::EINVAL) => Refusal::Other(anyhow!(
                "virtual_camera: {} refused {width}x{height} YUYV (VIDIOC_S_FMT: {err}); sizes \
                 above the module's limits need its max_width and max_height parameters",
                path.display()
            )),
            _ => Refusal::Other(anyhow!(
                "virtual_camera: {}: VIDIOC_S_FMT: {err}",
                path.display()
            )),
        });
    }
    // The frame interval is advisory for a loopback; consumers read it
    // back as the nominal rate. A driver without it still carries frames.
    let mut parm = StreamParm::output(fps);
    let _ = ioctl(raw, VIDIOC_S_PARM, &mut parm);

    let mut kept = Format::yuyv(0, 0);
    kept.type_ = BUF_TYPE_VIDEO_OUTPUT;
    ioctl(raw, VIDIOC_G_FMT, &mut kept).map_err(|err| {
        Refusal::Other(anyhow!(
            "virtual_camera: {}: VIDIOC_G_FMT: {err}",
            path.display()
        ))
    })?;
    let pix = kept.pix;
    if (pix.width, pix.height, pix.pixelformat) != (width, height, PIX_FMT_YUYV) {
        return Err(Refusal::Other(anyhow!(
            "virtual_camera: {} changed the format to {}x{} {}: the module's limit is below \
             {width}x{height} (raise max_width and max_height when loading v4l2loopback)",
            path.display(),
            pix.width,
            pix.height,
            fourcc(pix.pixelformat)
        )));
    }
    if pix.bytesperline as usize != yuyv_stride(width)
        || (pix.sizeimage as usize) < yuyv_len(width, height)
    {
        return Err(Refusal::Other(anyhow!(
            "virtual_camera: {} wants {} bytes per row and {} per frame, not the {} and {} of \
             packed YUYV {width}x{height}",
            path.display(),
            pix.bytesperline,
            pix.sizeimage,
            yuyv_stride(width),
            yuyv_len(width, height)
        )));
    }
    Ok(fd)
}

/// Open a video node read-write and non-blocking, closed on exec: the way
/// ffmpeg's v4l2 muxer and OBS open an output device.
fn open_node(path: &Path) -> Result<OwnedFd> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes())
        .with_context(|| format!("{} has a NUL byte in its name", path.display()))?;
    // SAFETY: `c_path` is NUL-terminated and the flags are constants, so
    // the call reads only what it was given and returns a new descriptor
    // or -1.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(anyhow!(
            "could not open {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `fd` is a descriptor this call just made, owned by nothing
    // else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn ioctl<T>(fd: RawFd, request: libc::c_ulong, value: &mut T) -> std::io::Result<()> {
    // SAFETY: the request's argument is a pointer to the struct it
    // names: `value`, live and borrowed for the call.
    if unsafe { libc::ioctl(fd, request as _, value as *mut T) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

/// A pixel format's four letters.
fn fourcc(code: u32) -> String {
    code.to_le_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_graphic() {
                *byte as char
            } else {
                '?'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Colour conversion
// ---------------------------------------------------------------------------

/// Bytes per row of packed YUYV: two pixels per four bytes, an odd last
/// pixel padded.
const fn yuyv_stride(width: u32) -> usize {
    width.div_ceil(2) as usize * 4
}

/// Bytes in one YUYV frame.
const fn yuyv_len(width: u32, height: u32) -> usize {
    yuyv_stride(width) * height as usize
}

/// Limited-range BT.601 luma of an 8-bit R'G'B' pixel (16 to 235).
#[inline(always)]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

/// Limited-range BT.601 Cb (16 to 240).
#[inline(always)]
fn cb(r: i32, g: i32, b: i32) -> u8 {
    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8
}

/// Limited-range BT.601 Cr (16 to 240).
#[inline(always)]
fn cr(r: i32, g: i32, b: i32) -> u8 {
    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8
}

/// Convert BGRA (`stride` bytes per row, alpha ignored) to packed YUYV
/// (Y0 U Y1 V per pixel pair, `yuyv_stride(width)` bytes per row) in
/// limited-range BT.601. Each pair shares the chroma of its averaged
/// colour; an odd last pixel is paired with itself.
fn bgra_to_yuyv(src: &[u8], stride: usize, width: usize, height: usize, dst: &mut [u8]) {
    let out_stride = yuyv_stride(width as u32);
    debug_assert!(dst.len() >= out_stride * height);
    debug_assert!(height == 0 || src.len() >= stride * (height - 1) + width * 4);
    let rows = src.chunks(stride).zip(dst.chunks_exact_mut(out_stride));
    for (row, out) in rows.take(height) {
        convert_row(&row[..width * 4], out);
    }
}

/// One row of `bgra_to_yuyv`: `row` is `width * 4` bytes.
fn convert_row(row: &[u8], out: &mut [u8]) {
    let (pairs, rest) = row.as_chunks::<8>();
    let (quads, _) = out.as_chunks_mut::<4>();
    for (pair, quad) in pairs.iter().zip(quads.iter_mut()) {
        let (b0, g0, r0) = (pair[0] as i32, pair[1] as i32, pair[2] as i32);
        let (b1, g1, r1) = (pair[4] as i32, pair[5] as i32, pair[6] as i32);
        let (r, g, b) = ((r0 + r1 + 1) >> 1, (g0 + g1 + 1) >> 1, (b0 + b1 + 1) >> 1);
        quad[0] = luma(r0, g0, b0);
        quad[1] = cb(r, g, b);
        quad[2] = luma(r1, g1, b1);
        quad[3] = cr(r, g, b);
    }
    if let ([b, g, r, _], Some(quad)) = (rest, quads.get_mut(pairs.len())) {
        let (b, g, r) = (*b as i32, *g as i32, *r as i32);
        quad[0] = luma(r, g, b);
        quad[1] = cb(r, g, b);
        quad[2] = luma(r, g, b);
        quad[3] = cr(r, g, b);
    }
}

// ---------------------------------------------------------------------------
// The kernel's own types and request numbers, from `linux/videodev2.h`.
// ---------------------------------------------------------------------------

const IOC_READ: u32 = 2;
const IOC_WRITE: u32 = 1;

/// `_IOWR('V', nr, type)`.
const fn v4l2_iowr(nr: u32, size: usize) -> libc::c_ulong {
    (((IOC_READ | IOC_WRITE) << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr)
        as libc::c_ulong
}

/// `_IOR('V', nr, type)`.
const fn v4l2_ior(nr: u32, size: usize) -> libc::c_ulong {
    ((IOC_READ << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr) as libc::c_ulong
}

const VIDIOC_QUERYCAP: libc::c_ulong = v4l2_ior(0, size_of::<Capability>());
const VIDIOC_G_FMT: libc::c_ulong = v4l2_iowr(4, size_of::<Format>());
const VIDIOC_S_FMT: libc::c_ulong = v4l2_iowr(5, size_of::<Format>());
const VIDIOC_S_PARM: libc::c_ulong = v4l2_iowr(22, size_of::<StreamParm>());

/// `V4L2_BUF_TYPE_VIDEO_OUTPUT`.
const BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
/// `V4L2_CAP_VIDEO_CAPTURE`, `V4L2_CAP_VIDEO_OUTPUT`, `V4L2_CAP_DEVICE_CAPS`.
const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const CAP_VIDEO_OUTPUT: u32 = 0x0000_0002;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
/// `v4l2_fourcc('Y', 'U', 'Y', 'V')`.
const PIX_FMT_YUYV: u32 = 0x5659_5559;
/// `V4L2_FIELD_NONE`.
const FIELD_NONE: u32 = 1;
/// `V4L2_CAP_TIMEPERFRAME`.
const CAP_TIMEPERFRAME: u32 = 0x1000;

/// `struct v4l2_capability`.
#[repr(C)]
#[derive(Default)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

impl Capability {
    /// This node's capabilities: `device_caps` when the driver reports it,
    /// else the whole device's.
    fn node_caps(&self) -> u32 {
        if self.capabilities & CAP_DEVICE_CAPS != 0 {
            self.device_caps
        } else {
            self.capabilities
        }
    }
}

/// `struct v4l2_pix_format`.
#[repr(C)]
#[derive(Clone, Copy)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format`: a type and a 200-byte union that holds pointers
/// (so it is 8-aligned, and the whole struct is 208 bytes on 64-bit).
#[repr(C)]
struct Format {
    type_: u32,
    _align: u32,
    pix: PixFormat,
    _rest: [u8; 152],
}

impl Format {
    /// A packed YUYV output format of `width` x `height`.
    fn yuyv(width: u32, height: u32) -> Self {
        Self {
            type_: BUF_TYPE_VIDEO_OUTPUT,
            _align: 0,
            pix: PixFormat {
                width,
                height,
                pixelformat: PIX_FMT_YUYV,
                field: FIELD_NONE,
                bytesperline: yuyv_stride(width) as u32,
                sizeimage: yuyv_len(width, height) as u32,
                colorspace: COLORSPACE,
                priv_: 0,
                flags: 0,
                ycbcr_enc: YCBCR_ENC,
                quantization: QUANTIZATION,
                xfer_func: 0,
            },
            _rest: [0; 152],
        }
    }
}

/// `struct v4l2_streamparm` for an output: the type, then
/// `struct v4l2_outputparm` in a 200-byte union.
#[repr(C)]
struct StreamParm {
    type_: u32,
    capability: u32,
    outputmode: u32,
    time_numerator: u32,
    time_denominator: u32,
    extendedmode: u32,
    writebuffers: u32,
    reserved: [u32; 4],
    _rest: [u8; 160],
}

impl StreamParm {
    fn output(fps: u32) -> Self {
        Self {
            type_: BUF_TYPE_VIDEO_OUTPUT,
            capability: CAP_TIMEPERFRAME,
            outputmode: 0,
            time_numerator: 1,
            time_denominator: fps.max(1),
            extendedmode: 0,
            writebuffers: 0,
            reserved: [0; 4],
            _rest: [0; 160],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isocast_engine::FramePool;
    use std::io::Read;

    #[test]
    fn ioctl_requests_match_the_kernel_header() {
        // The numbers (and sizes) linux/videodev2.h's own macros make on
        // x86-64, checked with a C program against the system header.
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_G_FMT, 0xc0d0_5604);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_S_PARM, 0xc0cc_5616);
        assert_eq!(size_of::<Capability>(), 104);
        assert_eq!(size_of::<PixFormat>(), 48);
        assert_eq!(size_of::<Format>(), 208);
        assert_eq!(std::mem::offset_of!(Format, pix), 8);
        assert_eq!(size_of::<StreamParm>(), 204);
        assert_eq!(std::mem::offset_of!(StreamParm, time_numerator), 12);
    }

    /// One BGRA pixel as a 1x2 frame row converts to these bytes.
    fn convert(pixels: &[[u8; 4]], width: usize) -> Vec<u8> {
        let height = pixels.len() / width;
        let src: Vec<u8> = pixels.iter().flatten().copied().collect();
        let mut dst = vec![0; yuyv_len(width as u32, height as u32)];
        bgra_to_yuyv(&src, width * 4, width, height, &mut dst);
        dst
    }

    #[test]
    fn primaries_map_to_the_bt601_limited_range_values() {
        // [Y, Cb, Cr] for each colour: the standard BT.601 studio-range
        // values. The pixels are BGRA in memory.
        let cases: [([u8; 4], [u8; 3]); 6] = [
            ([0, 0, 0, 255], [16, 128, 128]),
            ([255, 255, 255, 255], [235, 128, 128]),
            ([0, 0, 255, 255], [82, 90, 240]),
            ([0, 255, 0, 255], [144, 54, 34]),
            ([255, 0, 0, 255], [41, 240, 110]),
            ([128, 128, 128, 0], [126, 128, 128]),
        ];
        for (pixel, [y, u, v]) in cases {
            assert_eq!(convert(&[pixel, pixel], 2), [y, u, y, v], "{pixel:?}");
        }
    }

    #[test]
    fn every_colour_stays_in_the_limited_range() {
        for r in (0..=255i32).step_by(5) {
            for g in (0..=255).step_by(5) {
                for b in (0..=255).step_by(5) {
                    assert!((16..=235).contains(&(luma(r, g, b) as i32)));
                    assert!((16..=240).contains(&(cb(r, g, b) as i32)));
                    assert!((16..=240).contains(&(cr(r, g, b) as i32)));
                }
            }
        }
        // The extremes exactly, which the sampling above might step past.
        for corner in 0..8 {
            let (r, g, b) = (
                (corner & 1) * 255,
                ((corner >> 1) & 1) * 255,
                ((corner >> 2) & 1) * 255,
            );
            assert!((16..=235).contains(&(luma(r, g, b) as i32)));
            assert!((16..=240).contains(&(cb(r, g, b) as i32)));
            assert!((16..=240).contains(&(cr(r, g, b) as i32)));
        }
    }

    #[test]
    fn a_pair_of_different_colours_shares_averaged_chroma() {
        // Black beside white: each keeps its own luma, chroma is grey's.
        let out = convert(&[[0, 0, 0, 255], [255, 255, 255, 255]], 2);
        assert_eq!(out, [16, 128, 235, 128]);
        // Red beside blue: the chroma of their mean colour (128, 0, 128).
        let out = convert(&[[0, 0, 255, 255], [255, 0, 0, 255]], 2);
        let (r, g, b) = (128, 0, 128);
        assert_eq!(out, [82, cb(r, g, b), 41, cr(r, g, b)]);
    }

    #[test]
    fn a_row_stride_wider_than_the_pixels_is_respected() {
        // 2x2 pixels in rows of 16 bytes (8 of padding that must be
        // ignored, here filled with 0xff).
        let mut src = vec![0xff; 32];
        src[..8].copy_from_slice(&[0, 0, 255, 255, 0, 0, 255, 255]);
        src[16..24].copy_from_slice(&[0, 255, 0, 255, 0, 255, 0, 255]);
        let mut dst = vec![0; yuyv_len(2, 2)];
        bgra_to_yuyv(&src, 16, 2, 2, &mut dst);
        assert_eq!(dst, [82, 90, 82, 240, 144, 54, 144, 34]);
    }

    #[test]
    fn an_odd_width_pads_the_last_pair() {
        let red = [0, 0, 255, 255];
        let out = convert(&[red, red, red], 3);
        assert_eq!(yuyv_stride(3), 8);
        assert_eq!(out, [82, 90, 82, 240, 82, 90, 82, 240]);
    }

    fn card(path: &str, driver: &str, name: &str, caps: u32) -> Card {
        Card {
            path: PathBuf::from(path),
            driver: driver.into(),
            card: name.into(),
            caps,
        }
    }

    #[test]
    fn the_isocast_card_is_preferred_and_webcams_are_ignored() {
        let cards = [
            card(
                "/dev/video0",
                "uvcvideo",
                "HD Pro Webcam C920",
                CAP_VIDEO_CAPTURE,
            ),
            card(
                "/dev/video1",
                "uvcvideo",
                "HD Pro Webcam C920",
                CAP_VIDEO_CAPTURE,
            ),
            card(
                "/dev/video9",
                LOOPBACK_DRIVER,
                "Dummy video device",
                CAP_VIDEO_OUTPUT,
            ),
            card(
                "/dev/video10",
                LOOPBACK_DRIVER,
                PREFERRED_CARD,
                CAP_VIDEO_OUTPUT,
            ),
        ];
        let found = choose(&cards);
        assert_eq!(
            found.usable,
            [PathBuf::from("/dev/video10"), PathBuf::from("/dev/video9")]
        );
        assert!(found.held.is_empty());
    }

    #[test]
    fn a_capture_only_loopback_has_a_writer_already() {
        let cards = [card(
            "/dev/video10",
            LOOPBACK_DRIVER,
            PREFERRED_CARD,
            CAP_VIDEO_CAPTURE,
        )];
        let found = choose(&cards);
        assert!(found.usable.is_empty());
        assert_eq!(found.held, [PathBuf::from("/dev/video10")]);
        let message = no_device(&found, true).to_string();
        assert!(message.contains("already has a writer"), "{message}");
        assert!(message.contains("/dev/video10"), "{message}");
    }

    #[test]
    fn no_loopback_device_is_refused_naming_the_module() {
        let found = choose(&[card(
            "/dev/video0",
            "uvcvideo",
            "HD Pro Webcam C920",
            CAP_VIDEO_CAPTURE,
        )]);
        assert_eq!(found, Candidates::default());
        let message = no_device(&found, false).to_string();
        assert!(message.contains("v4l2loopback kernel module"), "{message}");
        assert!(message.contains("sudo modprobe v4l2loopback"), "{message}");
        assert!(message.contains("exclusive_caps=1"), "{message}");
        assert!(
            message.contains("sudo pacman -S v4l2loopback-dkms"),
            "{message}"
        );
        // Loaded but unreachable is a different problem with a different
        // message.
        let loaded = no_device(
            &Candidates {
                unopened: vec!["/dev/video10: Permission denied".into()],
                ..Candidates::default()
            },
            true,
        )
        .to_string();
        assert!(loaded.contains("Permission denied"), "{loaded}");
        assert!(!loaded.contains("not loaded"), "{loaded}");
    }

    #[test]
    fn the_real_search_finds_no_loopback_on_a_machine_without_the_module() {
        // Only meaningful where the module is absent (this repo's CI and
        // the dev box until the owner installs it). Note that it opens (and
        // queries, never streams) every real /dev/video* node.
        if module_loaded() {
            return;
        }
        let found = find_devices();
        assert!(found.usable.is_empty() && found.held.is_empty());
    }

    fn memfd() -> OwnedFd {
        // SAFETY: the name is NUL-terminated and the flag a constant.
        let fd = unsafe { libc::memfd_create(c"isocast-vcam-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create: {}", std::io::Error::last_os_error());
        // SAFETY: a descriptor just made and owned by nothing else.
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    #[test]
    fn send_writes_one_converted_frame_per_call() {
        let (width, height) = (64, 32);
        let mut pool = crate::frames::CpuPool::bgra(width, height);
        // Red on the left half, blue on the right.
        let mut fill = |bytes: &mut [u8], stride: usize| {
            for row in bytes.chunks_mut(stride) {
                for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    pixel.copy_from_slice(if x < 32 {
                        &[0, 0, 255, 255]
                    } else {
                        &[255, 0, 0, 255]
                    });
                }
            }
        };
        let frame = pool.draw(&mut fill).unwrap().unwrap();
        let (bytes, stride) = frame.plane(0).unwrap();
        let mut expected = vec![0; yuyv_len(width, height)];
        bgra_to_yuyv(
            &bytes,
            stride,
            width as usize,
            height as usize,
            &mut expected,
        );

        let fd = memfd();
        let raw = fd.as_raw_fd();
        let mut sink = V4l2Sink::with_fd(fd, width, height, PathBuf::from("memfd"));
        assert_eq!(sink.transport(), "v4l2loopback-yuyv");
        assert!(sink.send(&frame, Time::from_nanos(0)).unwrap());
        assert!(sink.send(&frame, Time::from_nanos(33_000_000)).unwrap());

        // Two whole frames, each exactly w*h*2 bytes and the conversion.
        let mut written = Vec::new();
        // SAFETY: `raw` is the memfd the sink still owns; duplicating it
        // gives the reader its own descriptor and offset.
        let dup = unsafe { libc::dup(raw) };
        let mut file = unsafe { std::fs::File::from_raw_fd(dup) };
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0)).unwrap();
        file.read_to_end(&mut written).unwrap();
        assert_eq!(written.len(), 2 * 64 * 32 * 2);
        assert_eq!(&written[..4096], &expected[..]);
        assert_eq!(&written[4096..], &expected[..]);
        // Left half is red, right half blue.
        assert_eq!(&written[..4], &[82, 90, 82, 240]);
        assert_eq!(&written[124..128], &[41, 240, 41, 110]);
    }

    #[test]
    fn a_wrong_sized_frame_is_an_error_not_a_garbled_write() {
        let mut pool = crate::frames::CpuPool::bgra(32, 32);
        let frame = pool.draw(&mut |bytes, _| bytes.fill(0)).unwrap().unwrap();
        let mut sink = V4l2Sink::with_fd(memfd(), 64, 32, PathBuf::from("memfd"));
        let message = sink
            .send(&frame, Time::from_nanos(0))
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("32x32") && message.contains("64x32"),
            "{message}"
        );
    }

    #[test]
    fn a_device_that_takes_no_more_is_a_dropped_frame() {
        // A pipe nobody reads: a non-blocking write of more than it holds
        // is short, and once it is full it says EAGAIN. Both are a frame
        // the loopback could not take, which the engine counts as dropped.
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors pipe2 returns.
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK) },
            0
        );
        let frame = vec![0u8; 1 << 20];
        assert!(!write_whole(fds[1], &frame).unwrap(), "short write");
        assert!(!write_whole(fds[1], &frame).unwrap(), "EAGAIN");
        // SAFETY: the two descriptors are this test's own.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}
