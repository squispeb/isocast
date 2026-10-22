# Linux (Omarchy) port: status

Where the Linux port stands, what is proven, what is open, and how it
relates to upstream `main`. Snapshot: 2026-10-22. Design and rationale
live in [port-omarchy.md](port-omarchy.md); per-task evidence lives in
the `PLAN.md` Log and [notes.md](notes.md).

## 1. Summary

- **Branch:** local `main` is based on **v0.0.3** (`0386c25`,
  v0.0.3 + 3 commits) with 39 Linux commits on top, none pushed. The
  latest upstream release is **v0.0.8** (2026-10-19), so we are **5
  releases behind**. Upstream is tracked by release tags, not commits
  on `main`; the sync plan is
  [linux-release-sync-plan.md](linux-release-sync-plan.md).
- **Crate:** `crates/isocast-platform-linux` implements the engine's
  `Platform` trait. The daemon and CLI select it with
  `cfg(target_os = "linux")` (`isocast-daemon/src/main.rs`,
  `isocast-cli/src/main.rs`).
- **Test machine:** Omarchy (Arch, Hyprland, Wayland), PipeWire,
  NVIDIA with NVENC, a 2×-scaled 2560×1440 panel and a Logitech C920.
- **What works today:** the daemon, CLI and Slint app all run. A take
  records program plus ISO files with cameras, microphones, system
  audio, per-app audio, browser pages (video only) and synthetic
  sources. Live monitors use shared surfaces. Live H.264/AAC encoders
  feed streaming and the monitor path. Timeline joins run in-process.
- **What doesn't yet:** a verified real desktop take (needs one
  picker click), browser page audio (in progress), the virtual camera,
  hotkeys and plugin sandboxing. The writer's crash safety and
  zero-copy DMA-BUF are deferred.

## 2. Milestones

| Task | State | What it delivers | How it was verified |
|---|---|---|---|
| L.0 Backend skeleton | done | Crate behind `Platform`. ffmpeg-child file writer with one thread per stream (fixes a pipe deadlock). Wgpu/Vulkan importer. JPEG and live monitors in the app. | Program+ISO takes with audio, checked with ffprobe; the deadlock found with gdb and fixed (`d9dfad9`, `ac465a7`). |
| L.1 Devices, doctor, tune, media | mostly done | `devices` (cameras as capture sees them, Hyprland windows by address), doctor's real hardware encoders and NVENC session cap, media files decoded through ffmpeg. | Real runs on this machine. **Open:** writer exact timestamps and fsynced fragments. |
| L.2 Screen capture | code complete | xdg-desktop-portal ScreenCast over zbus (`portal.rs`); PipeWire consumer copies frames to BGRA (`screencast.rs`); restore token in the 0600 secrets file. | Handshake and stream exercised. **Open:** one real desktop take (blocked on a picker click). DMA-BUF import is deferred. |
| L.3 Camera, mic, system and per-app audio | done | V4L2 cameras through ffmpeg (MJPEG/YUYV). Microphones and output monitors over in-process PipeWire. Per-app audio by creating links like `pw-link` does, because WirePlumber refuses capture→playback links. | C920 3 s ISO: 90/90 frames at 30 fps, 0 dropped, live picture by YAVG stats. Audio by raw samples and FFT. |
| L.4.1–L.4.5 Shared monitor surfaces | done | memfd/descriptor rings in place of Mach ports. The daemon draws rings, the client and app show them. | Release build at 720p30: ~30 fps program viewer, ~38 ms latency, 24–29% of a core, against ~80% for the JPEG poll it replaced. |
| L.4.6 DMA-BUF zero copy | deferred | — | L.4.5's numbers say the copy is affordable for now. |
| L.5.1 Live H.264 encoder | done | `LiveH264Encoder`: NVENC in-process through libavcodec. | Elementary stream from `isocast frames`, checked with ffprobe. |
| L.5.2 Live AAC encoder | done | `LiveAacEncoder`, libavcodec's native AAC. | Raw samples and packet counts. |
| L.5.3 Joins | done | `join.rs`: in-process libavformat/libavcodec joins, video copied as-is and retimed, audio through one continuous AAC encoder. | 113 frames with an exact seam; audio sample-for-sample across the cut; 9.3 ms tail. |
| L.6a Browser video | done (`78e2700`) | WebKitGTK `WebView` in a shown `GtkOffscreenWindow` on one shared GTK thread, snapshotted each frame into the BGRA pool. | 640×480/30 static page: 60/90 frames, 0 dropped, corner boundary exactly at row/col 100. `file://` and `http://` decode identically. Checked both in-process (`record`) and in isocastd (`start`/`stop`). |
| L.6b Browser page audio | **in progress (uncommitted)** | Port of macOS `audio.js` plus a script-message handler into the `AudioSink`. | — |
| L.6c BrowserControl | code in L.6a | Refresh and load/unload over the GTK thread. | Not yet exercised by a real call (L.6d). |
| L.6d Browser real take | open | Transparent-page composite and a live page. | — |
| L.6e Virtual camera | **blocked** | v4l2loopback output. | Kernel module not installed (needs root). |
| L.6f Hotkeys | open | Hyprland `bind` IPC, with the GlobalShortcuts portal as fallback. | — |
| L.6g Plugin sandboxing | open | SO_PEERCRED plus Landlock. | — |

## 3. Work in progress (uncommitted)

L.6b is partway through, and **the tree does not build right now**
(`cargo check -p isocast-platform-linux`: 9 errors).

- `src/browser.rs` was moved to `src/browser/mod.rs` to match macOS's
  layout. `src/browser/audio.js` is a copy of the macOS script. `git`
  sees the move as a delete plus an untracked folder; stage both
  together.
- `capture_browser` no longer refuses audio. `set_up` takes an
  `Option<AudioRx>`, registers the `isocastAudio` handler, injects the
  script at document start, and lets media play without a click.
- Still missing:
  1. Cargo dependencies `base64 = "0.22.1"` and
     `java_script_core = { package = "javascriptcore-rs", version = "=1.1" }`
     (webkit2gtk doesn't re-export it).
  2. Imports: `UserContentManagerExt` must be in scope at module level
     (it's only imported inside the CSS block today), and so must
     `SettingsExt`, for `set_media_playback_requires_user_gesture`.
  3. The `AudioRx` receiver, `decode`, `Stamper` and `Resampler` with
     their unit tests (ported from macOS). The edit that adds them
     failed because the `BrowserCapture::drop` anchor text had changed.
  4. Fix the `UserScriptInjectionTime` variant name: the binding calls
     it `Start`, not `AtDocumentStart`.
  5. Verify with a page that plays a known tone: FFT of the recorded
     track, plus A/V offset against the host clock.

## 4. Blocked on Ryan

- **L.2 real desktop take:** a source open is bounded to 8 s, which
  isn't long enough to answer the picker, so the picker must be
  answered first. Run `./target/debug/isocast permissions --request`,
  pick DP-3, then do a take and expect 2560×1440. The token then opens
  later captures silently.
- **L.6e virtual camera:** install the module with
  `sudo pacman -S v4l2loopback-dkms`, `modprobe v4l2loopback` and a
  `modules-load.d` entry (exact commands in notes.md, "Waiting on
  Ryan").
- **Text-guard commits** `2b85ce3`/`a59ed86`: the CI CJK grep guard
  (`scripts/check-text.sh`) question is still open.

## 5. Findings worth keeping (Linux-specific)

- ffmpeg picks between its inputs by timestamp. One writer thread
  feeding video and audio pipes therefore deadlocks, so use one
  thread per stream.
- WirePlumber won't link a capture stream to a playback node, so
  per-app audio creates its own links.
- glib 0.18's `timeout_add_local` attaches to the *global* default
  context. On a dedicated thread, use `timeout_source_new(..).attach(ctx)`.
  `SourceId::remove` (`g_source_remove`) also searches only the global
  context, so destroy sources through `ctx.find_source_by_id`.
- WebKitGTK errors on snapshots of a view whose offscreen window was
  never shown. `show_all()` fixes it; the `WEBKIT_DISABLE_*` env vars
  are not needed.
- Snapshots arrive at the display scale (2× here).
- `isocast record` runs the capture in the CLI process. Its stderr is
  the CLI's, not isocastd's `-native-` journal. After rebuilding,
  `isocast restart` before testing through the daemon: a stale daemon
  produces black frames for new source kinds.
- bindgen's libav enums with ROUND flags are UB in debug builds. Use
  `av_rescale_q` and explicit `AV_NOPTS` guards.
- Recorded pure red reads (234,0,3) because of the pipeline's
  limited-range YUV round trip. That isn't a capture bug.

## 6. Divergence from upstream

The next step is the release sync to v0.0.8:
[linux-release-sync-plan.md](linux-release-sync-plan.md). The notes
below describe upstream `main` *past* v0.0.8 (the knowledge reset,
remote engines and the Windows app), which arrives with the 0.0.9
sync, not now. Upstream `main` has changed a lot of what this branch
is built on:

- **Process:** a "knowledge reset" (`c6a2689`). `PLAN.md` is now a
  64-line roadmap, and the old queue and `notes.md` moved to
  `docs/archive/2026-10-22/`. Work is tracked in GitHub issues; current
  facts are in `docs/STATE.md`. `AGENTS.md` was rewritten (at most
  three lanes; CI is QA; merge only on green). **Linux is listed under
  "Later … after remote engines; nothing above may block it."**
- **Publishing:** the public repo has its own history, fed by
  `scripts/publish-snapshot.sh` / `scripts/ci.sh` (`git archive`
  snapshots). Agents never push.
- **Platform selection:** `isocast-daemon/src/platform.rs` now picks
  `NativePlatform`: macOS, Windows, otherwise `isocast-platform-stub`.
  Linux needs its own `cfg` arm there, in place of our direct import in
  `main.rs`.
- **New crates the port should use:** `isocast-platform-stub`,
  `-contracts` (contract workers, `scripts/platform-contracts.py`),
  `-credentials` (secrets; Linux still uses its 0600 file), `isocast-ipc`,
  `isocast-mux` (a crash-safe MOV/MP4 writer, currently Windows-only,
  and the natural answer to L.1's open writer row), `isocast-remote`,
  `isocast-discovery`, `isocast-obsws`.
- **`Platform` trait drift:** upstream added `same_volume` (required),
  plus `video_encoders`, `create_writer_for_frame`/`_batch`,
  `recover_file`, `retain_frame`, `encoder_frame_format`,
  `frame_budget`, `governor_headroom`, `process_cpu_seconds`/
  `system_cpu_seconds`, `recording_activity`, `camera_metadata`,
  `gpu_adapter_luid`, `prefers_deadline_timer`, `audio_thread`,
  `app_identifier` and `calibration_priority` (mostly with defaults). It
  added `Playback` and `SharedSurface` associated types and
  `FileWriter::finish(end)`/`commit`/`crash_safe`. It dropped this
  branch's `concurrent_encodes`, `converts_own_inputs` and
  `publish_shared_surface`. `isocast-surfaces` also changed on the
  Windows side (DX12 shared handles).

**Path:** park L.6b, merge the v0.0.8 tag and re-verify, following
[linux-release-sync-plan.md](linux-release-sync-plan.md). Only after
that, resume L.6b and the rest of L.6. The format changes above
(`docs/STATE.md`, the issues-based workflow) are taken on at the 0.0.9
sync. Nothing heads for `ci/<lane>` without Ryan, because Linux is not
on upstream's "Now" or "Next" list.
