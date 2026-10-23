//! Heap frame pools: BGRA for sources and CPU drawing, 8-bit 4:2:0
//! Y'CbCr (video range, Rec. 709) for the program's GPU conversion. A
//! frame is cheap to clone: clones share the same pixels, the way a
//! retained `CVPixelBuffer` does on macOS.
//!
//! The GPU touches frames through [`crate::VulkanImporter`], which
//! keeps textures for a pool slot on the slot itself (pools recycle a
//! few slots, like macOS's surface cache) and syncs only what changed:
//! CPU bytes into a texture before the compositor samples it, texture
//! bytes out to the CPU bytes before the writer reads them. The
//! zero-copy DMA-BUF design of docs/port-omarchy.md §3.8 removes those
//! copies; until then the couple of GB/s they cost is what macOS's
//! numbers are compared against.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Result, bail};
use isocast_engine::FramePool;

/// Frames one pool may have out at once. If writers stall, the source
/// drops frames instead of growing the pool without bound.
const MAX_FRAMES_IN_FLIGHT: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameKind {
    /// One BGRA plane.
    Bgra,
    /// Two planes: full-size 8-bit luma, then half-size Cb and Cr
    /// interleaved (NV12's layout), video range, Rec. 709.
    Ycbcr8,
}

/// One pool's slot: pixels and their GPU twins. The pool holds slots
/// weakly, so a slot is "out" exactly while a frame holds its Arc.
pub struct Shared {
    width: u32,
    height: u32,
    pub kind: FrameKind,
    planes: Mutex<Vec<Plane>>,
    /// Number of CPU draws since the slot was created.
    cpu_version: AtomicU64,
    /// The CPU version the slot's textures were last written from.
    gpu_synced: AtomicU64,
    /// How many times a GPU render wrote into this slot's textures; a
    /// writer compares its read-backs against it.
    render_version: AtomicU64,
    written_back: AtomicU64,
    /// One texture per plane, with the GPU instance that made them.
    textures: Mutex<Option<Imported>>,
}

/// A slot's GPU twins and the instance they live in. The engine makes a
/// new GPU (a new wgpu device) for each program run, snapshot and shared
/// frame viewer, so a frame must remember which one its textures belong
/// to: reading a frame back on another device looks its texture up in
/// the wrong registry and panics ("Cannot get non-existent resource").
#[derive(Clone)]
pub(crate) struct Imported {
    pub textures: Vec<wgpu::Texture>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// The importer instance that made them. Never compare the devices
    /// (`wgpu::Device` compares by id, and ids start over every
    /// instance, so two devices in a process all look like `DeviceId(0,1)`
    /// and compare equal — docs/notes.md); only the maker gets to reuse.
    pub maker: u64,
}

struct Plane {
    bytes: Vec<u8>,
    stride: usize,
}

/// A frame from a pool. Dropping the last clone returns the pixels to
/// the pool.
pub struct LinuxFrame {
    slot: Arc<Shared>,
}

impl Clone for LinuxFrame {
    fn clone(&self) -> Self {
        Self {
            slot: self.slot.clone(),
        }
    }
}

// The Arc carries the pixels; slot bookkeeping happens under its own
// locks and atomics only.
unsafe impl Send for LinuxFrame {}
unsafe impl Sync for LinuxFrame {}

impl Shared {
    /// The version of the CPU bytes.
    pub(crate) fn cpu_version(&self) -> u64 {
        self.cpu_version.load(Ordering::Acquire)
    }

    /// Renders into a slot's textures bump this.
    pub(crate) fn bump_render_version(&self) -> u64 {
        self.render_version.fetch_add(1, Ordering::Release) + 1
    }

    pub(crate) fn render_version(&self) -> u64 {
        self.render_version.load(Ordering::Acquire)
    }

    pub(crate) fn written_back(&self) -> u64 {
        self.written_back.load(Ordering::Acquire)
    }

    pub(crate) fn mark_written_back(&self, version: u64) {
        self.written_back.fetch_max(version, Ordering::Release);
    }

    /// The CPU version the textures hold (`u64::MAX` when unset).
    pub(crate) fn gpu_synced(&self) -> u64 {
        self.gpu_synced.load(Ordering::Acquire)
    }

    pub(crate) fn set_gpu_synced(&self, version: u64) {
        self.gpu_synced.store(version, Ordering::Release);
    }

    /// Fill `plane` from the CPU: a pool draw.
    fn fill_plane(&self, debug_plane: usize, fill: &mut dyn FnMut(&mut [u8], usize)) {
        let mut planes = self.planes.lock().expect("frame planes poisoned");
        let Plane { bytes, stride } = &mut planes[debug_plane];
        let stride = *stride;
        fill(bytes, stride);
    }

    /// Replace `plane`'s bytes with `bytes` (a GPU read-back result).
    pub(crate) fn set_plane(&self, plane: usize, bytes: Vec<u8>) {
        let mut planes = self.planes.lock().expect("frame planes poisoned");
        let Some(target) = planes.get_mut(plane) else {
            return;
        };
        target.bytes.clear();
        target.bytes.extend_from_slice(&bytes);
    }

    /// The slot's textures and their GPU instance, once the importer
    /// made them.
    pub(crate) fn imported(&self) -> Option<Imported> {
        let imported = self.textures.lock().ok()?;
        imported.clone()
    }

    pub(crate) fn set_imported(&self, imported: Imported) {
        *self.textures.lock().expect("frame textures poisoned") = Some(imported);
    }
}

impl LinuxFrame {
    /// The frame's width and height in pixels.
    pub(crate) fn size(&self) -> (u32, u32) {
        (self.slot.width, self.slot.height)
    }

    /// Whether the frame holds two-plane 4:2:0 Y'CbCr.
    pub(crate) fn is_ycbcr(&self) -> bool {
        self.slot.kind == FrameKind::Ycbcr8
    }

    /// The pixel bytes and stride of `plane`, for encoders and JPEG.
    pub fn plane(&self, index: usize) -> Result<(Vec<u8>, usize)> {
        let planes = self.slot.planes.lock().expect("frame planes poisoned");
        let Some(Plane { bytes, stride }) = planes.get(index) else {
            bail!("plane {index} doesn't exist on this frame");
        };
        Ok((bytes.clone(), *stride))
    }

    pub(crate) fn cpu_version(&self) -> u64 {
        self.slot.cpu_version()
    }

    /// The pool slot this frame is, for telling slots apart: a pool
    /// recycles a few, and two clones are the same slot.
    pub(crate) fn slot(&self) -> &Arc<Shared> {
        &self.slot
    }

    pub(crate) fn render_version(&self) -> u64 {
        self.slot.render_version()
    }

    pub(crate) fn written_back(&self) -> u64 {
        self.slot.written_back()
    }

    /// The GPU read the texels from the CPU version `version`.
    pub(crate) fn mark_written_back(&self, version: u64) {
        self.slot.mark_written_back(version)
    }

    pub(crate) fn gpu_synced(&self) -> u64 {
        self.slot.gpu_synced()
    }

    /// The slot's textures and their GPU instance.
    pub(crate) fn imported(&self) -> Option<Imported> {
        self.slot.imported()
    }

    /// Which pixel layout the frame holds.
    pub(crate) fn kind(&self) -> FrameKind {
        self.slot.kind
    }

    /// Merge a read-back result into the pool's CPU bytes.
    pub(crate) fn set_plane(&self, plane: usize, bytes: Vec<u8>) {
        self.slot.set_plane(plane, bytes)
    }

    /// Store the importer's textures on the slot, with the instance that
    /// made them.
    pub(crate) fn set_imported(&self, imported: Imported) {
        self.slot.set_imported(imported)
    }

    /// Store the CPU version the textures were last written from.
    pub(crate) fn set_gpu_synced(&self, version: u64) {
        self.slot.set_gpu_synced(version)
    }

    /// Render-target use bumps this, so writers know the texels hold
    /// newer pixels than the CPU bytes.
    pub(crate) fn bump_render_version(&self) {
        self.slot.bump_render_version();
    }

    /// One texture per plane: BGRA 1, Y'CbCr 2.
    pub(crate) fn plane_count(&self) -> usize {
        match self.slot.kind {
            FrameKind::Bgra => 1,
            FrameKind::Ycbcr8 => 2,
        }
    }
}

/// A frame pool. `draw` returns CPU-filled frames; `take` gives a
/// frame the compositor renders into.
pub struct CpuPool {
    width: u32,
    height: u32,
    kind: FrameKind,
    ledger: Arc<Ledger>,
}

/// One pool's slots and its counters, shared with the registry the
/// doctor's frame-budget check reads (held weakly there: a pool that is
/// dropped leaves the ledger).
struct Ledger {
    slots: Mutex<Vec<Arc<Shared>>>,
    /// The most slots the pool has ever had out at once.
    peak_in_use: AtomicU64,
    /// Draws and takes refused because every slot was out.
    refused: AtomicU64,
}

/// Every live pool, for `frame_budget`.
static POOLS: Mutex<Vec<Weak<Ledger>>> = Mutex::new(Vec::new());

/// The distinct-buffer ledger over every live pool, for the doctor and
/// `status.diagnostics.frame_budget`. Linux pools have one limit
/// ([`MAX_FRAMES_IN_FLIGHT`] slots each) and no per-output reserves, so
/// that is all this reports: slots (distinct buffers), those out right
/// now, the most ever out, and the refusals since the daemon started
/// (a refusal is a source frame dropped because writers still held every
/// slot).
pub(crate) fn diagnostics() -> serde_json::Value {
    let mut pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
    pools.retain(|pool| pool.strong_count() > 0);
    let (mut distinct, mut in_use, mut peak, mut refused, mut within) = (0, 0, 0, 0, true);
    let mut count = 0;
    for ledger in pools.iter().filter_map(Weak::upgrade) {
        let slots = ledger.slots.lock().unwrap_or_else(|e| e.into_inner());
        distinct += slots.len();
        in_use += slots
            .iter()
            .filter(|slot| Arc::strong_count(slot) > 1)
            .count();
        within &= slots.len() <= MAX_FRAMES_IN_FLIGHT;
        peak = peak.max(ledger.peak_in_use.load(Ordering::Relaxed));
        refused += ledger.refused.load(Ordering::Relaxed);
        count += 1;
    }
    serde_json::json!({
        "invariant": within,
        "pools": count,
        "distinct_buffers": distinct,
        "in_use": in_use,
        "peak_in_use": peak,
        "physical_limit": MAX_FRAMES_IN_FLIGHT,
        "refused": [refused],
    })
}

impl CpuPool {
    /// BGRA frames.
    pub fn bgra(width: u32, height: u32) -> Self {
        Self::with_kind(width, height, FrameKind::Bgra)
    }

    /// 8-bit 4:2:0 Y'CbCr frames in video range, tagged Rec. 709: what
    /// the encoders take without further conversion, at under half
    /// BGRA's size.
    pub fn ycbcr(width: u32, height: u32) -> Self {
        Self::with_kind(width, height, FrameKind::Ycbcr8)
    }

    fn with_kind(width: u32, height: u32, kind: FrameKind) -> Self {
        let ledger = Arc::new(Ledger {
            slots: Mutex::new(Vec::new()),
            peak_in_use: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        });
        let mut pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
        pools.retain(|pool| pool.strong_count() > 0);
        pools.push(Arc::downgrade(&ledger));
        Self {
            width,
            height,
            kind,
            ledger,
        }
    }

    fn initial_planes(&self) -> Vec<Plane> {
        let (width, height) = (self.width as usize, self.height as usize);
        match self.kind {
            FrameKind::Bgra => vec![Plane {
                bytes: vec![0; width * height * 4],
                stride: width * 4,
            }],
            FrameKind::Ycbcr8 => vec![
                Plane {
                    bytes: vec![0; width * height],
                    stride: width,
                },
                // Half size, Cb and Cr interleaved.
                Plane {
                    bytes: vec![0; width * height / 2],
                    stride: width,
                },
            ],
        }
    }
}

impl FramePool for CpuPool {
    type Frame = LinuxFrame;

    /// Take a frame from the pool and let `draw` fill its first plane
    /// (BGRA for BGRA pools; the engine only CPU-draws one-plane
    /// pools), handed as pixel bytes and bytes per row. `None` if
    /// every frame is checked out.
    fn draw(&mut self, draw: &mut dyn FnMut(&mut [u8], usize)) -> Result<Option<LinuxFrame>> {
        let Some(slot) = self.take_slot() else {
            return Ok(None);
        };
        slot.fill_plane(0, draw);
        slot.cpu_version.fetch_add(1, Ordering::Release);
        // The GPU's texels (if any) now disagree with the CPU's; the
        // importer re-syncs them the next time it samples, so dropping
        // them is not required — but a recycled slot's old textures
        // still hold the last draw's texels, and a fresh render into
        // them must not depend on those bytes anyway, so they stay.
        Ok(Some(LinuxFrame { slot }))
    }

    /// A frame for the GPU to draw into, with undefined pixels.
    fn take(&mut self) -> Result<Option<LinuxFrame>> {
        let Some(slot) = self.take_slot() else {
            return Ok(None);
        };
        // The compositor renders into this slot's textures immediately
        // after; flag that they'll hold pixels the pool hasn't copied
        // out, so the writer read-backs instead of trusting the CPU
        // bytes. Monotonic in case a reused slot repeats.
        slot.render_version.fetch_max(
            slot.cpu_version.load(Ordering::Acquire) + 1,
            Ordering::Release,
        );
        Ok(Some(LinuxFrame { slot }))
    }
}

impl CpuPool {
    /// A slot the pool doesn't hold: a recycled one if any, else (up
    /// to the in-flight cap) a brand-new one. `None` if every frame is
    /// out.
    fn take_slot(&mut self) -> Option<Arc<Shared>> {
        let ledger = &self.ledger;
        let mut slots = ledger.slots.lock().expect("pool slots poisoned");
        let slot = if let Some(slot) = slots.iter().find(|slot| Arc::strong_count(slot) == 1) {
            slot.clone()
        } else if slots.len() >= MAX_FRAMES_IN_FLIGHT {
            ledger.refused.fetch_add(1, Ordering::Relaxed);
            return None;
        } else {
            let slot = Arc::new(Shared {
                width: self.width,
                height: self.height,
                kind: self.kind,
                planes: Mutex::new(self.initial_planes()),
                cpu_version: AtomicU64::new(0),
                render_version: AtomicU64::new(0),
                written_back: AtomicU64::new(0),
                gpu_synced: AtomicU64::new(u64::MAX),
                textures: Mutex::new(None),
            });
            slots.push(slot.clone());
            slot
        };
        // The slot just taken counts as out (its clone is returned).
        let out = slots
            .iter()
            .filter(|slot| Arc::strong_count(slot) > 1)
            .count() as u64;
        ledger.peak_in_use.fetch_max(out, Ordering::Relaxed);
        Some(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::{CpuPool, diagnostics};
    use isocast_engine::FramePool;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    #[test]
    fn bgra_draw_and_recycles_slot() {
        let mut pool = CpuPool::bgra(64, 32);
        let mut fill = |bytes: &mut [u8], _stride: usize| bytes.fill(7);
        let first = pool.draw(&mut fill).unwrap().unwrap();
        assert_eq!(first.plane(0).unwrap().0.len(), 8192);
        assert_eq!(first.plane(0).unwrap().1, 256);
        let first_slot = Arc::downgrade(&first.slot);
        let second = pool.draw(&mut fill).unwrap().unwrap();
        assert!(!std::sync::Arc::ptr_eq(&first.slot, &second.slot));
        drop(first);
        drop(second);
        let recycled = pool.draw(&mut fill).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first_slot.upgrade().unwrap(), &recycled.slot));
        assert!(recycled.plane(0).unwrap().0.iter().all(|byte| *byte == 7));
    }

    #[test]
    fn ycbcr_take_has_expected_planes_and_versions() {
        let mut pool = CpuPool::ycbcr(64, 32);
        let frame = pool.take().unwrap().unwrap();
        assert!(frame.render_version() > frame.written_back());
        assert!(frame.is_ycbcr());
        assert_eq!(frame.plane(1).unwrap().0.len(), 1024);
    }

    #[test]
    fn the_ledger_counts_slots_peak_and_refusals() {
        // Other tests make pools too: look at this pool's own ledger.
        let mut pool = CpuPool::bgra(16, 16);
        let mut fill = |bytes: &mut [u8], _stride: usize| bytes.fill(1);
        let held: Vec<_> = (0..super::MAX_FRAMES_IN_FLIGHT)
            .map(|_| pool.draw(&mut fill).unwrap().unwrap())
            .collect();
        assert!(pool.draw(&mut fill).unwrap().is_none());
        assert!(pool.take().unwrap().is_none());
        assert_eq!(pool.ledger.slots.lock().unwrap().len(), 32);
        assert_eq!(pool.ledger.refused.load(Ordering::Relaxed), 2);
        assert_eq!(pool.ledger.peak_in_use.load(Ordering::Relaxed), 32);
        let ledger = diagnostics();
        assert_eq!(ledger["physical_limit"], 32);
        assert_eq!(ledger["invariant"], true);
        assert!(ledger["in_use"].as_u64().unwrap() >= 32);
        assert!(ledger["refused"][0].as_u64().unwrap() >= 2);
        drop(held);
        // Frames released: slots stay (distinct buffers), none is out,
        // and the next draw recycles one rather than refusing.
        assert!(pool.draw(&mut fill).unwrap().is_some());
        assert_eq!(pool.ledger.slots.lock().unwrap().len(), 32);
        assert_eq!(pool.ledger.refused.load(Ordering::Relaxed), 2);
    }
}
