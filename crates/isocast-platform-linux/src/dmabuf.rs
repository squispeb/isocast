//! Pictures the compositor draws on the GPU straight into our buffers,
//! the way ScreenCaptureKit hands macOS IOSurfaces: no shared-memory copy
//! on either side.
//!
//! A buffer is allocated with GBM on the compositor's render node, in a
//! layout (DRM modifier) the compositor said it can render into, then
//! imported into the engine's own Vulkan device (wgpu-hal's DMA-BUF
//! import), so each picture is one GPU-to-GPU copy into a pool frame's
//! texture. Read-backs happen only where a CPU consumer asks for bytes
//! (`gpu::read_back`), in this process, not in the compositor.
//!
//! With shared memory, Hyprland 0.56 renders each window picture into a
//! new framebuffer and reads it back synchronously inside its frame
//! (CScreenshareFrame::copyShm); with a DMA-BUF it renders into ours and
//! answers when its GPU is done. That is the whole reason for this module:
//! the readback stalls the desktop (docs/notes.md, desktop pacing).
//!
//! libgbm is loaded when first needed, not linked: a machine without it,
//! or a GPU whose Vulkan driver cannot import the layouts the compositor
//! offers, keeps the shared-memory path.

use std::ffi::{CStr, c_int, c_void};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::gpu::Gpu;
use wgpu::hal;

/// The DRM fourcc used: 32-bit BGRA with alpha in memory (Bgra8Unorm).
pub(crate) const ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
/// The modifier that means "no explicit layout": not importable with an
/// explicit modifier, so it is never asked for.
const INVALID_MODIFIER: u64 = 0x00ff_ffff_ffff_ffff;
const GBM_BO_USE_RENDERING: u32 = 1 << 2;

type CreateDevice = unsafe extern "C" fn(c_int) -> *mut c_void;
type CreateBo =
    unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const u64, u32, u32) -> *mut c_void;
type BoGetFd = unsafe extern "C" fn(*mut c_void) -> c_int;
type BoGetU32 = unsafe extern "C" fn(*mut c_void) -> u32;
type BoGetOffset = unsafe extern "C" fn(*mut c_void, c_int) -> u32;
type BoGetModifier = unsafe extern "C" fn(*mut c_void) -> u64;
type BoGetPlanes = unsafe extern "C" fn(*mut c_void) -> c_int;
type BoDestroy = unsafe extern "C" fn(*mut c_void);

/// The few libgbm entry points used, resolved once per process.
struct Gbm {
    create_device: CreateDevice,
    create_bo: CreateBo,
    bo_get_fd: BoGetFd,
    bo_get_stride: BoGetU32,
    bo_get_offset: BoGetOffset,
    bo_get_modifier: BoGetModifier,
    bo_get_plane_count: BoGetPlanes,
    bo_destroy: BoDestroy,
}

fn gbm() -> Result<&'static Gbm> {
    static GBM: OnceLock<Result<Gbm, String>> = OnceLock::new();
    GBM.get_or_init(|| load().map_err(|err| format!("{err:#}")))
        .as_ref()
        .map_err(|why| anyhow!("{why}"))
}

fn load() -> Result<Gbm> {
    // SAFETY: dlopen and dlsym with NUL-terminated names; the library is
    // never closed, so the function pointers stay valid.
    unsafe {
        let lib = libc::dlopen(c"libgbm.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if lib.is_null() {
            bail!("libgbm.so.1 is not installed");
        }
        let symbol = |name: &CStr| -> Result<*mut c_void> {
            let pointer = libc::dlsym(lib, name.as_ptr());
            if pointer.is_null() {
                bail!("libgbm has no {}", name.to_string_lossy());
            }
            Ok(pointer)
        };
        Ok(Gbm {
            create_device: std::mem::transmute::<*mut c_void, CreateDevice>(symbol(
                c"gbm_create_device",
            )?),
            create_bo: std::mem::transmute::<*mut c_void, CreateBo>(symbol(
                c"gbm_bo_create_with_modifiers2",
            )?),
            bo_get_fd: std::mem::transmute::<*mut c_void, BoGetFd>(symbol(c"gbm_bo_get_fd")?),
            bo_get_stride: std::mem::transmute::<*mut c_void, BoGetU32>(symbol(
                c"gbm_bo_get_stride",
            )?),
            bo_get_offset: std::mem::transmute::<*mut c_void, BoGetOffset>(symbol(
                c"gbm_bo_get_offset",
            )?),
            bo_get_modifier: std::mem::transmute::<*mut c_void, BoGetModifier>(symbol(
                c"gbm_bo_get_modifier",
            )?),
            bo_get_plane_count: std::mem::transmute::<*mut c_void, BoGetPlanes>(symbol(
                c"gbm_bo_get_plane_count",
            )?),
            bo_destroy: std::mem::transmute::<*mut c_void, BoDestroy>(symbol(c"gbm_bo_destroy")?),
        })
    }
}

/// A GBM device on one render node. Kept for the process: buffer objects
/// must not outlive it, and every capture shares it.
pub(crate) struct Allocator {
    device: *mut c_void,
    /// The render node, open for the device's lifetime.
    _node: OwnedFd,
}

// SAFETY: a gbm_device is only used to create and free buffer objects,
// which libgbm allows from any thread.
unsafe impl Send for Allocator {}
unsafe impl Sync for Allocator {}

impl Allocator {
    /// The allocator for the compositor's main device (`dev`, the
    /// `dev_t` its capture session names), or the first render node when
    /// it names none.
    pub(crate) fn for_device(dev: Option<u64>) -> Result<&'static Allocator> {
        static ALLOCATOR: OnceLock<Result<Allocator, String>> = OnceLock::new();
        ALLOCATOR
            .get_or_init(|| Self::open(dev).map_err(|err| format!("{err:#}")))
            .as_ref()
            .map_err(|why| anyhow!("{why}"))
    }

    fn open(dev: Option<u64>) -> Result<Self> {
        let gbm = gbm()?;
        let path = render_node(dev)?;
        let node = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {path}"))?;
        let node = OwnedFd::from(node);
        // SAFETY: a valid, open DRM fd that outlives the device.
        let device = unsafe { (gbm.create_device)(node.as_raw_fd()) };
        if device.is_null() {
            bail!("libgbm could not open {path}");
        }
        Ok(Self {
            device,
            _node: node,
        })
    }

    /// A `width`x`height` buffer in `fourcc`, in one of `modifiers` (the
    /// ones the compositor offered).
    pub(crate) fn allocate(
        &self,
        width: u32,
        height: u32,
        fourcc: u32,
        modifiers: &[u64],
    ) -> Result<Buffer> {
        let gbm = gbm()?;
        let modifiers: Vec<u64> = modifiers
            .iter()
            .copied()
            .filter(|modifier| *modifier != INVALID_MODIFIER)
            .collect();
        if modifiers.is_empty() {
            bail!("the compositor offers no explicit buffer layout");
        }
        // SAFETY: the device is live; the modifier list is valid for the call.
        let bo = unsafe {
            (gbm.create_bo)(
                self.device,
                width,
                height,
                fourcc,
                modifiers.as_ptr(),
                modifiers.len() as u32,
                GBM_BO_USE_RENDERING,
            )
        };
        if bo.is_null() {
            bail!("libgbm could not allocate a {width}x{height} buffer in any offered layout");
        }
        let buffer = Buffer {
            bo,
            // SAFETY (four calls): `bo` is the live buffer object just made.
            stride: unsafe { (gbm.bo_get_stride)(bo) },
            offset: unsafe { (gbm.bo_get_offset)(bo, 0) },
            modifier: unsafe { (gbm.bo_get_modifier)(bo) },
            planes: unsafe { (gbm.bo_get_plane_count)(bo) },
        };
        if buffer.planes != 1 {
            bail!(
                "the buffer layout has {} planes; only one-plane layouts are imported",
                buffer.planes
            );
        }
        Ok(buffer)
    }
}

/// The `/dev/dri/renderD*` node of the device `dev` (a DRM primary or
/// render node's `dev_t`): the one whose sysfs device is the same.
fn render_node(dev: Option<u64>) -> Result<String> {
    let device_of = |path: &str| std::fs::canonicalize(path).ok();
    let wanted = dev.and_then(|dev| {
        let (major, minor) = (libc::major(dev), libc::minor(dev));
        device_of(&format!("/sys/dev/char/{major}:{minor}/device"))
    });
    let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .context("listing /dev/dri")?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("renderD"))
        .collect();
    nodes.sort();
    for node in &nodes {
        let theirs = device_of(&format!("/sys/class/drm/{node}/device"));
        if wanted.is_none() || theirs == wanted {
            return Ok(format!("/dev/dri/{node}"));
        }
    }
    bail!("no render node for the compositor's GPU in /dev/dri")
}

/// One GBM buffer object: the compositor's target, imported into Vulkan.
pub(crate) struct Buffer {
    bo: *mut c_void,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    planes: c_int,
}

// SAFETY: the buffer object is only read for its layout and freed once.
unsafe impl Send for Buffer {}

impl Buffer {
    /// A new descriptor for the buffer's memory (for the compositor, or
    /// for an import, each of which takes its own).
    pub(crate) fn fd(&self) -> Result<OwnedFd> {
        let gbm = gbm()?;
        // SAFETY: the live buffer object; a new fd owned by the caller.
        let fd = unsafe { (gbm.bo_get_fd)(self.bo) };
        if fd < 0 {
            bail!("libgbm could not export the buffer");
        }
        // SAFETY: a fresh descriptor nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if let Ok(gbm) = gbm() {
            // SAFETY: the buffer object made in `allocate`, freed once.
            unsafe { (gbm.bo_destroy)(self.bo) };
        }
    }
}

/// Whether the engine's GPU can import DMA-BUFs at all (the device was
/// opened with the feature; see `isocast_engine::gpu`).
pub(crate) fn importable(gpu: &Gpu) -> bool {
    gpu.device
        .features()
        .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
}

/// `fd`, a one-plane DMA-BUF of `width`x`height` 32-bit BGRA pixels laid
/// out as `modifier`, `stride` and `offset` say, as a texture of the
/// engine's GPU that copies can read from.
pub(crate) fn import(
    gpu: &Gpu,
    fd: BorrowedFd<'_>,
    width: u32,
    height: u32,
    modifier: u64,
    stride: u32,
    offset: u32,
) -> Result<wgpu::Texture> {
    if !importable(gpu) {
        bail!("the GPU's Vulkan driver cannot import DMA-BUFs");
    }
    let fd = fd
        .try_clone_to_owned()
        .context("duplicating the buffer's descriptor")?;
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let format = wgpu::TextureFormat::Bgra8Unorm;
    // SAFETY: the device is the engine's; the descriptor describes the
    // buffer exactly (one plane, its modifier, stride and offset); wgpu
    // owns the resulting image and frees its memory with the texture.
    unsafe {
        let hal_device = gpu
            .device
            .as_hal::<hal::api::Vulkan>()
            .context("the engine's GPU is not a Vulkan device")?;
        let texture = hal_device
            .texture_from_dmabuf_fd(
                fd,
                &hal::TextureDescriptor {
                    label: Some("isocast DMA-BUF"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUses::COPY_SRC,
                    memory_flags: hal::MemoryFlags::empty(),
                    view_formats: Vec::new(),
                },
                modifier,
                u64::from(stride),
                u64::from(offset),
            )
            .map_err(|err| anyhow!("the GPU refused the DMA-BUF ({modifier:#x}): {err}"))?;
        drop(hal_device);
        Ok(gpu.device.create_texture_from_hal::<hal::api::Vulkan>(
            texture,
            &wgpu::TextureDescriptor {
                label: Some("isocast DMA-BUF"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            },
            wgpu::TextureUses::UNINITIALIZED,
        ))
    }
}

/// The one-plane layouts (DRM modifiers) the engine's GPU can import a
/// 32-bit BGRA DMA-BUF in and copy out of: what a stream may be offered
/// in. Empty when the GPU cannot import at all.
pub(crate) fn importable_modifiers(gpu: &Gpu) -> Vec<u64> {
    use ash::vk;
    if !importable(gpu) {
        return Vec::new();
    }
    // SAFETY: read-only queries on the engine's own physical device.
    unsafe {
        let Some(device) = gpu.device.as_hal::<hal::api::Vulkan>() else {
            return Vec::new();
        };
        let instance = device.shared_instance().raw_instance();
        let physical = device.raw_physical_device();
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(
            physical,
            vk::Format::B8G8R8A8_UNORM,
            &mut properties,
        );
        let count = list.drm_format_modifier_count as usize;
        let mut modifiers = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut modifiers);
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(
            physical,
            vk::Format::B8G8R8A8_UNORM,
            &mut properties,
        );
        modifiers
            .iter()
            .filter(|modifier| {
                modifier.drm_format_modifier_plane_count == 1
                    && modifier
                        .drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::TRANSFER_SRC)
            })
            .map(|modifier| modifier.drm_format_modifier)
            .collect()
    }
}
