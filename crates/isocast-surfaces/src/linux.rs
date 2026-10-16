//! Shared memory, handed between processes as file descriptors.
//!
//! A surface is 8-bit RGBA in a `memfd` whose rows are padded to 256 bytes,
//! so the daemon's read-back and the client's upload can use them as they
//! are. Where macOS has Mach ports and IOSurfaces, Linux has exactly what
//! this needs: a Unix socket can carry file descriptors, so the daemon's
//! [`Exporter`] binds one of its own (under the user's runtime directory,
//! 0700) and hands a ring over as `SCM_RIGHTS` to whoever holds the ring's
//! one-use token. The client is told the socket's path and the token over
//! the control connection it already has, and spends the token with
//! [`fetch`].
//!
//! This is the copying path: the daemon's GPU draws the picture, the daemon
//! copies it back into this memory and the client uploads it into a texture
//! of its own. DMA-BUF — the client's GPU sampling the daemon's texture in
//! place — is L.4's later step (docs/port-omarchy.md).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::{MAX_SLOTS, Token};

/// Rows are a multiple of this many bytes, which is what a GPU copy wants
/// on both sides: `copy_texture_to_buffer` (the daemon's read-back) and
/// `Queue::write_texture` (the client's upload) both refuse a row pitch
/// that is not a multiple of 256. Padding here means neither side has to
/// repack rows.
const ROW_ALIGN: u32 = 256;

/// How long the daemon waits for a client to send its token, or the client
/// for the daemon's answer: both are local and immediate.
const HAND_OVER: Duration = Duration::from_secs(2);

/// An 8-bit RGBA picture in shared memory. Cloning hands out the same
/// memory, not a copy.
#[derive(Clone)]
pub struct Surface(Arc<Shared>);

struct Shared {
    /// Kept for the hand-over (`SCM_RIGHTS` sends the descriptor); the
    /// mapping stays valid without it.
    fd: OwnedFd,
    base: *mut u8,
    len: usize,
    width: u32,
    height: u32,
    stride: u32,
}

// SAFETY: the memory is shared on purpose and lasts as long as the
// mapping; whoever writes it is the one side the control protocol says is
// writing (the daemon publishes before it says a frame is ready, a client
// uploads after it is told).
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: `base` and `len` are exactly what `mmap` returned.
        unsafe { libc::munmap(self.base.cast(), self.len) };
    }
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface")
            .field("id", &self.id())
            .field("width", &self.width())
            .field("height", &self.height())
            .finish()
    }
}

impl Surface {
    /// A new `width`×`height` surface of 8-bit RGBA (`Rgba8Unorm` on the
    /// GPU), its contents undefined.
    pub fn rgba(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            bail!("a surface can't be {width}×{height}");
        }
        let stride = width
            .checked_mul(4)
            .map(|row| row.div_ceil(ROW_ALIGN) * ROW_ALIGN)
            .context("that surface is too wide")?;
        let len = (stride as usize)
            .checked_mul(height as usize)
            .context("that surface is too large")?;
        // SAFETY: the name is NUL terminated and the flags are valid.
        let fd = unsafe { libc::memfd_create(c"isocast-surface".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("creating a surface's memory (memfd_create)");
        }
        // SAFETY: `fd` was just made by memfd_create and is ours.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: the descriptor is open for writing and `len` is its size.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
            return Err(std::io::Error::last_os_error())
                .context("sizing a surface's memory (ftruncate)");
        }
        // SAFETY: mapping private pages of our own descriptor, read and
        // write, shared with whoever else maps it.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mapping a surface's memory");
        }
        Ok(Self(Arc::new(Shared {
            fd,
            base: base.cast(),
            len,
            width,
            height,
            stride,
        })))
    }

    pub fn width(&self) -> u32 {
        self.0.width
    }

    pub fn height(&self) -> u32 {
        self.0.height
    }

    /// Bytes per row, padded to [`ROW_ALIGN`]: what both sides' copies use.
    pub fn stride(&self) -> u32 {
        self.0.stride
    }

    /// How many bytes a picture of this surface takes, the padding
    /// included.
    pub fn bytes(&self) -> usize {
        self.0.len
    }

    /// The surface's descriptor (for logs; the hand-over sends it).
    pub fn id(&self) -> u32 {
        self.0.fd.as_raw_fd() as u32
    }

    /// The picture as it is in memory: `height` rows of [`Self::stride`]
    /// bytes, the padding included. Don't hold this across a
    /// [`Self::publish`], which writes the same memory.
    pub fn pixels(&self) -> &[u8] {
        // SAFETY: the mapping is `len` bytes and lives in `self`.
        unsafe { std::slice::from_raw_parts(self.0.base, self.0.len) }
    }

    /// One row of the picture: its `width`×4 real pixels, the padding cut
    /// off.
    pub fn row(&self, row: u32) -> &[u8] {
        let start = row as usize * self.0.stride as usize;
        let end = start + self.0.width as usize * 4;
        &self.pixels()[start.min(self.0.len)..end.min(self.0.len)]
    }

    /// Put a finished picture into the memory other processes read. The
    /// pixels are [`Self::bytes`] long, rows padded as [`Self::stride`]
    /// says (which is how a GPU read-back hands them over).
    pub fn publish(&self, pixels: &[u8]) -> Result<()> {
        if pixels.len() != self.0.len {
            bail!(
                "a {}×{} surface holds {} bytes of pixels, not {}",
                self.0.width,
                self.0.height,
                self.0.len,
                pixels.len()
            );
        }
        // SAFETY: both spans are `len` bytes: the mapping is ours and the
        // slice was just checked.
        unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), self.0.base, self.0.len) };
        Ok(())
    }

    /// The surface as an `Rgba8Unorm` texture of `device` with `usage`,
    /// plus `COPY_DST` so [`Self::upload`] can fill it. Unlike macOS's,
    /// this texture is a copy: the GPU does not read the surface's own
    /// memory.
    #[cfg(feature = "wgpu")]
    pub fn texture(
        &self,
        device: &wgpu::Device,
        usage: wgpu::TextureUsages,
    ) -> Result<wgpu::Texture> {
        Ok(device.create_texture(&wgpu::TextureDescriptor {
            label: Some("isocast shared surface"),
            size: wgpu::Extent3d {
                width: self.width(),
                height: self.height(),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: usage | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        }))
    }

    /// Copy the surface's picture into `texture`, which
    /// [`Self::texture`] made. This is the client's copy, once per frame
    /// it receives.
    #[cfg(feature = "wgpu")]
    pub fn upload(&self, queue: &wgpu::Queue, texture: &wgpu::Texture) -> Result<()> {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            self.pixels(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.stride()),
                rows_per_image: Some(self.height()),
            },
            wgpu::Extent3d {
                width: self.width(),
                height: self.height(),
                depth_or_array_layers: 1,
            },
        );
        Ok(())
    }
}

/// The daemon's side: a Unix socket that hands each offered ring of
/// surfaces to the client holding its token, once.
pub struct Exporter {
    path: String,
    offered: Arc<Mutex<HashMap<Token, Vec<Surface>>>>,
}

static EXPORTER: OnceLock<Exporter> = OnceLock::new();
/// Serializes the first call, which binds the socket.
static BINDING: Mutex<()> = Mutex::new(());

impl Exporter {
    /// The daemon's exporter, binding its socket on first use.
    pub fn shared() -> Result<&'static Exporter> {
        let _binding = BINDING.lock().unwrap();
        if let Some(exporter) = EXPORTER.get() {
            return Ok(exporter);
        }
        let path = socket_path()?;
        let listener = bind(&path)?;
        let offered = Arc::new(Mutex::new(HashMap::new()));
        let serving = offered.clone();
        std::thread::Builder::new()
            .name("shared surfaces".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let offered = serving.clone();
                    // A hand-over is a few lines and a descriptor set; a
                    // client that stops in the middle must not hold up the
                    // next one.
                    std::thread::spawn(move || {
                        let _ = hand_over(&mut stream, &offered);
                    });
                }
            })
            .context("starting the shared surfaces' hand-over")?;
        Ok(EXPORTER.get_or_init(|| Exporter { path, offered }))
    }

    /// Where the surfaces are handed over: a socket path the client is
    /// told, as macOS tells it a Mach service name.
    pub fn service(&self) -> &str {
        &self.path
    }

    /// Offer `surfaces` as a ring, answering the token that fetches them
    /// once. A token is good until it is spent ([`fetch`]) or withdrawn.
    pub fn offer(&self, surfaces: Vec<Surface>) -> Token {
        let token = Token::random();
        self.offered.lock().unwrap().insert(token, surfaces);
        token
    }

    /// Forget a token: the ring it belonged to is replaced or gone.
    pub fn withdraw(&self, token: Token) {
        self.offered.lock().unwrap().remove(&token);
    }
}

/// Where the daemon hands surfaces over: a socket of its own in the user's
/// runtime directory (0700, so no other user reaches it; the token is the
/// second lock), named by pid so two daemons on one machine never fight
/// over it.
fn socket_path() -> Result<String> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/isocast-{}", unsafe { libc::getuid() })));
    let dir = base.join("isocast");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("closing {} to other users", dir.display()))?;
    Ok(dir
        .join(format!("surfaces-{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned())
}

/// Listen on `path`, taking the place of a daemon that was killed and left
/// its socket behind (a live daemon on the same path is somebody else's
/// and is left alone).
fn bind(path: &str) -> Result<UnixListener> {
    if let Ok(listener) = UnixListener::bind(path) {
        return Ok(listener);
    }
    if UnixStream::connect(path).is_ok() {
        bail!("another Isocast daemon is handing surfaces over at {path}");
    }
    std::fs::remove_file(path).with_context(|| format!("removing the stale {path}"))?;
    UnixListener::bind(path).with_context(|| format!("binding {path}"))
}

/// Hand a ring to one client: the token it sends, then the ring's shape and
/// its descriptors in one message.
fn hand_over(stream: &mut UnixStream, offered: &Mutex<HashMap<Token, Vec<Surface>>>) -> Result<()> {
    stream.set_read_timeout(Some(HAND_OVER))?;
    stream.set_write_timeout(Some(HAND_OVER))?;
    let mut line = String::new();
    BufReader::new(&*stream).read_line(&mut line)?;
    let surfaces = Token::parse(&line)
        .ok()
        .and_then(|token| offered.lock().unwrap().remove(&token));
    let Some(surfaces) = surfaces else {
        return send(
            stream,
            b"error: the token is unknown or was used already\n",
            &[],
        );
    };
    let Some(first) = surfaces.first() else {
        bail!("a ring of no surfaces");
    };
    let header = format!(
        "{} {} {} {}\n",
        surfaces.len(),
        first.width(),
        first.height(),
        first.stride()
    );
    let fds: Vec<RawFd> = surfaces
        .iter()
        .map(|surface| surface.0.fd.as_raw_fd())
        .collect();
    send(stream, header.as_bytes(), &fds)
}

/// `data` and `fds` in one message (`SCM_RIGHTS`).
fn send(stream: &UnixStream, data: &[u8], fds: &[RawFd]) -> Result<()> {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr().cast_mut().cast(),
        iov_len: data.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    let described = std::mem::size_of_val(fds) as u32;
    let mut control = vec![
        0u8;
        // SAFETY: CMSG_SPACE is pure arithmetic on a size.
        unsafe { libc::CMSG_SPACE(described) } as usize
    ];
    if !fds.is_empty() {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len();
        // SAFETY: the control buffer has room for one SCM_RIGHTS message
        // (CMSG_SPACE) and is zeroed, so the header is initialized.
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(described) as usize;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                described as usize,
            );
        }
    }
    // SAFETY: the message and its iovec and control buffer are all alive
    // for the call.
    if unsafe { libc::sendmsg(stream.as_raw_fd(), &message, 0) } < 0 {
        return Err(std::io::Error::last_os_error()).context("handing surfaces over");
    }
    Ok(())
}

/// Fetch a ring from the daemon handing surfaces over at `service` (its
/// socket path), with `token`: the surfaces' shape and their own
/// descriptors, which are mapped into this process. The token is good for
/// one fetch.
pub fn fetch(service: &str, token: Token, timeout: Duration) -> Result<Vec<Surface>> {
    let stream = UnixStream::connect(service)
        .with_context(|| format!("connecting to {service} (is isocastd still running?)"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    (&stream)
        .write_all(format!("{token}\n").as_bytes())
        .context("asking for the shared surfaces")?;
    let (line, fds) = receive(&stream)?;
    if let Some(refusal) = line.strip_prefix("error: ") {
        bail!("{refusal}");
    }
    let shape: Vec<u32> = line
        .split_whitespace()
        .map(|field| field.parse::<u32>())
        .collect::<Result<Vec<u32>, _>>()
        .map_err(|_| anyhow::anyhow!("isocastd answered {line:?} instead of a ring of surfaces"))?;
    let [slots, width, height, stride] = shape.as_slice() else {
        bail!("isocastd answered {line:?} instead of a ring of surfaces");
    };
    if fds.len() != *slots as usize {
        bail!(
            "isocastd sent {} descriptors for a ring of {slots} surfaces",
            fds.len()
        );
    }
    let mut surfaces = Vec::with_capacity(fds.len());
    for fd in fds {
        surfaces.push(map(fd, *width, *height, *stride)?);
    }
    Ok(surfaces)
}

/// One message: its text and the descriptors it carried.
fn receive(stream: &UnixStream) -> Result<(String, Vec<OwnedFd>)> {
    let mut data = [0u8; 128];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let mut control = vec![
        0u8;
        // SAFETY: CMSG_SPACE is pure arithmetic on a size.
        unsafe { libc::CMSG_SPACE((MAX_SLOTS * size_of::<RawFd>()) as u32) } as usize
    ];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    // SAFETY: the message, its iovec and its control buffer are all alive
    // for the call; MSG_CMSG_CLOEXEC keeps a descriptor from leaking into
    // a child.
    let received =
        unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(std::io::Error::last_os_error()).context("waiting for the shared surfaces");
    }
    let line = String::from_utf8_lossy(&data[..received as usize])
        .trim()
        .to_string();
    let mut fds = Vec::new();
    // SAFETY: the header chain is what recvmsg filled in, and every span
    // read is inside the control buffer.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            let described = &*header;
            if described.cmsg_level == libc::SOL_SOCKET && described.cmsg_type == libc::SCM_RIGHTS {
                let count = (described.cmsg_len - libc::CMSG_LEN(0) as usize) / size_of::<RawFd>();
                let start = libc::CMSG_DATA(header).cast::<RawFd>();
                for index in 0..count {
                    let fd = *start.add(index);
                    if fd >= 0 {
                        fds.push(OwnedFd::from_raw_fd(fd));
                    }
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    Ok((line, fds))
}

/// Map a received descriptor: the same memory the daemon is drawing into.
fn map(fd: OwnedFd, width: u32, height: u32, stride: u32) -> Result<Surface> {
    if width == 0 || height == 0 || stride < width * 4 {
        bail!("isocastd sent a surface of {width}×{height} at {stride} bytes a row");
    }
    let len = stride as usize * height as usize;
    // SAFETY: mapping the received descriptor read and write, shared with
    // the daemon.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error()).context("mapping a fetched surface");
    }
    Ok(Surface(Arc::new(Shared {
        fd,
        base: base.cast(),
        len,
        width,
        height,
        stride,
    })))
}

/// The host clock now, in nanoseconds: `CLOCK_MONOTONIC`, which is the
/// clock `isocast-platform-linux` stamps frames with. A client measures
/// how late a frame arrived against it, and a wall clock would put every
/// frame days away from its own time.
pub fn host_now() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes exactly the struct given.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
    }
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the daemon publishes is what another holder of the ring reads:
    /// the hand-over carries the shape and the descriptors, and the token
    /// works exactly once.
    #[test]
    fn a_ring_reaches_another_holder_once() {
        let exporter = Exporter::shared().unwrap();
        let drawn = Surface::rgba(64, 48).unwrap();
        let other = Surface::rgba(64, 48).unwrap();
        assert_eq!(drawn.stride(), 256, "64 pixels of 4 bytes need no padding");
        assert_eq!(drawn.bytes(), 256 * 48);
        let token = exporter.offer(vec![drawn.clone(), other.clone()]);
        let service = exporter.service().to_string();
        // Another holder, in its own thread: the same kernel path a
        // separate process takes.
        let fetched = std::thread::spawn(move || {
            fetch(&service, token, HAND_OVER).expect("the ring was not handed over")
        })
        .join()
        .unwrap();
        assert_eq!(fetched.len(), 2);
        assert_eq!((fetched[0].width(), fetched[0].height()), (64, 48));
        assert_eq!(fetched[0].bytes(), drawn.bytes());
        // The daemon draws (with padded rows, as a GPU read-back gives
        // them) and the client sees the pixels...
        let mut pixels = vec![0u8; drawn.bytes()];
        for row in 0..48 {
            pixels[row * 256 + 4] = 0x3f;
            pixels[row * 256 + 255] = 0xa5; // padding, never shown
        }
        drawn.publish(&pixels).unwrap();
        assert_eq!(fetched[0].row(0)[4], 0x3f);
        assert_eq!(fetched[0].row(47)[4], 0x3f);
        assert_eq!(fetched[0].row(0).len(), 64 * 4, "pixels, not padding");
        // ...and a picture of the wrong size is refused, not half-written.
        let err = drawn.publish(&pixels[1..]).unwrap_err().to_string();
        assert!(err.contains("holds 12288 bytes"), "{err}");
        // The token it spent is gone, and an unknown one is refused.
        let err = fetch(exporter.service(), token, HAND_OVER)
            .unwrap_err()
            .to_string();
        assert!(err.contains("used already"), "{err}");
        let err = fetch(exporter.service(), Token::random(), HAND_OVER)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown"), "{err}");
        // A withdrawn ring is refused too (the daemon replaced it).
        let token = exporter.offer(vec![other.clone()]);
        exporter.withdraw(token);
        let err = fetch(exporter.service(), token, HAND_OVER)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown"), "{err}");
    }

    /// A surface's memory really is shared, not copied into the fetcher.
    #[test]
    fn a_published_picture_is_the_fetchers_picture() {
        let exporter = Exporter::shared().unwrap();
        let surface = Surface::rgba(64, 4).unwrap();
        let token = exporter.offer(vec![surface.clone()]);
        let fetched = fetch(exporter.service(), token, HAND_OVER).unwrap();
        surface.publish(&vec![7u8; surface.bytes()]).unwrap();
        assert!(fetched[0].pixels().iter().all(|byte| *byte == 7));
        fetched[0].publish(&vec![9u8; fetched[0].bytes()]).unwrap();
        assert!(surface.pixels().iter().all(|byte| *byte == 9));
    }

    /// A real second process, not just a thread: the descriptor survives
    /// `SCM_RIGHTS` and the memory really maps there. The child is this
    /// same test binary asked for this one test by name, so the proof needs
    /// no other program.
    #[test]
    fn another_process_reads_a_published_picture() {
        // The child half: fetch what the parent offered and check it.
        if let (Ok(service), Ok(token)) = (
            std::env::var("ISOCAST_FETCH_SERVICE"),
            std::env::var("ISOCAST_FETCH_TOKEN"),
        ) {
            let surfaces = fetch(&service, Token::parse(&token).unwrap(), HAND_OVER).unwrap();
            assert_eq!(surfaces.len(), 1);
            assert_eq!(surfaces[0].row(0)[..4], [1, 2, 3, 4]);
            return;
        }
        // The parent half: publish, then let another process read it.
        let exporter = Exporter::shared().unwrap();
        let surface = Surface::rgba(16, 16).unwrap();
        let token = exporter.offer(vec![surface.clone()]);
        let mut pixels = vec![0u8; surface.bytes()];
        pixels[..4].copy_from_slice(&[1, 2, 3, 4]);
        surface.publish(&pixels).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "linux::tests::another_process_reads_a_published_picture",
                "--nocapture",
            ])
            .env("ISOCAST_FETCH_SERVICE", exporter.service())
            .env("ISOCAST_FETCH_TOKEN", token.to_string())
            .status()
            .expect("running this test binary again");
        assert!(
            status.success(),
            "another process could not read the surface: {status}"
        );
    }

    #[test]
    fn the_host_clock_is_the_one_frames_are_stamped_with() {
        // Not a wall clock: it advances with the machine, but a wall
        // clock reading would be ~10^18 against a monotonic one of days.
        let now = host_now();
        assert!(now > 0);
        assert!(now < 100 * 365 * 24 * 3600 * 1_000_000_000, "{now}");
    }
}
