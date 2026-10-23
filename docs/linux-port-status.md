# Linux (Omarchy) port: status

Where the Linux port stands, what is proven, what is open, and how it
relates to upstream `main`. Snapshot: 2026-10-22, after the v0.0.8
sync. Design and rationale
live in [port-omarchy.md](port-omarchy.md); per-task evidence lives in
the `PLAN.md` Log and [notes.md](notes.md).

## Requirements (Arch / Omarchy)

Everything in this list except `v4l2loopback-dkms` is installed on the
development machine; the build and every verification in this document
ran against it.

**Build:** a stable Rust toolchain (`rustup`), plus the C libraries the
Linux crate binds to and `clang` for their bindings:

```sh
sudo pacman -S --needed rustup clang pkgconf ffmpeg pipewire webkit2gtk-4.1 gtk3 wayland
```

**Run:**

| Package | Needed for | Required? |
|---|---|---|
| `pipewire`, `wireplumber`, `pipewire-pulse` (`libpulse` gives `pactl`) | microphones, system and per-app audio, playback | yes |
| `ffmpeg` (`ffmpeg`, `ffprobe`) | the file writer, cameras, media sources, editor exports | yes |
| `xdg-desktop-portal`, `xdg-desktop-portal-hyprland` | screen capture (the ScreenCast portal) | for screen sources |
| `hyprland` | global hotkeys (session binds), app-capture window positions (`hyprctl`); window capture uses the ext-image-copy-capture protocol | for hotkeys and window/app sources |
| `webkit2gtk-4.1` | browser sources | for browser sources |
| `nvidia-utils` (or another hardware encoder) | NVENC encoding; software encoders are used without it | recommended |
| `v4l2loopback-dkms` and the running kernel's headers (`linux-omarchy-headers` on Omarchy, `linux-headers` on stock Arch) | the virtual camera (L.6e) | for the virtual camera |
| Landlock enabled in the kernel (`landlock` in `/sys/kernel/security/lsm`, ABI 4 or newer; 10 here) | the plugin sandbox (L.6g) | for sandboxed plugins |

The virtual camera's module, once (root):

```sh
sudo pacman -S --needed v4l2loopback-dkms linux-omarchy-headers   # linux-headers on stock Arch
sudo modprobe v4l2loopback devices=1 video_nr=10 card_label="Isocast Camera" exclusive_caps=1
# keep it across reboots, with the same options:
echo v4l2loopback | sudo tee /etc/modules-load.d/isocast-v4l2loopback.conf
echo 'options v4l2loopback devices=1 video_nr=10 card_label="Isocast Camera" exclusive_caps=1' \
  | sudo tee /etc/modprobe.d/isocast-v4l2loopback.conf
```

Check: `lsmod | grep v4l2loopback` and `v4l2-ctl --list-devices` show
"Isocast Camera" at `/dev/video10`.

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
- **What doesn't yet:** the virtual camera,
  hotkeys and plugin sandboxing. Stream destinations with their own size and machine
  calibration are refused with a message (section 3). Zero-copy
  DMA-BUF is deferred.

## 2. Milestones

| Task | State | What it delivers | How it was verified |
|---|---|---|---|
| L.0 Backend skeleton | done | Crate behind `Platform`. ffmpeg-child file writer with one thread per stream (fixes a pipe deadlock). Wgpu/Vulkan importer. JPEG and live monitors in the app. | Program+ISO takes with audio, checked with ffprobe; the deadlock found with gdb and fixed (`d9dfad9`, `ac465a7`). |
| L.1 Devices, doctor, tune, media | mostly done | `devices` (cameras as capture sees them, Hyprland windows by address), doctor's real hardware encoders and NVENC session cap, media files decoded through ffmpeg. | Real runs on this machine. **Open:** writer exact timestamps and fsynced fragments. |
| L.2 Screen capture | done | xdg-desktop-portal ScreenCast over zbus (`portal.rs`); PipeWire consumer copies frames to BGRA (`screencast.rs`); restore token in the 0600 secrets file. | Handshake and stream exercised. **Open:** one real desktop take (blocked on a picker click). DMA-BUF import is deferred. |
| L.2b Window and app capture | done | ext-foreign-toplevel-list + ext-image-copy-capture as a Wayland client (`toplevel.rs`): no portal or picker. | Pixel-exact window content against `grim`; follows close and reopen; app windows composited at their positions. |
| L.3 Camera, mic, system and per-app audio | done | V4L2 cameras through ffmpeg (MJPEG/YUYV). Microphones and output monitors over in-process PipeWire. Per-app audio by creating links like `pw-link` does, because WirePlumber refuses capture→playback links. | C920 3 s ISO: 90/90 frames at 30 fps, 0 dropped, live picture by YAVG stats. Audio by raw samples and FFT. |
| L.4.1–L.4.5 Shared monitor surfaces | done | memfd/descriptor rings in place of Mach ports. The daemon draws rings, the client and app show them. | Release build at 720p30: ~30 fps program viewer, ~38 ms latency, 24–29% of a core, against ~80% for the JPEG poll it replaced. |
| L.4.6 DMA-BUF zero copy | deferred | — | L.4.5's numbers say the copy is affordable for now. |
| L.5.1 Live H.264 encoder | done | `LiveH264Encoder`: NVENC in-process through libavcodec. | Elementary stream from `isocast frames`, checked with ffprobe. |
| L.5.2 Live AAC encoder | done | `LiveAacEncoder`, libavcodec's native AAC. | Raw samples and packet counts. |
| L.5.3 Joins | done | `join.rs`: in-process libavformat/libavcodec joins, video copied as-is and retimed, audio through one continuous AAC encoder. | 113 frames with an exact seam; audio sample-for-sample across the cut; 9.3 ms tail. |
| L.S1 Sync to v0.0.8 | done (`f8fd13e` + fixes) | v0.0.8 merged; Linux fitted to its `Platform`; every earlier verification repeated. Found and fixed: the program's H.264 (monitors, replay, streams) was a flat green picture; browser pages aborted isocastd at exit; editor joins silently wrote the wrong frames (now refused). | fmt, clippy `-D warnings` and 262 tests clean on the workspace. The table in section 3. |
| L.6a Browser video | done (`78e2700`) | WebKitGTK `WebView` in a shown `GtkOffscreenWindow` on one shared GTK thread, snapshotted each frame into the BGRA pool. | 640×480/30 static page: 60/90 frames, 0 dropped, corner boundary exactly at row/col 100. `file://` and `http://` decode identically. Checked both in-process (`record`) and in isocastd (`start`/`stop`). |
| L.6b Browser page audio | done | macOS's `audio.js` plus a script-message handler into the `AudioSink` (decode, resample to 48 kHz, stamp); autoplay allowed while the sound is routed to Isocast. | Gated 440 Hz page: 439.9 Hz, bursts 1.000 s apart, 0 dropouts; sound 38.7 ms ahead of a flash timed off the same AudioContext. |
| L.6c BrowserControl | done | Refresh, load/unload when hidden or shown, over the GTK thread. | `refresh` (no-cache GET), `shutdown_when_hidden` and `refresh_when_shown` verified in takes. |
| L.6d Browser real take | done | Live pages: `requestAnimationFrame` animates (GLib default context on the page thread, `HardwareAccelerationPolicy::Never`). | rAF flash page: 32 transitions in 7.8 s (0 before); a transparent page composites exactly over another source. |
| L.6e Virtual camera | code done, live path **blocked** | v4l2loopback OUTPUT sink (`vcam.rs`), BGRA to YUYV. | 13 unit tests and the refusal in a daemon; the live path needs the kernel module (root). |
| L.6f Hotkeys | done (nested compositor) | Session-only Hyprland binds, events from `.socket2.sock`. | Push-to-talk press/hold/release with a virtual keyboard in a nested Hyprland; user binds never unbound. Real session and a physical key: waiting on Ryan. |
| L.6g Plugin sandboxing | done | Landlock (ABI 10 here) applied in `pre_exec`, SO_PEERCRED on the main socket. | A probe plugin's denied and allowed reads, sockets and network checked; the deck example runs. |

## 3. Release features on Linux (0.0.4 to 0.0.8)

Checked 2026-10-22 against the scratch daemon (`ISOCAST_SOCKET` and
`ISOCAST_SETUP` under `target/scratch/sync-0.0.8/`), numbers only.
Each feature *works*, is *refused* with a message, or is *broken*.

| Feature | Release | On Linux | Evidence |
|---|---|---|---|
| Crash-safe takes | 0.0.5 | works (L.S1f) | The writer reads its whole fragments back from the file; a pause reports recoverable 6.02 s of 7.3; `kill -9` of isocastd and its writers keeps exactly the committed 6.0 s; a torn tail is cut back at launch. |
| Extra canvases, vertical canvas | 0.0.6, 0.0.8 | works | Program + ISO + vertical + a 720×720 canvas: four files, 90/90 frames each, right sizes, real pictures; the vertical monitor stream decodes. |
| Multistream, per-destination settings | 0.0.5, 0.0.8 | works (L.S1b) | Two local `ffmpeg -listen` receivers: 3039 and 802 kb/s by destination, 0 dropped. A destination's own size is scaled by libswscale on the encoder thread: 640x360 next to 1280x720, 0 dropped, 40-46 dB PSNR against the big stream downscaled; scaling costs 0.7-4.4 ms per frame. |
| Replay buffer | 0.0.5 | works | `replay save` after 6 s: 6.0 s, 181 frames, 0 decode errors, picture and tone. |
| obs-websocket | 0.0.5 | works | v5 handshake with the password from the Linux secrets file; GetVersion, GetSceneList, GetRecordStatus answered. |
| Editor: timeline edits and export | 0.0.5–0.0.7 | works (L.S1a) | In points, crops, angles with the program's sound and the vertical edited file re-encode the whole join through ffmpeg (no smart render yet); every frame lands on its source frame, audio cos 1.0 per part. Plain joins still copy. |
| Editor sound, audio monitoring | 0.0.7 | works (L.S1c) | A PipeWire output stream; a monitored tone reaches a test sink beep for beep, 0 underruns; "default" follows the default output. |
| NLE export | 0.0.7 | works | fcpxml, xmeml (and Resolve's), otio written and well-formed. They point at the takes, so they also work around the join refusals. |
| Projectors and multiview | 0.0.6–0.0.7 | works | The app opens both (`shown: true`); the daemon serves their shared-frame feeds (program, scene tiles, vertical) with no errors. Pixels not inspected. |
| Masks | 0.0.8 | works | Circle shape mask on a white item: 384 px circle at x 128–511, y 48–431 (the 0.8 default box), black outside. |
| Governor, frame budget | 0.0.8 | works (L.S1d), partial sources | Headroom from CPU % and memory PSI (no thermal or encoder-latency source); the frame budget is a CPU-pool slot ledger, shown by the doctor. The governor reducing quality under load is not verified. |
| Machine profile | 0.0.8 | works (L.S1d) | Calibration runs in the daemon's helper at nice 15 and completes in about 22 s wall (limit 29 s), with 10 to 15 NVENC and ProRes encoder rows (backend `ffmpeg`, each row says hardware or software), graphics, memory and disk rows, and honest skipped reasons; cancel reaps ffmpeg at once, even if the helper has already exited; the profile persists across restart. Rows are evidence only: the capacity fit stays macOS-only. The required UHD30 x4 HEVC fit cell often lands just under its minimum frame count, so the profile is normally partial (complete=false) on this box. Governor headroom is CPU % plus memory PSI (no thermal or encoder-latency source), and Linux ProRes ISO files now count as software ProRes for the governor; the frame budget is a CPU-pool slot ledger (`pool_count`) with no per-output domains. Fixed HEVC `bit_depth = 10` being written as 8-bit Main. Not verified: the governor degrading quality under load. |
| `.isoprj` files | 0.0.6 | works | A copied project folder's `.isoprj` opens and its timeline points at the copied files. |
| Virtual camera | 0.0.5 | code done; live path waits on v4l2loopback (L.6e) | v4l2loopback OUTPUT sink (vcam.rs): the device is found by QUERYCAP driver name ("Isocast Camera" preferred, ISOCAST_VCAM_DEVICE override). S_FMT, S_PARM and G_FMT run per start; GPU readback, then BGRA to YUYV (limited-range BT.601) and a whole-frame write(2). Transport "v4l2loopback-yuyv"; `vcam remove` stays macOS-only. On stop, readers freeze on the last frame (no placeholder card as on macOS). Without the kernel module the start is refused, naming v4l2loopback and the install and modprobe commands (exclusive_caps=1). Verified: 13 unit tests, an isolated daemon showing the refusal, and conversion at 4.1 ms (1080p) and 16.4 ms (4K). Waiting on the owner installing v4l2loopback-dkms (headers already present) to verify the live ioctl and write path, restart with a reader attached, and real colours. |
| Microphone channels | 0.0.8 | works | `channels = [2]` on the C920 records mono, 0 dropouts; `[3]` is refused with "valid: 1 to 2". |
| System audio excluding apps | 0.0.8 | works (L.S1e) | Per-stream registry links when `exclude` or `exclude_own` is used (every playback stream except the excluded and Isocast's own, tagged `isocast.owner`; picks up streams started mid-take); the default sink's monitor otherwise. Verified with pw-play and paplay tones by FFT: excluding one leaves only the other, an unmatched name excludes nothing, monitoring is left out. Limits: sound is before the sink's volume/mute; filter/loopback nodes (EasyEffects, module-loopback, a loopback from a microphone) that are themselves playback streams would be mixed in additionally and could double a signal or put a microphone into the system audio; non-stream sink audio is missed in link mode; streams on different sinks are linked under one driver (clock effect not tested). |
| Global hotkeys | 0.0.8 | works on Hyprland (L.6f), verified in a nested compositor | `LinuxHotkeys` registers session-only Hyprland binds by physical position (`code:N`; media keys by keysym) and reads Press/Release from the event socket. Push-to-talk held 2.5 s gave press, hold, release, also when the modifiers are lifted first. A plain z fires nothing. A chord that a user bind or another daemon already holds (code binds included) gives global:false and a warning, and the user's binds are never unbound. Config reload re-installs the binds, SIGTERM removes them, kill -9 leftovers are swept. Needs Hyprland (other desktops: focused input only). Not yet run on the real session or a physical keyboard. Never use `Keybind:remove()`: it crashes Hyprland 0.56.2. |
| Window and app capture | — | works (L.2b) | The standard ext-foreign-toplevel-list, ext-image-capture-source and ext-image-copy-capture protocols, as a Wayland client in toplevel.rs: no portal, picker or restore token. Window matches app id and title (largest match wins), follows the window across close and reopen, holds the take size fixed at open (even-rounded) and letterboxes a resized window. Application composites the app's windows on the display's shown workspaces at their hyprctl positions onto a transparent canvas, which clears when the windows leave. The doctor reads window sizes from the compositor, without Hyprland IPC. shm copies only, no DMA-BUF. Foot window with red/blue/green blocks: red x 0-279, blue 280-559, y 0-383 exact, matching grim's crop except under an occluding dialog. 12 s take across resize, close and reopen: resumes on reopen, 0 dropped on the program file. Two foot windows composited like grim, 0 stray pixels. Application window closed mid-take: snapshot goes fully transparent. Doctor size 1250x1322 with a live and a stale Hyprland signature. Wrong app or title lists the valid pairs. **Open:** DPMS-off, hidden-workspace and minimized windows are unverified (`hyprctl dispatch` is Lua here and has aborted the compositor), a monitor scale other than 1, release-build cost at 4K, and Isocast's own windows are not excluded. |
| Live browser pages, `BrowserControl` | — | works (L.6c, L.6d) | WebKitGTK offscreen pages animate on `requestAnimationFrame` (about 62 Hz; the page thread runs GLib's default context and the view uses `HardwareAccelerationPolicy::Never`). rAF flash: 32 transitions in 7.8 s, versus 0 before. A transparent page composites exactly over another source. `refresh` (no-cache GET), `shutdown_when_hidden` (empty while hidden, GET on return) and `refresh_when_shown` (reload on return) verified in takes. Page audio still 439.9 Hz. Software rendering: heavy or WebGL pages untested. |
| Plugin sandbox | — | works (L.6g) | Process plugins run under a Landlock ruleset (raw syscalls, applied in pre_exec; ABI 10 here, needs >= 4) with an SO_PEERCRED uid check on the main socket and a /proc-based pid scope fallback. Denied without a grant: $HOME, /tmp, /proc, main and abstract sockets, signals, exec outside the program, TCP connect, and on ABI 10 UDP and DNS sends. Allowed: own folder, data/, granted files, 127.0.0.1 serving, recordings per grant. Gaps: recordings `write` can overwrite or truncate existing files (no per-name patterns; disclosed in the consent text, `plugin add`, doctor and docs/plugins.md), and below ABI 10 UDP and DNS (below ABI 9 also PipeWire, D-Bus and the main socket, which doctor warns about) aren't limited. Verified with a probe plugin and the deck example; the macOS and Windows builds are unchecked (CI). |

Not checked in this pass: OBS import, Studio Mode, tally colours and
the editor window itself.

## 4. Blocked on Ryan

- **Hyprland is in safe mode** since 2026-10-22 22:10: agents' Lua
  probing through `hyprctl dispatch` crashed the session. Log out and
  back in to restore the normal config (notes.md, incident note).
- **L.6f on the real session:** one physical press of a registered
  chord (command in notes.md, "Waiting on Ryan").
- **L.2 real desktop take:** a source open is bounded to 8 s, which
  isn't long enough to answer the picker, so the picker must be
  answered first. Run `./target/debug/isocast permissions --request`,
  pick DP-3, then do a take and expect 2560×1440. The token then opens
  later captures silently.
- **L.6e virtual camera:** install and load the v4l2loopback module
  (commands under "Requirements" above), then verify the live path
  (notes.md, "Waiting on Ryan").
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
