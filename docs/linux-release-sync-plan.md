# Linux port: syncing to upstream releases

Written 2026-10-22. The Linux port follows upstream by **release**, not
by commits on `main`. Work between releases is unreleased and still
moving, so the port catches up only to a tagged release. This plan
covers the first catch-up: from our base (v0.0.3) to **v0.0.8**, the
latest release.

## 1. Where we stand, in releases

| | |
|---|---|
| Our base | `0386c25` = **v0.0.3 + 3 commits** (`git describe`: `v0.0.3-3-g0386c25`), workspace version `0.0.3` |
| Latest release | **v0.0.8** (`c3baa39`, tagged 2026-10-19), workspace version `0.0.8` |
| Releases behind | **5** (0.0.4, 0.0.5, 0.0.6, 0.0.7, 0.0.8) |
| Our work on top | 39 Linux commits (L.0 to L.6a), plus uncommitted L.6b |
| Out of scope | the 9 commits on `origin/main` after v0.0.8 (knowledge reset, remote engines R1, Windows app, website). They come with 0.0.9. |

What those five releases added, from `docs/releases/0.0.5.md` to
`0.0.8.md` (0.0.4 has no notes file):

- **0.0.5:** OBS import, obs-websocket v5, replay buffer, virtual
  camera, multistream, the editor window, crash-safe takes by default,
  per-ISO audio.
- **0.0.6:** projectors, multiview, a vertical 9:16 canvas, tally
  colours, crop in the editor, `.isoprj` project files.
- **0.0.7:** NLE export (Final Cut, Premiere, Resolve), editor sound,
  one GPU device for the whole app, a lighter multiview.
- **0.0.8:** up to eight extra canvases, per-destination settings,
  shape masks, a measured machine profile, a live quality governor and
  a frame budget across outputs.

Structure that v0.0.8 already has and the port must fit into: the
`isocast-daemon/src/platform.rs` platform selector (macOS, Windows,
otherwise `isocast-platform-stub`), `isocast-ipc`, `isocast-mux`
(crash-safe writer), `isocast-platform-windows` and
`isocast-platform-contracts`. v0.0.8 still has the old 5,333-line
`PLAN.md` and `docs/notes.md`; the knowledge reset came after it.

## 2. How big the overlap is

54 files changed on our side and 571 on theirs. **28 files changed on
both sides.** The real conflicts are where both sides edited the same
code:

| File | Ours | v0.0.8 | Risk |
|---|---|---|---|
| `isocast-daemon/src/main.rs` | +84/−69 | +2077/−610 | high: our platform import moves into `platform.rs` |
| `isocast-engine/src/lib.rs` | +58/−3 | +310/−14 | high: `Platform` trait drift (section 4) |
| `isocast-cli/src/main.rs` | +18/−12 | +1329/−161 | medium |
| `isocast-engine/src/record/engine.rs` | +7/−1 | +2248/−418 | medium: small edits of ours in heavily rewritten code |
| `isocast-ui/src/{frames,preview}.rs` | +198/−15 | +63/−16 | medium: the Linux monitor path |
| `isocast-surfaces/src/lib.rs` | +32/−2 | +1/−1 | low |
| `isocast-engine/src/doctor.rs` | +146/−17 | +47/−6 | low/medium |
| `isocast-daemon/src/{diagnostics,export,plugins,recovery,hotkeys}.rs`, `isocast-engine/src/{source,record/shared_frames}.rs`, `isocast-client/src/shared.rs`, `isocast-platform-macos/src/permissions.rs`, `isocast-ui/src/{window,offscreen}.rs`, `isocast-ui/src/areas/{canvas,sources/mod}.rs` | small `cfg(linux)` edits | large | low each, many of them |
| Cargo manifests, `Cargo.lock`, `THIRD_PARTY_NOTICES` | deps | deps | mechanical |
| `PLAN.md`, `docs/notes.md` | section L, Log, notes | many sections | mechanical: take theirs, re-add ours |

`crates/isocast-platform-linux/` (all ours) and `docs/port-omarchy.md`
don't conflict.

## 3. Steps

### Step 0: Make our current state safe

1. Park the unfinished L.6b. Commit it as a WIP commit on a side
   branch, `linux/l6b-wip`. It doesn't compile, so it must not go on
   the line being synced. Reset `main`'s working tree to `78e2700`.
2. Keep the pre-sync tip: `git branch linux/pre-0.0.8 78e2700`.
3. Run the full gates on `78e2700` (`cargo fmt --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo test`) and record
   the result. That gives a baseline to compare against after the sync.
4. Nothing is pushed at any step. The public repo only takes snapshots
   through the delegator.

### Step 1: Bring v0.0.8 in

- Create `linux/sync-0.0.8` from `78e2700` and **merge** the tag:
  `git merge --no-ff v0.0.8`.
- **Why merge, not rebase:**
  - Rebasing replays 39 commits. Almost every one touches `PLAN.md`
    or `docs/notes.md`, and those were rewritten upstream, so we would
    resolve the same conflicts about 39 times.
  - A merge resolves each conflict once and keeps the L.* commits and
    their verification history as they were.
  - Upstream publishes by snapshot (`git archive`), so a merge commit
    in our history costs nothing downstream.
- The merge commit's message names the release (`Sync Linux port to
  v0.0.8`) and lists the conflict decisions.

### Step 2: Resolve conflicts, area by area

Resolution rule: **take v0.0.8's code, then re-apply the smallest
Linux delta on top.** Never take ours over theirs in shared code.

1. **Docs:** take v0.0.8's `PLAN.md` and `docs/notes.md`. Re-insert our
   section L, the L.* Log entries and the Linux notes sections (all
   dated, all from `linux/pre-0.0.8`).
2. **Manifests:** take theirs, re-add the Linux crate's deps and the
   daemon/CLI/UI `cfg(target_os = "linux")` dependency blocks, then
   regenerate `Cargo.lock` with `cargo update -w` (no version bumps
   beyond what v0.0.8 pins). Add the Linux crates' licences to
   `THIRD_PARTY_NOTICES`.
3. **Platform selection:** add a Linux arm to `platform.rs`,
   `#[cfg(target_os = "linux")] pub use isocast_platform_linux::LinuxPlatform as NativePlatform;`,
   and narrow the stub to `not(any(macos, windows, linux))`. Drop our
   direct `HostPlatform` import from `daemon/main.rs` and point the
   CLI's in-process `record` at the same selector.
4. **Engine, CLI, daemon, UI files:** take theirs; re-apply each of our
   `cfg(linux)` edits by reading `git diff 0386c25 78e2700 -- <file>`.

### Step 3: Make the Linux crate fit v0.0.8's `Platform`

- **Must add:** `same_volume` (required; compare `st_dev` like
  `volume`).
- **Have defaults; decide each one on purpose and write the choice
  down:**
  - `create_writer_for_frame`
  - `recover_file`, which ties into crash-safe takes (below)
  - `retain_frame`
  - `encoder_frame_format`
  - `frame_budget`
  - `governor_headroom`
  - `process_cpu_seconds` and `system_cpu_seconds` (cheap from
    `/proc`; the governor and machine profile want them)
  - `camera_metadata`
  - `app_identifier`
  - `calibration_priority`
- **Our three branch-only methods** (`concurrent_encodes`,
  `converts_own_inputs`, `publish_shared_surface`, added for
  L.4/L.5):
  - Check whether v0.0.8 already covers the need:
    `retain_frame`/`encoder_frame_format` against
    `converts_own_inputs`, the machine profile's encoder data against
    `concurrent_encodes`, and the engine's shared-frames path against
    `publish_shared_surface`.
  - Drop the ones it covers. Keep the rest as additive trait methods
    with defaults, so macOS and Windows are untouched.
- **Crash-safe takes** (on by default since 0.0.5): see how v0.0.8's
  engine uses `FileWriter::commit`/`crash_safe`.
  - In this sync, the Linux ffmpeg writer reports itself as *not*
    crash-safe, so it is honest without a new writer.
  - Making it crash-safe, either with `isocast-mux` (pure Rust MOV/MP4,
    now Windows-only) or fragmented MP4, is the open L.1 writer row and
    a follow-up task.
- The crate must still compile to nothing on macOS and Windows, so
  upstream CI stays green on its own targets.

### Step 4: Verify

1. **Gates:** workspace `cargo fmt --check`,
   `cargo clippy --all-targets -- -D warnings` and `cargo test`, all
   clean on Linux.
2. **Repeat what was proven before**, same recipes as the PLAN Log,
   scratch in `target/scratch/sync-0.0.8/`, numbers only:
   - `isocast devices --json`, `isocast doctor`;
   - camera ISO take (C920: 90/90 frames at 30 fps, 0 dropped,
     YAVG live);
   - mic, system and per-app audio takes (FFT of a known tone);
   - program take with audio (no hang, A/V aligned);
   - live monitors in the app (shared surface ring at about 30 fps);
   - `isocast frames` H.264 elementary stream, which must pass ffprobe;
   - timeline join (exact seam, sample-exact audio);
   - browser page take (red/green corner page, file and http), both
     through `record` and through `isocastd`.
3. **New release features on Linux.** Each one ends as *works*,
   *refused with a clear `platform_text` message*, or *broken (bug
   filed)*:
   - crash-safe takes (with Step 3's honest "not crash-safe");
   - extra canvases and the vertical canvas;
   - multistream and per-destination settings;
   - replay buffer;
   - obs-websocket;
   - the editor and NLE export;
   - projectors and multiview;
   - masks;
   - governor, frame budget and machine profile;
   - `.isoprj` files;
   - virtual camera (refused until L.6e).
4. Record every failure with its evidence in `docs/notes.md` before
   fixing anything. Three failed attempts at the same problem mean it
   is marked blocked, per `AGENTS.md`.

### Step 5: Close out

- Update `docs/linux-port-status.md`: base = v0.0.8, plus the
  feature-matrix results from Step 4.3.
- Add PLAN Log line `L.S1 sync to v0.0.8` and tick it. Add
  `docs/notes.md` entries for the conflict decisions and new gotchas.
- Fast-forward local `main` to `linux/sync-0.0.8` once the gates and
  Step 4.2 pass.
- Then rebase `linux/l6b-wip` onto it and finish L.6b, followed by L.6c,
  L.6d, L.6f and L.6g.

## 4. The rule from now on

- Linux syncs only to **release tags**. When 0.0.9 is tagged, repeat
  Steps 0 to 5 from v0.0.8 to v0.0.9.
- 0.0.9 will bring the knowledge reset (`docs/STATE.md`, the short
  `PLAN.md`, the issues-based workflow). At that sync, move the Linux
  plan and notes into that format.
- Between syncs, Linux work lands on our line only. Nothing from
  upstream `main` past the last release is cherry-picked, unless a
  specific fix is needed and is recorded as such.

## 5. Open decisions (defaults chosen, overrule if needed)

| Decision | Default | Why |
|---|---|---|
| Merge or rebase onto v0.0.8 | merge | resolve each conflict once and keep the verified L.* history |
| L.6b during the sync | parked on `linux/l6b-wip` | it doesn't compile, and "pull the release first" comes before it |
| Linux writer crash safety in this sync | report not crash-safe; `isocast-mux` as a follow-up | keeps the sync a sync, not a writer rewrite |
| New 0.0.4–0.0.8 features that don't work on Linux | refuse cleanly; one follow-up task each | the sync is judged on parity of what already worked, not on every new feature |
