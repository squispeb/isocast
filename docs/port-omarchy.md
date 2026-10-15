# Porting Isocast to Linux (Omarchy)

> **Review findings (2026-10-14, claude-opus-5-5 high):** the
> architecture claims hold, but the review found specifics in this
> document to correct; the code and design already follow the
> corrections below, and the doc gets its final pass as they land.
>
> - §1: **ten** associated types, not nine. **Three** backend seams,
>   not one (`Platform`, `GlobalHotkeys` in engine/hotkeys.rs,
>   `CameraSink`); the daemon also calls `isocast_platform_macos`
>   directly in ~21 more places, and `doctor` had no `cfg` — verify
>   `cargo check -p isocast-daemon` fails at main.rs:3930 on Linux.
>   The macOS crate is ~10.9k lines across 22 modules, not 19k.
> - Mapping fixes: `writer.rs`'s "fsync like AVAssetWriter" is wrong
>   (accepting a sample is not a durable commit; that gap is the
>   still-open G.3b, and the 55-kill matrix is its *upcoming* gate);
>   `stream.rs` is only the encoders (RTMP/FLV live in the engine) and
>   an ffmpeg-CLI live encoder cannot do per-frame forced keyframes or
>   mid-session `set_rate`, so streams/monitors need in-process
>   encoders (LGPL libav or the NVENC API); `join.rs` is
>   AVAssetReader+Writer, not a composition, and the concat demuxer
>   can't do the per-part hold/pad/cut; `system.rs` is `machine()` and
>   `probe()`, not powermetrics; `plugin.rs` is the Seatbelt launcher
>   (Linux plugins run unsandboxed today; SO_PEERCRED + Landlock is
>   the answer); microphones go through AVFoundation, not Core Audio;
>   `hotkeys.rs` implements `GlobalHotkeys`, not `Platform`.
> - **Per-app audio has a Linux equivalent** after all: PipeWire can
>   link a capture stream to an application's output node, which
>   implements `capture_application_audio` and also fixes
>   `exclude_own`. §7's "no equivalent" claim is withdrawn.
> - Licensing: link only an **LGPL** libav build, never x264/x265/fdk
>   (GPL); a user-installed ffmpeg run as a separate process is fine.
> - `av1_nvenc` in `ffmpeg -encoders` only means FFmpeg was built with
>   it; GA104 (Ampere) has **no** AV1 encoder, so this GPU can't run it.
> - Frame-design premise changed: `FramePool::draw` hands out
>   CPU-writable pixels *and* `FrameImporter::target` must make the
>   same frame a GPU render target, so L.0's frame design is heap
>   pools with importer-owned textures per pool slot and row-aligned
>   staged copies; DMA-BUF waits for L.2/L.5, and `Gpu::new` will need
>   a device-creation hook when external memory arrives.
> - L.0's prerequisites (corrected): the Vulkan feature split, the
>   Frame/importer pair, the daemon rewiring (including `doctor`),
>   every no-default trait method (~30), and `volume()` for the disk
>   checks. Secrets are not needed for L.0 (only streaming uses them).
> - §7 correction: Ryan's flagship includes an iPhone (Continuity
>   Camera), which is macOS-only for good — "nothing blocks the
>   flagship" was wrong.
> - The L.4 milestone gets a note: Linux shared surfaces don't exist
>   on the other-systems stub yet, and the engine has no post-render
>   sync hook, so either the engine gains one small hook or the first
>   version shares through memfd + client-side copies.

A review of what a Linux port needs, written against this repository's actual
code and this machine (Omarchy: Hyprland on Wayland, AMD Ryzen 7 5800X,
NVIDIA RTX 3060 Ti, PipeWire 1.6.8, Hyprland 0.56.2, FFmpeg n9.0.1 with
NVENC). The goal is not merely "it runs": it to work *great* here with the
same performance measurements S.1 and the engine already produce on macOS
(zero dropped frames at the flagship, ≤ ½ core of daemon CPU for the
flagship-1080, p99 media-clock wakes within budget, equal A/V durations, and
files that line up).

## 1. What the architecture already gives us

The port surface is far smaller than the feature list suggests:

- **One trait.** `isocast_engine::Platform`
  (`crates/isocast-engine/src/lib.rs:58`) is the *only* backend seam, with
  nine associated types: `Frame`, `FramePool`, `Writer`, `Capture`,
  `Importer`, `Media`, `Playback`, `VideoEncoder`, `AudioEncoder`,
  `SharedSurface`. Every OS-specific behaviour of the engine — pacing,
  joins, secrets, devices, permissions — is a method on it.
- **The engine has zero platform cfgs.** `cargo check -p
  isocast-protocol -p isocast-engine` already passes on this Linux machine
  (2026-10-14, Rust 1.99, ~49 s). The claim in PLAN.md — "macOS code lives
  in `isocast-platform-macos` behind traits" — is true in the code, not
  just in the docs.
- **The remaining macOS surface is one crate.** `isocast-platform-macos`
  is ~19k lines in 22 modules, each a small, independent mapping problem
  (§3).
- **A daemon shim.** `isocast-daemon/src/main.rs` names `MacPlatform` in
  about fifteen places; those become `cfg`-selected aliases (a
  `platform-linux` crate replacing it, or renaming the trait's impl per
  target). No engine work.
- **`isocast-surfaces` already has the stub.** `other.rs` bails for
  non-macOS; `docs/shared-frames.md` sketches the Windows design — the same
  ideas apply to Linux (§3.9).
- **The app is portable already.** `isocast-ui` is Slint + winit + Skia;
  its macOS-only parts (Core Animation previews, focused-key monitors) have
  working non-macOS fallbacks in `preview.rs` (`--preview image` via
  Slint, redrawn per frame). Previews just won't be as cheap as the CA
  layers at first.

What this means: a Linux port is *one new backend crate plus a daemon
rename-shim*, not a fork of the engine.

## 2. What changes on this machine (Omarchy specifics)

Facts measured here, 2026-10-14:

- **CPU.** Ryzen 7 5800X: 8 cores / 16 threads, strong per-core clocks.
  Roughly comparable per-core to the base M2's bigger P cores, but *no
  unified memory* and no PCIe-on-package free lunch: copies matter more.
- **GPU.** RTX 3060 Ti (Ampere GA104), driver 610.57.4. This is the
  decisive constraint, and mostly a *good* one:
  - Encoding: NVENC is on par with VideoToolbox for quality-per-bit at
    1080p and better than nothing at 4K. FFmpeg reports `h264_nvenc`,
    `hevc_nvenc` and even `av1_nvenc` available.
  - VAAPI: absent (this is an NVIDIA box; `vainfo` shows only the NVDEC
    decode backend). Any encoder-selection code must not assume VA-API.
- **Ram bandwidth.** ~50 GB/s dual-channel. A 4K60 BGRA pipe costs
  ~2.0 GB/s per stream; the flagship's three video outputs plus program
  put ~5-6 GB/s through the compositor's copies — fine on the CPU, but the
  zero-copy DMA-BUF path (§3.8) is what actually buys the macOS numbers.
- **Display server.** Hyprland 0.56.2 (Wayland), the Hyprland
  xdg-desktop-portal implementation is running, so `org.freedesktop.portal.ScreenCast`
  works today through PipeWire (this is the OBS-on-Linux path).
- **Audio.** PipeWire 1.6.8 with the PulseAudio compat layer (`pactl`
  works). Default mic (C920) and monitors (mix amp, HDMI) are visible as
  ordinary sources — unlike macOS there is *no system-wide tap API on
  Wayland*; system-audio capture means "monitor of the default sink", and
  *per-application* audio has no native equivalent (§3.3).
- **Cameras.** `/dev/video0` UVC works via V4L2; no v4l2loopback kernel
  module loaded (the virtual camera needs it; `dkms` install is a one-time
  step).
- **Toolchain.** Rust 1.99, clang, pkg-config present; no ffmpeg CLI
  dependency risk (`ffmpeg` n9.0.1 already installed) but the engine can't
  assume it (bundle or depend on libav via FFI; see §3.5).

Everything else — TOML setups, the protocol, MCP, CLI, plugins — is
already cross-platform (threads, Unix sockets, `native-tls` with OpenSSL).

## 3. Backend map, module by module

Each macOS module maps to a Linux counterpart inside a new
`isocast-platform-linux` crate with the same trait implementation. Sizes
are the macOS code they should roughly mirror.

| macOS module (lines) | What it does | Linux counterpart | Effort |
|---|---|---|---|
| `pacing.rs` (89) | kqueue `EVFILT_TIMER NOTE_ABSOLUTE` + leeway vs launchd | `clock_nanosleep(TIMER_ABSTIME)` on a raised-priority thread; Linux timer slack (~50-200 µs) is 1-2 orders of magnitude *better* than launchd's — this module is practically free | trivial |
| `frames.rs` (164) | IOSurface-backed `CVPixelBuffer` pools (BGRA, 8/10-bit 4:2:0) | pools of `dev/dri`-allocated GBM buffers (or memfd-backed) for compositor draws; DMA-BUF Fd = the frame | small |
| `gpu.rs` (565) | CVMetalTextureCache + `wgpu-hal::metal::Device::texture_from_raw` | `wgpu-hal::vulkan` external-memory import of the same DMA-BUFs (Ashpd: `ImageDmaBufAllocator`-style FDs imported into Vulkan via `VK_EXT_external_memory_dma_buf`); same `Surface{-kept, +TTL}` recycling | medium |
| `capture.rs` (356) | ScreenCaptureKit display/window/app modes | PipeWire screencast from the portal (`org.freedesktop.portal.ScreenCast` via `ashpd`-level D-Bus, or direct `pipewire-rs`); `source_types` gives monitors; windows through the same portal when the WM supports toplevel handles (Hyprland does for monitor now; per-window needs verification) | large |
| `camera.rs` (465) | AVFoundation formats/negotiation | V4L2 via `v4l2r` or the `v4l` crate; UVC negotiation is simpler; capture card = V4L2 | medium |
| `audio.rs` (487) | Core Audio mic tap + ScreenCaptureKit system audio | PulseAudio-compat capture (mic defaults), monitor-of-sink for system audio via `pipewire-rs` stream or `pactl`-level capture; per-app audio has no comfortable primitive (see §3.3) | medium |
| `writer.rs` (946) | AVAssetWriter fragmented MOV/MP4, colour tags, ProRes | **this is the hardest fidelity piece** — replace with our own fragmented-MP4 muxer (or `libav` bindings for it) fed by NVENC/x264 output; fragment every ~2 s and fsync like AVAssetWriter does on accept; colour tags via the CX/NAL入れ boxes; ProRes via `prores_ks` (software, slower than VideoToolbox) | large |
| `stream.rs` (849) | VT H.264 + AAC feeding our own RTMP/FLV | the RTMP/FLV half **is portable** (no OS API); swap the encoder to `h264_nvenc` in-process first (FFmpeg CLI on a pipe), AAC via `audio` encoder crate or libav AAC | medium |
| `media.rs` (256) | AVFoundation decode for media sources | FFmpeg (`libav` bindings or CLI pipe at snapshot warm-up); the engine's `MediaReader` surface is the same | medium |
| `join.rs` (679) | AVMutableComposition-based concat export re-encoding AAC | ffmpeg concat demuxer +AAC re-encode (it already re-encodes there); or Stream copy where it already does | small |
| `jpeg.rs` (137) | CoreImage JPEG | `turbojpeg` (or image crate with lossy quality); trivial | trivial |
| `keychain.rs` (92) | Security framework | `keyring` crate (`secret-service` via libsecret on Linux/GNOME keyring; kwallet backend exists too) | small |
| `hotkeys.rs` (471) | Carbon global chords + passive media-key tap | Hyprland `bind` IPC (`hyprctl dispatch` + `hyprland.conf` injection on bind), falling back to the desktop's global-shortcuts portal (`org.freedesktop.portal.GlobalShortcuts` — supported on GNOME/KDE; Hyprland's status to verify); focused-window chords go through Slint/winit as today | medium |
| `devices.rs` (660) | SysProfiler-style discovery + stable ids | enumerating: displays from DRM/portal, windows from Hyprland IPC, cameras from V4L2, sinks/records from PipeWire; stable ids = ALSA node ids / DRM connector names — good ones exist | medium |
| `permissions.rs` (230) | TCC queries/prompts | the portal *is* the permission: doctor checks "portal alive, screencast picker last seen granted, pipewire playing", and `request_permissions` opens the portal dialog; near-parity with the macOS doctor story, just different UI | small |
| `system.rs` (213) | launchd/energy paths + `powermetrics` | `/proc`-based stats + NVIDIA `nvml` bindings for GPU-time; for the per-process parity S.1 wants, nvidia-smi `--query-accounted-apps` covers GPU energy | medium |
| `diagnostics.rs` (659) | per-process CPU/GPU counters (Mach APIs), disks/power | `/proc/PID/stat` + VFS counters for CPU, `nvml` for GPU, AMD RAPL (`/sys/class/power_supply/BAT*`-independent `cpuinfo_*` power caps) for package energy; no `powermetrics` equivalent — the sampler CPU cost target (~0.2% of a core) must be re-proven here | medium |
| `plugin.rs` (488) | macOS credential/helper plumbing | mostly portable; drop the Mach-service bits | small |
| `output.rs` (376) | AVFoundation render callback for audio playback | `pipewire-rs` play on device; near-parity with the AVFoundation path | small |
| `termination.rs` (61) | NSApplication quit hook vs daemon files | SIGTERM handler does the same job (daemon doesn't run an app main loop on Linux) | trivial |
| `virtual_camera*` (90+381+239) | CMIO camera extension + IOSurface sink | v4l2loopback kernel module (userspace writer is a plain file to write; module needs installing, `sudo modprobe`) | medium |
| `browser/` (639) | WKWebView offscreen window + page-audio routing | **the deepest niche** — three real options (§3.4); none as sharp as WKWebView's offscreen snapshot; WebKitGTK is the closest safe one | large |

## 3.3 The three hard comm things (say it up front)

1. **Per-application audio capture.** ScreenCaptureKit app-mode has *no*
   Linux equivalent the portal/PipeWire stack gives for free. The honest
   options: route apps through a dedicated `pw` sink they must opt into
   (an agent-friendly story: `isocast doctor` offers to wire it via
   `pw-loopback`/filter-chain), or patch PulseAudio record-of-`monitor`
   plus a manual exclusion run list. Start with **system audio + mic only**
   (matching the phase-3 milestone) and mark app audio as a documented gap.

2. **ProRes and 10-bit through NVENC.** VideoToolbox does ProRes in the
   media engines; NVENC does not. ProRes on this CPU is software only and
   will cost most of a core per 4K60 stream. The practical Linux quality
   tier becomes HEVC/H.264 NVENC (+ optional AV1 on Ada GPUs, not on a
   3060 Ti) with 10-bit HEVC via `hevc_nvenc p010` where supported by the
   driver — verify at runtime and fall back to 8-bit with the quality
   preset struggled upward.

3. **AVAssetWriter guarantees.** The whole "never lose a recording"
   contract (fragmented writing, last-sample flush, G.3/R1-R2 pagination,
   `recoverable_seconds` honesty) is built on AVAssetWriter behaviour we
   measured. On Linux we own the muxer, which is *better*: fragmented MP4
   with explicit fsync per accepted fragment gives *stronger* guarantees
   than AVAssetWriter's (we can fsync exactly when the engine's counts
   say "committed"), but it has to be built with the same 55-kill matrix
   the macOS side was validated with.

## 3.4 Browser source picks

Ranked by fidelity-of-fit for an unaffected look:

1. **WebKitGTK** (`webkit2gtk-6.0`) — same engine family Court's
   `objc2-web-kit` targets, open source, hardware-accelerated. Offscreen
   (`MediaList` window off-screen painting) works; page-audio routing to
   the mixer needs the same `audio.js`-style in-page script trick with a
   WebKitGTK `AudioContext` bridge instead of the AVFoundation capture
   context. Best open-source match; one new GTK windowing dependency.
2. **CEF** (`cef-rs`) — Chromium 的 same engine OBS uses on every
   platform; audio capture is native, dependency is heavy (a ~100 MB
   blob), and it vendors Chromium — a licensing/documentation cost the
   project's clean-room rule doesn't forbid but keeps the tree heavier.
3. **`wry`/WKWebView-on-Linux** — poor fit: it always opens a visible
   window on Linux (contradicts "offscreen page"), unless run through the
   thread it demands offscreen — even then rendering is window-accelerated
   and the capture story is unknown.

Pick WebKitGTK first; CEF is the fallback if WebKitGTK's page-audio
bridge proves unreliable. Either way: same `BrowserControl` surface, one
feature flag per engine choice bound at Cargo `cfg`.

## 3.8 The zero-copy GPU story (the performance heart)

macOS achieves the flagship numbers by never copying video memory:
ScreenCaptureKit hands IOSurfaces, `CVMetalTextureCache` turns each plane
into a Metal texture, wgpu-hal wraps it, and NVENC/AVAssetWriter take the
same surface. Rebuilding that on Vulkan/NVIDIA:

1. **Capture side.** PipeWire's screencast portal delivers either SHM or
   DMA-BUF (`pw_stream` nodes negotiate ` spa_video_format.bgra` either
   way). Ask for DMA-BUF and import straight into wgpu via
   `wgpu-hal::vulkan` external memory — no CPU copy is taken, matching
   `texture_from_raw`'s cost.
2. **Compositor draw** is already pure wgpu, untouched.
3. **Encode side.** Getting NVENC to consume the Vulkan image without a
   GPU→GPU copy through an FFmpeg CLI pipe is not possible with a
   `rawvideo` pipe. In-process `libav` (or direct `cuvid`/`cuBLAS`
   bindings) is the only way to a zero-copy NVENC path. Plan the milestone
   to **accept one GPU→CPU→NVENC copy** in v0.1 (that's what the numbers
   measure against) and to wire the libav zero-copy path as the pair to
   S.3 optimization rounds on this backend, exactly as the macOS branch
   did after its first zero-copy path closed.

## 4. The platform crate: shape

```text
 crates/
  isocast-platform-macos/   (untouched)
  isocast-platform-linux/   (new: impl Platform for LinuxPlatform)
  isocast-daemon/           (one change: MacPlatform -> [cfg] LinuxPlatform)
```

- Name the surviving trait impl `LinuxPlatform` (types `Frame = Buf`),
  mirroring the macOS crate's `MacPlatform`, keeping every method surface
  identical so CLI/MCP/schema diffs stay empty.
- `wgpu` features: change `"metal"` to per-target `metal | vulkan`
  (`cfg` list features in engine Cargo.toml). `Backends::PRIMARY` already
  resolves Vulkan-first on Linux, so `Gpu::new()` needs no edit.
- `native-tls` on Linux pulls OpenSSL via `openssl-sys`; pkg-config +
  `libssl-dev` are already present on this machine. No dependency change.

## 5. Milestones (smallest-first, each verifiable)

- **L.0 Skeleton:** `LinuxPlatform` with synthetic sources only, a real
  writer (NVENC via ffmpeg CLI / fragmented MP4), pacing, secrets, and
  `isocast record` writing a **flagship** synthetic take whose files
  `ffprobe` like the macOS ones and whose 10-minute take lines up within a
  frame. Verifies the whole encode/write/pacing path before any capture.
- **L.1 Devices + doctor/tune for this box:** `isocast devices`,
  `doctor`, `tune` with Linux backends (NVENC session-count probe,
  pipeline pick, disk-speed probe already portable).
- **L.2 Screen capture:** PipeWire screencast from the Hyprland portal +
  the DMA-BUF import (with plain BGRA copies still working). Verified with
  `isocast record --source screen --seconds 10` against `ffprobe`.
- **L.3 Camera + mic + system audio:** V4L2 camera, PipeWire mic +
  monitor-of-sink with `doctor`-validated session caps on NVENC.
- **L.4 Shared surfaces + UI:** DMA-BUF shared rings and the Slint
  previews; the benchmark harness's S.1 rows recorded on this machine.
- **L.5 Renderer zero-copy + NVENC in-process:** wire libav NVENC;
  re-run S.1/S.2 to verify the drop zero remains and CPU degrades less
  than the copy path.
- **L.6 Browser source (WebKitGTK), virtual camera (v4l2loopback),
  OBS collection import parity checks**, same验收 each.

Before L.5, *all* "performance metrics" parity is the honest claim across
the encode/write/pacing path plus whatever capture reads; after L.5 the
macOS-matched zero-copy claim is available.

## 6. Honest caveats for a "same performance metrics" claim on this rig

- The M-series had the encoders *and* the mixer on one chip. Here the
  same three-way pipeline splits CPU (5800X) and GPU (3060 Ti): the daemon
  CPU medians will read *worse* than the M2's at first because what macOS
  charged to `VT services` charges to our process. That is a measurement
  relocation, not a regression; S.1's table needs an in-process row on
  Linux NVENC.

- macOS's pacing work (S.3 hosted / launchd coalescing) was fighting macOS
  timer slack of 80-200 ms for daemon contexts. Linux's `clock_nanosleep`
  is 1-2 orders of magnitude better in the *worst* cases, so the pacing
  task list is simpler, yet the escalation ladder (`PROCESS_STRETCHED`,
  Adaptive escalation) should be preserved and hooked to Linux evidence
  rather than removed — background loads on this rig (cargo, browsers) can
  still stretch wakes.

- NVENC `hevc_nvenc` quality-per-bit at p5 on a 3060 Ti is behind
  VideoToolbox's HEVC "high"; if Ryan's quality taste tuned to the Mac's
  HEVC "high" encode the parity check on size/bitrate alone will see
  larger files at the same preset. Expect to have to raise the bitrate
  target (or drop a global quality-BS ladder note) to compare fairly.

- ProRes on Linux is software-only; if a setup asks ProRes on this rig,
  `doctor` should warn that the 4K tier will not keep up and `tune` offers
  HEVC instead — same message macOS users already got at the encoder
  capacity limit, just harder here.

## 7. What really does not port (write it into `docs/obs-parity.md` for Linux)

- **Continuity Camera / iPhone over USB** — macOS-only, full stop.
- **CoreMediaIO virtual camera** — becomes v4l2loopback; apps see it
  identically once the module is loaded, but there's no "camera extension
  with provisioning story": the one-time `sudo modprobe` stays.
- **ScreenCaptureKit per-app capture & app-audio mode** — §3.3(1) gap.
- **CoreAudio monitoring of arbitrary output by route** — on PipeWire
  monitoring exists but per-app routing rules differ; same gap direction.
- **TCC-style System Settings integration** — gone; portals replace it.

None of these block the flagship use case Ryan actually runs (display +
camera + mic + system audio, streams, ISO files, timeline), and the
history of the task queue (synthetic sources first, trait-first
framings) means they are written as *absences in the backend table*, not
in the engine.
