use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use windows::Win32::Foundation::{GENERIC_ALL, HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, DXGI_SHARED_RESOURCE_READ,
    DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter1, IDXGIFactory6, IDXGIResource1,
};
use windows::core::{Interface, PCWSTR};

use crate::native::{Event, SharedHandle};

pub const MAX_POOL_FRAMES: usize = 32;

/// A D3D11 device with a GPU timeline shared by its captures. No software
/// adapter fallback: this path promises native GPU surfaces, not CPU pixels.
#[derive(Clone)]
pub struct GraphicsDevice(Arc<Device>);

struct Device {
    raw: ID3D11Device5,
    submit: Mutex<Submit>,
    fence: ID3D11Fence,
    shared_fence: SharedHandle,
    name: String,
    luid: (u32, i32),
}

struct Submit {
    context: ID3D11DeviceContext4,
    next: u64,
}

impl GraphicsDevice {
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory6 = CreateDXGIFactory1()?;
            let adapter: IDXGIAdapter1 =
                factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)?;
            let desc = adapter.GetDesc1()?;
            let name = String::from_utf16_lossy(
                &desc.Description[..desc
                    .Description
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(desc.Description.len())],
            );
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("could not open the D3D11 capture device")?;
            let raw: ID3D11Device5 = device.context("D3D11 returned no device")?.cast()?;
            let context: ID3D11DeviceContext4 =
                context.context("D3D11 returned no context")?.cast()?;
            // WGC also submits on this device from its internal worker. Our
            // mutex alone cannot serialize those immediate-context calls.
            let multithread: ID3D11Multithread = context.cast()?;
            let _ = multithread.SetMultithreadProtected(true);
            let mut fence = None;
            raw.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)?;
            let fence: ID3D11Fence = fence.context("D3D11 returned no fence")?;
            let shared_fence =
                SharedHandle::new(fence.CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())?);
            Ok(Self(Arc::new(Device {
                raw,
                submit: Mutex::new(Submit { context, next: 1 }),
                fence,
                shared_fence,
                name,
                luid: (desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart),
            })))
        }
    }

    pub fn name(&self) -> &str {
        &self.0.name
    }

    pub fn adapter_luid(&self) -> (u32, i32) {
        self.0.luid
    }

    pub(crate) fn raw(&self) -> &ID3D11Device5 {
        &self.0.raw
    }

    pub(crate) fn completed(&self) -> Result<u64> {
        let value = unsafe { self.0.fence.GetCompletedValue() };
        if value == u64::MAX {
            bail!("D3D11 capture device was removed");
        }
        Ok(value)
    }

    pub(crate) fn notify(&self, value: u64, event: &Event) -> Result<()> {
        unsafe { self.0.fence.SetEventOnCompletion(value, event.handle()) }?;
        Ok(())
    }
}

/// One lease on an owned GPU texture. Clones share the immutable picture;
/// neither the capture pool nor the producer can reuse it until all leases
/// and the GPU copy have finished. The fence must be waited on by consumers.
#[derive(Clone)]
pub struct Frame {
    slot: Arc<Slot>,
    device: GraphicsDevice,
}

struct Slot {
    texture: ID3D11Texture2D,
    shared: SharedHandle,
    width: u32,
    height: u32,
    ready: AtomicU64,
}

impl Frame {
    pub fn width(&self) -> u32 {
        self.slot.width
    }
    pub fn height(&self) -> u32 {
        self.slot.height
    }
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.slot.texture
    }
    pub fn fence(&self) -> &ID3D11Fence {
        &self.device.0.fence
    }
    pub fn ready_value(&self) -> u64 {
        self.slot.ready.load(Ordering::Acquire)
    }
    pub fn adapter_luid(&self) -> (u32, i32) {
        self.device.adapter_luid()
    }

    pub(crate) fn shared_texture(&self) -> HANDLE {
        self.slot.shared.handle()
    }

    pub(crate) fn shared_fence(&self) -> HANDLE {
        self.device.0.shared_fence.handle()
    }

    /// For explicit CPU-side consumers/diagnostics only. The compositor and
    /// encoder must use a GPU fence wait instead of blocking their CPU thread.
    pub fn wait_ready(&self, timeout: Duration) -> Result<bool> {
        let started = Instant::now();
        loop {
            if self.device.completed()? >= self.ready_value() {
                return Ok(true);
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Ok(false);
            }
            // No event registration that could outlive a timed-out caller.
            // Native GPU consumers use fence(), never this diagnostic poll.
            std::thread::sleep(remaining.min(Duration::from_millis(1)));
        }
    }
}

pub(crate) struct TexturePool {
    device: GraphicsDevice,
    slots: Vec<Arc<Slot>>,
}

impl TexturePool {
    pub(crate) fn new(device: GraphicsDevice) -> Self {
        Self {
            device,
            slots: Vec::new(),
        }
    }

    pub(crate) fn copy(
        &mut self,
        source: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<Option<Frame>> {
        let completed = self.device.completed()?;
        // On resize, discard only idle old-size textures; retained pictures
        // keep their dimensions and still count toward the 32-frame bound.
        self.slots.retain(|slot| {
            (slot.width == width && slot.height == height)
                || Arc::strong_count(slot) > 1
                || slot.ready.load(Ordering::Acquire) > completed
        });
        let slot = self.slots.iter().find(|slot| {
            slot.width == width
                && slot.height == height
                && Arc::strong_count(slot) == 1
                && slot.ready.load(Ordering::Acquire) <= completed
        });
        let slot = match slot {
            Some(slot) => slot.clone(),
            None if self.slots.len() == MAX_POOL_FRAMES => return Ok(None),
            None => {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
                    MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED).0
                        as u32,
                    ..Default::default()
                };
                let mut texture = None;
                unsafe {
                    self.device
                        .raw()
                        .CreateTexture2D(&desc, None, Some(&mut texture))
                }?;
                let texture = texture.context("D3D11 returned no texture")?;
                let resource: IDXGIResource1 = texture.cast()?;
                let shared = unsafe {
                    SharedHandle::new(resource.CreateSharedHandle(
                        None,
                        (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0,
                        PCWSTR::null(),
                    )?)
                };
                let slot = Arc::new(Slot {
                    texture,
                    shared,
                    width,
                    height,
                    ready: AtomicU64::new(0),
                });
                self.slots.push(slot.clone());
                slot
            }
        };
        let mut submit = self.device.0.submit.lock();
        let value = submit.next;
        submit.next = value
            .checked_add(1)
            .context("D3D11 capture fence exhausted")?;
        // Mark busy BEFORE submission; a failed Signal must never allow this
        // slot to be reused while its copy might still be executing.
        slot.ready.store(value, Ordering::Release);
        unsafe {
            let bounds = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: width,
                bottom: height,
                back: 1,
            };
            submit.context.CopySubresourceRegion(
                &slot.texture,
                0,
                0,
                0,
                0,
                source,
                0,
                Some(&bounds),
            );
            submit.context.Signal(&self.device.0.fence, value)?;
            submit.context.Flush();
        }
        Ok(Some(Frame {
            slot,
            device: self.device.clone(),
        }))
    }
}
