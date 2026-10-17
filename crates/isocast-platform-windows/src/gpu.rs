//! D3D11 capture textures sampled in place by wgpu/DX12. No pixel upload or
//! readback: opening the NT handle maps the same allocation on the same GPU.
//! D3D11's copy-completion fence gates the actual wgpu rendering queue.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_engine::gpu::{FrameImporter, Gpu, GpuFrame, GpuPixels};
use wgpu::hal;
use windows::Win32::Graphics::Direct3D11::{ID3D11Fence, ID3D11Texture2D};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::core::{GUID, IUnknown, Interface, implement};

use crate::Frame;

const KEPT_SURFACES: usize = 16;
const MAX_SURFACES: usize = 64;
const SURFACE_TTL: Duration = Duration::from_secs(2);
const FRAME_LEASE: GUID = GUID::from_u128(0x5c2880e7_2659_47f3_9296_a7990b7861e1);

/// The default import owns a frame lease until wgpu releases its native
/// resource after GPU completion. Private data belongs to this particular
/// OpenSharedHandle result, not to the D3D11 texture or other DX12 opens;
/// retaining it therefore cannot create a cycle back to the pool's slot.
#[implement()]
struct FrameLease {
    _frame: Frame,
}

pub struct Dx12Importer {
    device: wgpu::Device,
    raw: ID3D12Device,
    queue: ID3D12CommandQueue,
    luid: (u32, i32),
    // A process normally shares one D3D11 device. Keep only the most recent
    // fence pair, so repeated device replacements cannot grow this cache.
    fence: Option<(ID3D11Fence, ID3D12Fence)>,
    surfaces: HashMap<usize, Surface>,
}

struct Surface {
    // A native reference prevents pointer reuse (the cache key), but is NOT
    // a frame lease: the pool can reuse the slot once its consumers finish.
    _source: ID3D11Texture2D,
    texture: wgpu::Texture,
    used: Instant,
}

impl Dx12Importer {
    pub fn new(gpu: &Gpu) -> Result<Self> {
        let raw = unsafe { gpu.device.as_hal::<hal::api::Dx12>() }
            .context("Windows frame import requires a wgpu DX12 device")?
            .raw_device()
            .clone();
        // Device::raw_queue is the separate PRESENTATION queue. Waiting on
        // it does not gate the compositor's work; use Queue::as_raw instead.
        let queue = unsafe { gpu.queue.as_hal::<hal::api::Dx12>() }
            .context("Windows frame import requires a wgpu DX12 queue")?
            .as_raw()
            .clone();
        let luid = unsafe { raw.GetAdapterLuid() };
        Ok(Self {
            device: gpu.device.clone(),
            raw,
            queue,
            luid: (luid.LowPart, luid.HighPart),
            fence: None,
            surfaces: HashMap::new(),
        })
    }

    fn check_adapter(&self, frame: &Frame) -> Result<()> {
        if frame.adapter_luid() != self.luid {
            bail!(
                "Windows frame adapter {:?} differs from compositor adapter {:?}; open capture and wgpu on the same GPU",
                frame.adapter_luid(),
                self.luid
            );
        }
        Ok(())
    }

    fn wait(&mut self, frame: &Frame) -> Result<()> {
        if self
            .fence
            .as_ref()
            .is_none_or(|(source, _)| source.as_raw() != frame.fence().as_raw())
        {
            let mut fence = None;
            unsafe { self.raw.OpenSharedHandle(frame.shared_fence(), &mut fence) }
                .context("could not open the capture fence on DX12")?;
            self.fence = Some((
                frame.fence().clone(),
                fence.context("DX12 returned no capture fence")?,
            ));
        }
        let (_, fence) = self.fence.as_ref().expect("opened above");
        // GPU-side wait only. D3D12 queue operations are thread-safe/FIFO:
        // any future wgpu submission using the returned texture follows this
        // wait, even when another producer submits between import and draw.
        unsafe { self.queue.Wait(fence, frame.ready_value()) }
            .context("could not wait for the captured texture on DX12")?;
        Ok(())
    }

    fn texture(&self, frame: &Frame, retain: bool) -> Result<wgpu::Texture> {
        let mut resource: Option<ID3D12Resource> = None;
        unsafe {
            self.raw
                .OpenSharedHandle(frame.shared_texture(), &mut resource)
        }
        .context("could not open the captured texture on DX12")?;
        let resource = resource.context("DX12 returned no capture texture")?;
        let desc = unsafe { resource.GetDesc() };
        if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
            || desc.Width != u64::from(frame.width())
            || desc.Height != frame.height()
            || desc.DepthOrArraySize != 1
            || desc.MipLevels != 1
            || desc.SampleDesc.Count != 1
            || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            || !desc
                .Flags
                .contains(D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS)
        {
            bail!(
                "DX12 capture texture must be a single-plane BGRA simultaneous-access texture matching the frame"
            );
        }
        if retain {
            let lease: IUnknown = FrameLease {
                _frame: frame.clone(),
            }
            .into();
            unsafe { resource.SetPrivateDataInterface(&FRAME_LEASE, &lease) }?;
        }
        let size = wgpu::Extent3d {
            width: frame.width(),
            height: frame.height(),
            depth_or_array_layers: 1,
        };
        let format = wgpu::TextureFormat::Bgra8Unorm;
        let texture = unsafe {
            hal::dx12::Device::texture_from_raw(
                resource,
                format,
                wgpu::TextureDimension::D2,
                size,
                1,
                1,
            )
        };
        // SAFETY: the opened resource belongs to this DX12 device and has
        // exactly this layout. D3D11 sharing hands it over in COMMON, with
        // contents preserved (PRESENT maps to COMMON, not UNINITIALIZED).
        // It is read-only here. Simultaneous-access textures decay to COMMON
        // after ExecuteCommandLists, even after explicit barriers; on reuse
        // DX12 permits the previous promotable read state as StateBefore.
        // The frame lease (or cached-import caller) prevents a D3D11 write
        // until every GPU reader is done. No external write overlaps a read.
        Ok(unsafe {
            self.device.create_texture_from_hal::<hal::api::Dx12>(
                texture,
                &wgpu::TextureDescriptor {
                    label: Some("captured D3D11 texture"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::TextureUses::PRESENT,
            )
        })
    }

    fn forget_old(&mut self, now: Instant) {
        if self.surfaces.len() < KEPT_SURFACES {
            return;
        }
        self.surfaces
            .retain(|_, surface| now.duration_since(surface.used) < SURFACE_TTL);
        while self.surfaces.len() >= MAX_SURFACES {
            let oldest = *self
                .surfaces
                .iter()
                .min_by_key(|(_, surface)| surface.used)
                .expect("not empty")
                .0;
            self.surfaces.remove(&oldest);
        }
    }
}

impl FrameImporter for Dx12Importer {
    type Frame = Frame;

    fn import(&mut self, frame: &Frame) -> Result<GpuFrame> {
        self.check_adapter(frame)?;
        let texture = self.texture(frame, true)?;
        self.wait(frame)?;
        Ok(GpuFrame {
            width: frame.width(),
            height: frame.height(),
            pixels: GpuPixels::Rgb(texture),
        })
    }

    fn import_cached(&mut self, frame: &Frame) -> Result<Option<GpuFrame>> {
        self.check_adapter(frame)?;
        let key = frame.texture().as_raw() as usize;
        let now = Instant::now();
        if !self.surfaces.contains_key(&key) {
            let texture = self.texture(frame, false)?;
            self.forget_old(now);
            self.surfaces.insert(
                key,
                Surface {
                    _source: frame.texture().clone(),
                    texture,
                    used: now,
                },
            );
        }
        self.wait(frame)?;
        let surface = self.surfaces.get_mut(&key).expect("inserted above");
        surface.used = now;
        Ok(Some(GpuFrame {
            width: frame.width(),
            height: frame.height(),
            pixels: GpuPixels::Rgb(surface.texture.clone()),
        }))
    }

    fn target(&mut self, _frame: &Frame) -> Result<wgpu::Texture> {
        bail!(
            "captured Windows frames are read-only; compositor output pools are not implemented yet"
        )
    }

    fn ycbcr_target(&mut self, _frame: &Frame) -> Result<(wgpu::Texture, wgpu::Texture)> {
        bail!("Windows Y'CbCr output pools are not implemented yet")
    }
}
