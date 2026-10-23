//! Showing pool frames to the GPU with no engine changes: each plane
//! of a frame becomes a Vulkan texture that the compositor samples in
//! one pass, or renders into (the program's Y'CbCr conversion). The
//! textures are kept on the frame's pool slot (pools recycle a few),
//! so a recycled frame keeps its textures, and only what changed
//! between the CPU bytes and the GPU texels is copied, through
//! row-aligned staging buffers (the DMA-BUF zero-copy import lands
//! with L.2/L.5 of docs/port-omarchy.md; this module's contract stays
//! exactly as it is).
//!
//! A wgpu texture means nothing to another device, and the engine gives
//! every viewer its own (`Gpu::new`), so a viewer showing frames the
//! live program's compositor drew keeps copies of its own, filled from
//! the pixels — the picture exists in the drawing device's texels, so it
//! is read back first. That is one more copy a frame; DMA-BUF is what
//! would remove it (L.4.6 of docs/port-omarchy.md).

use std::sync::mpsc;
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_engine::gpu::{FrameImporter, Gpu, GpuFrame, GpuPixels, Matrix, YCbCr};

use crate::frames::{FrameKind, Imported, LinuxFrame, Shared};

/// Rows a texture copy's buffer needs: 256-byte alignment.
const ROW_ALIGN: u64 = 256;

/// The importer the compositor, snapshots and shared-frame viewers
/// hold. One thread uses it at a time, like macOS's `MetalImporter`.
pub struct VulkanImporter {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Identifies this importer's device ([`Gpu::id`]): `wgpu::Device`
    /// compares by an id that starts over every instance, so two devices
    /// in one process can compare equal, and reusing the wrong textures
    /// ends in "Cannot get non-existent resource". Every importer on the
    /// same device has the same token, so they share a slot's textures:
    /// the engine opens one device per process, and a per-importer token
    /// made every viewer copy every frame it showed into textures of its
    /// own (read back, then uploaded), up to a pool's 32 slots each, kept
    /// as long as the pool.
    token: u64,
    /// Copies of slots another device drew (`Foreign`), kept so a viewer
    /// showing the program doesn't remake its textures every frame.
    foreign: Vec<Foreign>,
}

/// A pool slot's textures on *this* device, for a slot whose own
/// textures belong to another one.
struct Foreign {
    /// The slot, weakly: a recycled slot must not be answered with the
    /// dead one's textures.
    slot: Weak<Shared>,
    size: (u32, u32),
    kind: FrameKind,
    /// The CPU version these textures hold (`None`: never filled).
    synced: Option<u64>,
    textures: Vec<wgpu::Texture>,
}

impl Foreign {
    fn is(&self, slot: &Arc<Shared>) -> bool {
        self.slot
            .upgrade()
            .is_some_and(|ours| Arc::ptr_eq(&ours, slot))
    }
}

unsafe impl Send for VulkanImporter {}

fn extent(width: u32, height: u32) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    }
}

impl VulkanImporter {
    pub fn new(gpu: &Gpu) -> Result<Self> {
        Ok(Self {
            device: gpu.device.clone(),
            queue: gpu.queue.clone(),
            token: gpu.id,
            foreign: Vec::new(),
        })
    }

    /// The textures for `frame`'s planes, created in the pool slot the
    /// first time (kept since). `created_for_render` says the engine
    /// will paint every pixel (a render target), else the CPU bytes
    /// must go in before the GPU can sample.
    fn textures_for(
        &mut self,
        frame: &LinuxFrame,
        created_for_render: bool,
    ) -> Result<Vec<wgpu::Texture>> {
        if let Some(imported) = frame.imported() {
            if imported.maker == self.token {
                return Ok(imported.textures);
            } // Another GPU made this slot's textures, and wgpu textures do
            // not cross devices: this one samples the frame through
            // textures of its own, filled from the pixels
            // (`textures_of_our_own`). A render target can only be its
            // maker's; the engine's every program canvas is.
            if created_for_render {
                bail!(
                    "a frame whose textures another GPU made cannot be painted on this one: \
                     draw targets' textures belong to the device that made them"
                );
            }
            return self.textures_of_our_own(frame);
        }
        let textures = make_textures(&self.device, frame);
        frame.set_imported(Imported {
            textures: textures.clone(),
            device: self.device.clone(),
            queue: self.queue.clone(),
            maker: self.token,
        });
        // A render target is painted before it is sampled; anything
        // else gets the CPU bytes on its first import.
        frame.set_gpu_synced(if created_for_render {
            frame.cpu_version()
        } else {
            u64::MAX
        });
        Ok(textures)
    }

    /// The frame's planes on this device, for a frame whose own textures
    /// belong to another one: read back out of the device that has the
    /// picture, then uploaded here. Only after a device is lost and a
    /// new one opened do two devices see the same slots.
    fn textures_of_our_own(&mut self, frame: &LinuxFrame) -> Result<Vec<wgpu::Texture>> {
        // The drawing device's newest texels into the pool's CPU bytes
        // (a writer's read-back of the same version is reused).
        read_back(frame)?;
        let cpu = frame.cpu_version();
        let (width, height) = frame.size();
        let wrong_shape =
            |foreign: &Foreign| foreign.size != (width, height) || foreign.kind != frame.kind();
        if let Some(index) = self
            .foreign
            .iter()
            .position(|foreign| foreign.is(frame.slot()) && wrong_shape(foreign))
        {
            self.foreign.remove(index);
        }
        if !self.foreign.iter().any(|foreign| foreign.is(frame.slot())) {
            // Slots are recycled a few at a time: a dead slot's copies
            // are no use again.
            self.foreign
                .retain(|foreign| foreign.slot.strong_count() > 0);
            self.foreign.push(Foreign {
                slot: Arc::downgrade(frame.slot()),
                size: (width, height),
                kind: frame.kind(),
                synced: None,
                textures: make_textures(&self.device, frame),
            });
        }
        let foreign = self
            .foreign
            .iter_mut()
            .find(|foreign| foreign.is(frame.slot()))
            .expect("just made");
        if foreign.synced != Some(cpu) {
            upload_planes(&self.device, &self.queue, frame, &foreign.textures)?;
            foreign.synced = Some(cpu);
        }
        Ok(foreign.textures.clone())
    }

    /// The CPU bytes into every plane's textures, remembering that they
    /// hold this version (a frame is known whole-heart): skips planes
    /// already transferred.
    fn sync(&self, frame: &LinuxFrame, textures: &[wgpu::Texture]) -> Result<()> {
        let cpu = frame.cpu_version();
        if frame.gpu_synced() == cpu {
            return Ok(());
        }
        upload_planes(&self.device, &self.queue, frame, textures)?;
        frame.set_gpu_synced(cpu);
        Ok(())
    }
}

/// One texture per plane of `frame`, on `device`: what the compositor
/// samples, and what it paints into (everything the engine may ask of
/// them).
fn make_textures(device: &wgpu::Device, frame: &LinuxFrame) -> Vec<wgpu::Texture> {
    let (width, height) = frame.size();
    let usage = wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_SRC
        | wgpu::TextureUsages::COPY_DST
        | wgpu::TextureUsages::RENDER_ATTACHMENT;
    let texture = |label: &str, size: (u32, u32), format: wgpu::TextureFormat| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: extent(size.0, size.1),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    match frame.kind() {
        FrameKind::Bgra => vec![texture(
            "isocast frame BGRA",
            (width, height),
            wgpu::TextureFormat::Bgra8Unorm,
        )],
        FrameKind::Ycbcr8 => vec![
            texture(
                "isocast frame luma",
                (width, height),
                wgpu::TextureFormat::R8Unorm,
            ),
            texture(
                "isocast frame chroma",
                (width / 2, height / 2),
                wgpu::TextureFormat::Rg8Unorm,
            ),
        ],
    }
}

/// The CPU bytes into every plane's textures of a frame whose own
/// textures belong to another device: the same upload [`VulkanImporter::sync`]
/// does, without claiming the slot's `gpu_synced` for this device.
fn upload_planes(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    frame: &LinuxFrame,
    textures: &[wgpu::Texture],
) -> Result<()> {
    let (width, height) = frame.size();
    let plane_count = frame.plane_count();
    for (index, texture) in textures.iter().enumerate().take(plane_count) {
        let (bytes, stride) = frame.plane(index)?;
        let size = match (frame.kind(), index) {
            (FrameKind::Ycbcr8, 1) => extent(width / 2, height / 2),
            _ => extent(width, height),
        };
        upload_plane(device, queue, texture, bytes, stride, size)?;
    }
    Ok(())
}

/// The CPU bytes into a plane's texture, over row-aligned staging
/// (buffer rows take the 256 alignment; the row gaps are mapped
/// scratch and never read as pixels).
fn upload_plane(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    bytes: Vec<u8>,
    stride: usize,
    size: wgpu::Extent3d,
) -> Result<()> {
    let row_bytes = stride as u64;
    let aligned = row_bytes.div_ceil(ROW_ALIGN) * ROW_ALIGN;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("isocast frame upload"),
        size: aligned * size.height as u64,
        usage: wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: true,
    });
    {
        let mut mapped = buffer
            .slice(..)
            .get_mapped_range_mut()
            .context("the GPU refused the upload map")?;
        for row in 0..size.height as usize {
            let (from, into_at) = (row * stride, row as u64 * aligned);
            let Some(source) = bytes.get(from..from + stride) else {
                bail!("the pool's plane is shorter than its size promised");
            };
            let into_at = into_at as usize;
            // A write-only view: mapped memory may be write-combining
            // and not take ordinary reads, so wgpu hands out a slice
            // that only copies in.
            mapped
                .slice(into_at..into_at + stride)
                .copy_from_slice(source);
        }
    }
    buffer.unmap();
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_buffer_to_texture(
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(u32::try_from(aligned).unwrap_or(u32::MAX)),
                rows_per_image: None,
            },
        },
        texture.as_image_copy(),
        size,
    );
    queue.submit([encoder.finish()]);
    Ok(())
}

/// A texture's texels out into host memory, the rows still padded to
/// [`ROW_ALIGN`], with the stride they come back at. A copy straight
/// into row-padded shared memory takes them as they are; a copy into
/// tightly packed pixels de-pads them ([`read_plane`]).
pub(crate) fn copy_out(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    row_bytes: u64,
    size: wgpu::Extent3d,
) -> Result<(Vec<u8>, u64)> {
    let aligned = row_bytes.div_ceil(ROW_ALIGN) * ROW_ALIGN;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("isocast frame read-back"),
        size: aligned * size.height as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(u32::try_from(aligned).unwrap_or(u32::MAX)),
                rows_per_image: None,
            },
        },
        size,
    );
    let submission = queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(Duration::from_secs(5)),
        })
        .context("the GPU did not finish the read-back copy")?;
    rx.recv()
        .context("the GPU did not return the read-back")?
        .context("the GPU refused the read-back map")?;
    let mapped = slice
        .get_mapped_range()
        .context("the GPU refused the read-back map")?;
    let mut out = vec![0u8; (aligned * size.height as u64) as usize];
    if let Some(all) = mapped.get(..out.len()) {
        out.copy_from_slice(all);
    } else {
        bail!("the GPU's read-back buffer is shorter than the texture");
    }
    drop(mapped);
    buffer.unmap();
    Ok((out, aligned))
}

/// A texture's bytes out, dropping the row-alignment gap. Blocks until
/// the GPU has finished (five seconds at most).
fn read_plane(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    stride: usize,
    size: wgpu::Extent3d,
) -> Result<Vec<u8>> {
    let (padded, aligned) = copy_out(device, queue, texture, stride as u64, size)?;
    let mut out = vec![0u8; stride * size.height as usize];
    for row in 0..size.height as usize {
        let from = (row as u64 * aligned) as usize;
        out[row * stride..row * stride + stride].copy_from_slice(&padded[from..from + stride]);
    }
    Ok(out)
}

/// A frame's newest texels, from its textures, into the pool's CPU
/// bytes — what the writer and the JPEG encoder call before reading
/// plane bytes. The device and queue are the frame's own: the engine
/// makes a new GPU per program run, snapshot and shared-frame viewer,
/// so a frame's textures only mean anything to the instance that made
/// them (using "the process's" GPU instead read a live texture out of a
/// dead registry and panicked).
pub(crate) fn read_back(frame: &LinuxFrame) -> Result<bool> {
    let version = frame.render_version();
    if version <= frame.written_back() {
        return Ok(false);
    }
    let Some(imported) = frame.imported() else {
        bail!("a frame with GPU pixels must have textures");
    };
    let Imported {
        textures,
        device,
        queue,
        maker: _,
    } = imported;
    let (width, height) = frame.size();
    for (index, texture) in textures.iter().enumerate() {
        let (size, stride) = match frame.kind() {
            FrameKind::Bgra => (extent(width, height), width as usize * 4),
            FrameKind::Ycbcr8 => match index {
                0 => (extent(width, height), width as usize),
                _ => (extent(width / 2, height / 2), width as usize),
            },
        };
        let bytes = read_plane(&device, &queue, texture, stride, size)?;
        frame.set_plane(index, bytes);
    }
    frame.mark_written_back(version);
    Ok(true)
}

impl FrameImporter for VulkanImporter {
    type Frame = LinuxFrame;

    /// `frame` shown to the GPU: textures are made (their bytes
    /// synced with the CPU's) when they don't match the CPU version.
    fn import(&mut self, frame: &Self::Frame) -> Result<GpuFrame> {
        let textures = self.textures_for(frame, false)?;
        if frame.gpu_synced() != frame.cpu_version() {
            self.sync(frame, &textures)?;
        }
        Ok(Self::build_gpu_frame(frame, &textures))
    }

    /// An 8-bit BGRA frame for the GPU to draw into. The compositor
    /// paints every pixel, so nothing is copied in; the writer reads
    /// these texels back out.
    fn target(&mut self, frame: &Self::Frame) -> Result<wgpu::Texture> {
        let textures = self.textures_for(frame, true)?;
        if textures.len() != 1 {
            bail!("the GPU render target needs a one-plane BGRA frame, not Y'CbCr");
        }
        frame.bump_render_version();
        Ok(textures[0].clone())
    }

    /// The two textures of an 8-bit Y'CbCr frame the GPU draws into.
    fn ycbcr_target(&mut self, frame: &Self::Frame) -> Result<(wgpu::Texture, wgpu::Texture)> {
        let textures = self.textures_for(frame, true)?;
        if let [luma, chroma] = textures.as_slice() {
            frame.bump_render_version();
            return Ok((luma.clone(), chroma.clone()));
        }
        bail!("ycbcr_target needs a two-plane Y'CbCr frame, not BGRA");
    }

    /// Linux's textures come from a pool slot they are kept in
    /// already, so `import_cached` is [`Self::import`] exactly.
    fn import_cached(&mut self, frame: &Self::Frame) -> Result<Option<GpuFrame>> {
        Ok(Some(self.import(frame)?))
    }

    /// The same as [`Self::ycbcr_target`].
    fn ycbcr_target_cached(
        &mut self,
        frame: &Self::Frame,
    ) -> Result<Option<(wgpu::Texture, wgpu::Texture)>> {
        Ok(Some(self.ycbcr_target(frame)?))
    }
}

impl VulkanImporter {
    fn build_gpu_frame(frame: &LinuxFrame, textures: &[wgpu::Texture]) -> GpuFrame {
        let (width, height) = frame.size();
        match frame.kind() {
            FrameKind::Bgra => GpuFrame {
                width,
                height,
                pixels: GpuPixels::Rgb(textures[0].clone()),
            },
            FrameKind::Ycbcr8 => GpuFrame {
                width,
                height,
                pixels: GpuPixels::YCbCr {
                    luma: textures[0].clone(),
                    chroma: textures[1].clone(),
                    coding: YCbCr {
                        matrix: Matrix::Bt709,
                        full_range: false,
                        bits: 8,
                    },
                },
            },
        }
    }
}
