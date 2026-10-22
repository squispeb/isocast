# Linux (Omarchy) port: status

Where the Linux port stands, what is proven, what is open, and how it
relates to upstream `main`. Snapshot: 2026-10-22, after the v0.0.8
sync. Design and rationale
live in [port-omarchy.md](port-omarchy.md); per-task evidence lives in
the `PLAN.md` Log and [notes.md](notes.md).

## 1. Summary

- **Branch:** local `main` is based on **v0.0.8** (`c3baa39`, the
  latest release, 2026-10-19), merged in by `f8fd13e` ("Sync Linux
  port to v0.0.8") on top of the 39 Linux commits, plus the fixes the
  sync's verification found, and L.6b (browser page audio) after it.
  Nothing is pushed. Upstream is tracked by release tags, not commits
  on `main`; the sync plan is
  [linux-release-sync-plan.md](linux-release-sync-plan.md). The
  pre-sync tip is kept as `linux/pre-0.0.8` (`78e2700`).
- **Crate:** `crates/isocast-platform-linux` implements the engine's
  `Platform` trait. The daemon selects it in
  `isocast-daemon/src/platform.rs` (`NativePlatform`, a Linux arm next
  to macOS and Windows). The CLI no longer links it: since v0.0.8
  `isocast record` runs a private daemon.
- **Test machine:** Omarchy (Arch, Hyprland, Wayland), PipeWire,
  NVIDIA with NVENC, a 2×-scaled 2560×1440 panel and a Logitech C920.
- **What works today:** the daemon, CLI and Slint app all run. A take
  records program plus ISO files with cameras, microphones (with
  v0.0.8's channel picking), system audio, per-app audio, browser
  pages (picture and sound) and synthetic sources, on extra and vertical
  canvases too. Live monitors and projectors use shared surfaces. Live
  H.264/AAC encoders feed monitors, the replay buffer and multistream.
  Timeline joins run in-process; obs-websocket, masks, NLE export and
  `.isoprj` projects work.
- **What doesn't yet:** a verified real desktop take (needs one
  picker click), the virtual camera,
  hotkeys and plugin sandboxing. Editor exports with in points, crops
  or angles, stream destinations with their own size, editor sound and
  machine calibration are refused with a message (section 3). The
  writer's crash safety and zero-copy DMA-BUF are deferred.

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
| L.S1 Sync to v0.0.8 | done (`f8fd13e` + fixes) | v0.0.8 merged; Linux fitted to its `Platform`; every earlier verification repeated. Found and fixed: the program's H.264 (monitors, replay, streams) was a flat green picture; browser pages aborted isocastd at exit; editor joins silently wrote the wrong frames (now refused). | fmt, clippy `-D warnings` and 262 tests clean on the workspace. The table in section 3. |
| L.6a Browser video | done (`78e2700`) | WebKitGTK `WebView` in a shown `GtkOffscreenWindow` on one shared GTK thread, snapshotted each frame into the BGRA pool. | 640×480/30 static page: 60/90 frames, 0 dropped, corner boundary exactly at row/col 100. `file://` and `http://` decode identically. Checked both in-process (`record`) and in isocastd (`start`/`stop`). |
| L.6b Browser page audio | done | macOS's `audio.js` plus a script-message handler into the `AudioSink` (decode, resample to 48 kHz, stamp); autoplay allowed while the sound is routed to Isocast. | Gated 440 Hz page: 439.9 Hz, bursts 1.000 s apart, 0 dropouts; sound 38.7 ms ahead of a flash timed off the same AudioContext. |
| L.6c BrowserControl | code in L.6a | Refresh and load/unload over the GTK thread. | Not yet exercised by a real call (L.6d). |
| L.6d Browser real take | open | Transparent-page composite and a live page. | — |
| L.6e Virtual camera | **blocked** | v4l2loopback output. | Kernel module not installed (needs root). |
| L.6f Hotkeys | open | Hyprland `bind` IPC, with the GlobalShortcuts portal as fallback. | — |
| L.6g Plugin sandboxing | open | SO_PEERCRED plus Landlock. | — |

## 3. Release features on Linux (0.0.4 to 0.0.8)

Checked 2026-10-22 against the scratch daemon (`ISOCAST_SOCKET` and
`ISOCAST_SETUP` under `target/scratch/sync-0.0.8/`), numbers only.
Each feature *works*, is *refused* with a message, or is *broken*.

| Feature | Release | On Linux | Evidence |
|---|---|---|---|
| Crash-safe takes | 0.0.5 | works, honestly not crash-safe | The ffmpeg writer keeps `crash_safe() == false`, so no commit barriers run. `kill -9` of isocastd 4 s into a take: both files keep 3.8 s (114 frames); the next start lists the take as interrupted, "recoverable duration unknown". `isocast-mux` or fragmented MP4 is the follow-up. |
| Extra canvases, vertical canvas | 0.0.6, 0.0.8 | works | Program + ISO + vertical + a 720×720 canvas: four files, 90/90 frames each, right sizes, real pictures; the vertical monitor stream decodes. |
| Multistream, per-destination settings | 0.0.5, 0.0.8 | works, except a destination's own size (refused) | Two local `ffmpeg -listen` receivers: 3039 and 802 kb/s by destination, 0 dropped, both with the picture. A destination with its own width/height fails with "scaling a stream to a destination's own size isn't available here". |
| Replay buffer | 0.0.5 | works | `replay save` after 6 s: 6.0 s, 181 frames, 0 decode errors, picture and tone. |
| obs-websocket | 0.0.5 | works | v5 handshake with the password from the Linux secrets file; GetVersion, GetSceneList, GetRecordStatus answered. |
| Editor: timeline edits and export | 0.0.5–0.0.7 | partly: out-point trims work; in points, crops, angles and vertical edited files refused | The Linux join copied each file from its start and ignored v0.0.8's `in_seconds`, `crop`, `audio` and `size`, so an edited export held the wrong frames. It now refuses them by field ("parts[1].in_seconds: … not available on Linux yet"). |
| Editor sound, audio monitoring | 0.0.7 | refused | `play_audio` bails: PipeWire playback isn't written yet. |
| NLE export | 0.0.7 | works | fcpxml, xmeml (and Resolve's), otio written and well-formed. They point at the takes, so they also work around the join refusals. |
| Projectors and multiview | 0.0.6–0.0.7 | works | The app opens both (`shown: true`); the daemon serves their shared-frame feeds (program, scene tiles, vertical) with no errors. Pixels not inspected. |
| Masks | 0.0.8 | works | Circle shape mask on a white item: 384 px circle at x 128–511, y 48–431 (the 0.8 default box), black outside. |
| Governor, frame budget | 0.0.8 | governor works; no frame budget | Governor runs and reacted to output pressure during multistream. `frame_budget` and `governor_headroom` stay `None` (no Linux ledger or sampler yet), so the doctor shows no budget line. |
| Machine profile | 0.0.8 | refused | Calibration's worker is macOS-only in the daemon: "this platform has no calibration backend yet"; the capacity table is used. |
| `.isoprj` files | 0.0.6 | works | A copied project folder's `.isoprj` opens and its timeline points at the copied files. |
| Virtual camera | 0.0.5 | refused until L.6e | "not available on Linux yet; it arrives as a v4l2loopback camera (L.6e)". |
| Microphone channels | 0.0.8 | works | `channels = [2]` on the C920 records mono, 0 dropouts; `[3]` is refused with "valid: 1 to 2". |
| System audio excluding apps | 0.0.8 | refused | A monitor carries every app mixed; recording what the setup excludes would be worse than refusing. |

Not checked in this pass: OBS import, Studio Mode, tally colours and
the editor window itself.

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
- In the offscreen view, a page's `requestAnimationFrame` loop stops
  after its first frames (a flash driven by it stayed white for a whole
  take), while `setInterval` keeps running. A live page (L.6d) needs
  this looked at.
- WebKitGTK keeps an `AudioContext` suspended without a click unless
  the view's `WebsitePolicies` autoplay is `Allow`;
  `media-playback-requires-user-gesture` alone isn't enough.
- Since v0.0.8, `isocast record` runs a private isocastd and shuts it
  down afterwards, so exit-time bugs show as a failed record ("isolated
  daemon exited …; diagnostics retained in /tmp/isocast-…"). After
  rebuilding, `isocast restart` before testing through the daemon: a
  stale daemon produces black frames for new source kinds.
- WebKit's default context is a C++ static that `exit()` destroys on
  the exiting thread; its timers then abort. The pages use a context
  owned by the page thread instead.
- Frames the GPU drew hold their picture in textures until
  `gpu::read_back`; anything reading `LinuxFrame::plane` must read back
  first (the writer, JPEG and now the live encoder do).
- bindgen's libav enums with ROUND flags are UB in debug builds. Use
  `av_rescale_q` and explicit `AV_NOPTS` guards.
- Recorded pure red reads (234,0,3) because of the pipeline's
  limited-range YUV round trip. That isn't a capture bug.

## 6. Divergence from upstream

We are synced to **v0.0.8**. The notes below describe upstream `main`
*past* v0.0.8 (the knowledge reset, remote engines and the Windows
app), which arrives with the 0.0.9 sync:

- **Process:** a "knowledge reset" (`c6a2689`). `PLAN.md` is now a
  64-line roadmap, and the old queue and `notes.md` moved to
  `docs/archive/2026-10-22/`. Work is tracked in GitHub issues; current
  facts are in `docs/STATE.md`. `AGENTS.md` was rewritten (at most
  three lanes; CI is QA; merge only on green). **Linux is listed under
  "Later … after remote engines; nothing above may block it."**
- **Publishing:** the public repo has its own history, fed by
  `scripts/publish-snapshot.sh` / `scripts/ci.sh` (`git archive`
  snapshots). Agents never push.
- **More `Platform` drift after v0.0.8:** `video_encoders`,
  `create_writer_batch`, `recording_activity`, `gpu_adapter_luid`,
  `prefers_deadline_timer`, `audio_thread`. Upstream `main` also has
  none of this branch's three additive methods (`concurrent_encodes`,
  `converts_own_inputs`, `publish_shared_surface`); at the 0.0.9 sync,
  check again whether its versions cover them.
- **Crates the port should use later:** `-credentials` (secrets;
  Linux still uses its 0600 file), `isocast-mux` (crash-safe MOV/MP4,
  Windows-only at v0.0.8, the natural answer to L.1's writer row),
  `isocast-remote`, `isocast-discovery`. `isocast-surfaces` also
  changed on the Windows side (DX12 shared handles).

**Next:** L.6c, L.6d (a live page: see the `requestAnimationFrame`
finding above), L.6f and L.6g, then the L.S1 follow-ups. The format changes above
(`docs/STATE.md`, the issues-based workflow) are taken on at the 0.0.9
sync. Nothing heads for `ci/<lane>` without Ryan, because Linux is not
on upstream's "Now" or "Next" list.
