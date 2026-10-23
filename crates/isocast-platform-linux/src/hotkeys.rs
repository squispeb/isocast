//! Global hotkeys on Linux (L.6f), through Hyprland's own IPC.
//!
//! A chord is registered as a session-scoped Hyprland bind whose dispatcher
//! is `hl.dsp.event("isocast:<pid>:<generation>:<n>:d|u")`; Hyprland then
//! writes `custom>>isocast:...` on its event socket (`.socket2.sock`), where
//! one reader thread turns the lines into press/release edges. Nothing is
//! written to the user's configuration, and the binds die with the
//! compositor session, with the daemon (`clear`) or with a config reload
//! (the reader re-applies them).
//!
//! Design notes (the reasons are in docs/notes.md, L.6f):
//! - Keys are bound by physical position (`code:<evdev + 8>`), as the macOS
//!   back end does; the media keys have no position and bind by keysym.
//! - Binds are removed with `hl.unbind(<chord>)`, never `Keybind:remove()`:
//!   Hyprland 0.56.2 segfaults the compositor when `remove()` is called on
//!   a second bind that shares its key with another (a press/release pair).
//! - A release bind only fires while the modifiers held at that moment
//!   equal its own, so lifting a modifier first would lose the key-up (a
//!   push-to-talk key stuck on). Each chord therefore also gets
//!   non-consuming release binds for every smaller modifier subset; the
//!   reader forwards a release only for a key it saw pressed.
//! - Our binds carry `isocast <pid> <chord>` as their description, which
//!   is how conflicts with other daemons and binds left by a dead daemon
//!   are found (Hyprland lists `code:` binds with an empty key).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::Duration;

use isocast_engine::hotkeys::{Edge, GlobalHotkeys, Key, OnKey};
use isocast_protocol::hotkeys::{Phase, Registration};

const UNAVAILABLE: &str = "global registration is not available here; use focused input";
const DESCRIPTION: &str = "isocast";

/// The back end the daemon uses on Linux. All state is process-global: one
/// reader thread and one set of binds per daemon.
pub struct LinuxHotkeys;

impl GlobalHotkeys for LinuxHotkeys {
    fn replace(&self, keys: Vec<Key>, generation: u64, on_key: OnKey) -> Vec<Registration> {
        replace(keys, generation, on_key)
    }
}

/// Remove this process's binds (daemon shutdown). Best effort.
pub fn clear() {
    let mut state = lock();
    let chords = std::mem::take(&mut state.bound);
    state.entries.clear();
    state.down.clear();
    state.on_key = None;
    if chords.is_empty() {
        return;
    }
    if let Some(instance) = Instance::find()
        && let Err(message) = instance.dispatch(&unbind_lua(&chords))
    {
        eprintln!("isocastd hotkeys: removing binds at shutdown: {message}");
    }
}

#[derive(Default)]
struct State {
    generation: u64,
    on_key: Option<OnKey>,
    /// Event index -> binding id, for the binds currently installed.
    entries: HashMap<usize, String>,
    /// Every chord string bound (press, release and subset releases).
    bound: Vec<String>,
    /// Plan to re-apply on a config reload or a reconnect.
    plan: Vec<Planned>,
    /// Event indexes whose press was seen and whose release has not been.
    down: HashSet<usize>,
    reader: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn lock() -> StateGuard {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(State::default());
    }
    StateGuard(guard)
}

struct StateGuard(MutexGuard<'static, Option<State>>);

impl std::ops::Deref for StateGuard {
    type Target = State;
    fn deref(&self) -> &State {
        self.0.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for StateGuard {
    fn deref_mut(&mut self) -> &mut State {
        self.0.as_mut().unwrap()
    }
}

fn debug(message: std::fmt::Arguments<'_>) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ENABLED.get_or_init(|| std::env::var_os("ISOCAST_DEBUG_KEYS").is_some()) {
        eprintln!("isocastd hotkeys: {message}");
    }
}

// ---- Hyprland instance and IPC -----------------------------------------

struct Instance {
    signature: String,
    dir: PathBuf,
}

impl Instance {
    /// The running compositor: `HYPRLAND_INSTANCE_SIGNATURE` when its socket
    /// answers, otherwise the newest live instance under `$XDG_RUNTIME_DIR/hypr`
    /// (a daemon started from a systemd unit, or kept across a compositor
    /// restart, has no or a stale signature).
    fn find() -> Option<Self> {
        let base = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("hypr");
        let live = |name: &str| {
            let dir = base.join(name);
            UnixStream::connect(dir.join(".socket.sock"))
                .ok()
                .map(|_| Instance {
                    signature: name.to_string(),
                    dir,
                })
        };
        if let Some(signature) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .ok()
            .filter(|s| !s.is_empty() && !s.contains('/'))
            && let Some(instance) = live(&signature)
        {
            return Some(instance);
        }
        let mut dirs: Vec<_> = std::fs::read_dir(&base)
            .ok()?
            .flatten()
            .filter_map(|e| {
                let modified = std::fs::metadata(e.path().join(".socket.sock"))
                    .ok()?
                    .modified()
                    .ok()?;
                Some((modified, e.file_name().to_str()?.to_string()))
            })
            .collect();
        dirs.sort();
        dirs.iter().rev().find_map(|(_, name)| live(name))
    }

    /// Run one Lua expression through `hyprctl dispatch`. hyprctl wraps its
    /// argument as `return hl.dispatch(<expr>)`, so statements need an
    /// immediately invoked function.
    fn dispatch(&self, lua: &str) -> Result<(), String> {
        let output = Command::new("hyprctl")
            .env("HYPRLAND_INSTANCE_SIGNATURE", &self.signature)
            .arg("dispatch")
            .arg(lua)
            .output()
            .map_err(|e| format!("hyprctl: {e}"))?;
        let text = String::from_utf8_lossy(&output.stdout);
        if output.status.success() && text.trim() == "ok" {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "hyprctl dispatch: {} {}",
                text.trim(),
                stderr.trim()
            ))
        }
    }

    fn binds(&self) -> Result<Vec<Bind>, String> {
        let output = Command::new("hyprctl")
            .env("HYPRLAND_INSTANCE_SIGNATURE", &self.signature)
            .args(["binds", "-j"])
            .output()
            .map_err(|e| format!("hyprctl: {e}"))?;
        parse_binds(&output.stdout)
    }

    fn event_socket(&self) -> PathBuf {
        self.dir.join(".socket2.sock")
    }
}

#[derive(Debug, PartialEq)]
struct Bind {
    modmask: u32,
    key: String,
    description: String,
    release: bool,
    mouse: bool,
    submap: String,
    dispatcher: String,
}

fn parse_binds(json: &[u8]) -> Result<Vec<Bind>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| format!("hyprctl binds -j: {e}"))?;
    let list = value
        .as_array()
        .ok_or("hyprctl binds -j: not a list".to_string())?;
    Ok(list
        .iter()
        .map(|b| Bind {
            modmask: b["modmask"].as_u64().unwrap_or(0) as u32,
            key: b["key"].as_str().unwrap_or("").to_string(),
            description: b["description"].as_str().unwrap_or("").to_string(),
            release: b["release"].as_bool().unwrap_or(false),
            mouse: b["mouse"].as_bool().unwrap_or(false),
            submap: b["submap"].as_str().unwrap_or("").to_string(),
            dispatcher: b["dispatcher"].as_str().unwrap_or("").to_string(),
        })
        .collect())
}

// ---- Chord to Hyprland bind --------------------------------------------

/// Hyprland modifier bits: shift 1, ctrl 4, alt 8, super 64. Caps lock (2)
/// and num lock (16) never take part in a match.
fn hypr_modmask(modifiers: u32) -> u32 {
    [(1, 4), (2, 8), (4, 1), (8, 64)]
        .into_iter()
        .filter(|(bit, _)| modifiers & bit != 0)
        .map(|(_, mask)| mask)
        .sum()
}

fn modifier_names(modifiers: u32) -> Vec<&'static str> {
    [(1, "CTRL"), (2, "ALT"), (4, "SHIFT"), (8, "SUPER")]
        .into_iter()
        .filter(|(bit, _)| modifiers & bit != 0)
        .map(|(_, name)| name)
        .collect()
}

/// The Hyprland key string for a portable key name: `code:N` (physical US
/// position, evdev code + 8), or a keysym for the media keys.
fn key_string(key: &str) -> Option<String> {
    if let Some(sym) = media_keysym(key) {
        return Some(sym.to_string());
    }
    evdev_code(key).map(|code| format!("code:{}", code + 8))
}

fn media_keysym(key: &str) -> Option<&'static str> {
    Some(match key {
        "media_play_pause" => "XF86AudioPlay",
        "media_next" => "XF86AudioNext",
        "media_previous" => "XF86AudioPrev",
        "media_volume_up" => "XF86AudioRaiseVolume",
        "media_volume_down" => "XF86AudioLowerVolume",
        "media_mute" => "XF86AudioMute",
        _ => return None,
    })
}

/// Linux evdev code of the key in the physical US position.
fn evdev_code(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    if bytes.len() == 1 {
        let c = bytes[0];
        if c.is_ascii_lowercase() {
            // The three letter rows start at evdev KEY_Q, KEY_A and KEY_Z.
            for (rows, first) in [("qwertyuiop", 16), ("asdfghjkl", 30), ("zxcvbnm", 44)] {
                if let Some(i) = rows.find(c as char) {
                    return Some(first + i as u32);
                }
            }
            return None;
        }
        return match c {
            b'1'..=b'9' => Some(2 + (c - b'1') as u32),
            b'0' => Some(11),
            _ => None,
        };
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        return match n {
            1..=10 => Some(58 + n),
            11 => Some(87),
            12 => Some(88),
            13..=24 => Some(170 + n),
            _ => None,
        };
    }
    if let Some(n) = key
        .strip_prefix("numpad_")
        .and_then(|n| n.parse::<usize>().ok())
    {
        return [82, 79, 80, 81, 75, 76, 77, 71, 72, 73].get(n).copied();
    }
    Some(match key {
        "numpad_decimal" => 83,
        "numpad_multiply" => 55,
        "numpad_plus" => 78,
        "numpad_minus" => 74,
        "numpad_divide" => 98,
        "numpad_enter" => 96,
        "numpad_equal" => 117,
        "numpad_clear" => 69,
        "space" => 57,
        "return" => 28,
        "tab" => 15,
        "escape" => 1,
        "backspace" => 14,
        "delete" => 111,
        "left" => 105,
        "right" => 106,
        "up" => 103,
        "down" => 108,
        "home" => 102,
        "end" => 107,
        "pageup" => 104,
        "pagedown" => 109,
        "minus" => 12,
        "equal" => 13,
        "left_bracket" => 26,
        "right_bracket" => 27,
        "backslash" => 43,
        "semicolon" => 39,
        "quote" => 40,
        "comma" => 51,
        "period" => 52,
        "slash" => 53,
        "grave" => 41,
        _ => return None,
    })
}

/// The keysyms the US layout gives that position, lower-cased, for finding
/// binds the user wrote by keysym (`SUPER + Q`). Not exhaustive: a bind in
/// another layout's keysym, or one written as `code:N`, is not seen.
fn us_keysyms(key: &str) -> Vec<String> {
    let one = |s: &str| vec![s.to_ascii_lowercase()];
    if key.len() == 1 {
        return one(key);
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        let mut names = vec![format!("f{n}")];
        names.extend(
            match n {
                13 => vec!["xf86tools"],
                14..=18 => vec![
                    [
                        "xf86launch5",
                        "xf86launch6",
                        "xf86launch7",
                        "xf86launch8",
                        "xf86launch9",
                    ][n as usize - 14],
                ],
                20 => vec!["xf86audiomicmute"],
                _ => vec![],
            }
            .into_iter()
            .map(str::to_string),
        );
        return names;
    }
    if let Some(n) = key
        .strip_prefix("numpad_")
        .and_then(|n| n.parse::<usize>().ok())
    {
        let nav = [
            "kp_insert",
            "kp_end",
            "kp_down",
            "kp_next",
            "kp_left",
            "kp_begin",
            "kp_right",
            "kp_home",
            "kp_up",
            "kp_prior",
        ];
        return vec![
            format!("kp_{n}"),
            nav.get(n).copied().unwrap_or("").to_string(),
        ];
    }
    let names: &[&str] = match key {
        "numpad_decimal" => &["kp_decimal", "kp_delete"],
        "numpad_multiply" => &["kp_multiply"],
        "numpad_plus" => &["kp_add"],
        "numpad_minus" => &["kp_subtract"],
        "numpad_divide" => &["kp_divide"],
        "numpad_enter" => &["kp_enter"],
        "numpad_equal" => &["kp_equal"],
        "numpad_clear" => &["num_lock"],
        "space" => &["space"],
        "return" => &["return"],
        "tab" => &["tab", "iso_left_tab"],
        "escape" => &["escape"],
        "backspace" => &["backspace"],
        "delete" => &["delete"],
        "left" | "right" | "up" | "down" | "home" | "end" => &[],
        "pageup" => &["prior", "page_up"],
        "pagedown" => &["next", "page_down"],
        "minus" => &["minus"],
        "equal" => &["equal"],
        "left_bracket" => &["bracketleft"],
        "right_bracket" => &["bracketright"],
        "backslash" => &["backslash"],
        "semicolon" => &["semicolon"],
        "quote" => &["apostrophe"],
        "comma" => &["comma"],
        "period" => &["period"],
        "slash" => &["slash"],
        "grave" => &["grave"],
        _ => &[],
    };
    let mut names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    if names.is_empty() {
        names.push(key.to_string());
    }
    if let Some(sym) = media_keysym(key) {
        names.push(sym.to_ascii_lowercase());
    }
    names
}

/// `CTRL + ALT + code:52`, or just the key when there are no modifiers.
fn chord_string(modifiers: u32, key: &str) -> String {
    let mut parts: Vec<String> = modifier_names(modifiers)
        .into_iter()
        .map(str::to_string)
        .collect();
    parts.push(key.to_string());
    parts.join(" + ")
}

/// Every modifier subset of the chord, the full set first.
fn modifier_subsets(modifiers: u32) -> Vec<u32> {
    let mut subsets: Vec<u32> = (0..16).filter(|m| m & !modifiers == 0).collect();
    subsets.sort_by_key(|m| std::cmp::Reverse(m.count_ones()));
    subsets
}

// ---- Lua ---------------------------------------------------------------

fn lua_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn unbind_lua(chords: &[String]) -> String {
    let mut lua = String::from("(function() ");
    for chord in chords {
        lua.push_str(&format!("hl.unbind({}); ", lua_string(chord)));
    }
    lua.push_str("return hl.dsp.no_op() end)()");
    lua
}

fn description(pid: u32, chord: &str) -> String {
    format!("{DESCRIPTION} {pid} {chord}")
}

/// `isocast <pid> <chord>` back to its parts.
fn parse_description(text: &str) -> Option<(u32, &str)> {
    let mut parts = text.splitn(3, ' ');
    if parts.next()? != DESCRIPTION {
        return None;
    }
    let pid = parts.next()?.parse().ok()?;
    Some((pid, parts.next()?))
}

/// One key of the plan: its press chord, its release chords and the event
/// index the daemon's binding is looked up by.
#[derive(Clone, Debug, PartialEq)]
struct Planned {
    n: usize,
    binding: String,
    modifiers: u32,
    key: String,
}

impl Planned {
    fn press_chord(&self) -> String {
        chord_string(self.modifiers, &self.key)
    }

    /// Release binds: (chord, consuming). The full chord consumes, as the
    /// press does, so a program never sees a key-up for a key it did not see
    /// go down; the smaller subsets (a modifier let go first) do not.
    fn release_chords(&self) -> Vec<(String, bool)> {
        modifier_subsets(self.modifiers)
            .into_iter()
            .map(|m| (chord_string(m, &self.key), m == self.modifiers))
            .collect()
    }

    fn chords(&self) -> Vec<String> {
        let mut chords = vec![self.press_chord()];
        chords.extend(self.release_chords().into_iter().map(|(c, _)| c));
        chords.sort();
        chords.dedup();
        chords
    }
}

fn event_name(pid: u32, generation: u64, n: usize, phase: char) -> String {
    format!("isocast:{pid}:{generation}:{n}:{phase}")
}

fn install_lua(pid: u32, generation: u64, remove: &[String], plan: &[Planned]) -> String {
    let mut lua = String::from("(function() ");
    for chord in remove {
        lua.push_str(&format!("hl.unbind({}); ", lua_string(chord)));
    }
    for p in plan {
        let press = p.press_chord();
        lua.push_str(&format!(
            "hl.bind({}, hl.dsp.event({}), {{description={}}}); ",
            lua_string(&press),
            lua_string(&event_name(pid, generation, p.n, 'd')),
            lua_string(&description(pid, &press)),
        ));
        for (chord, consuming) in p.release_chords() {
            lua.push_str(&format!(
                "hl.bind({}, hl.dsp.event({}), {{description={}, release=true{}}}); ",
                lua_string(&chord),
                lua_string(&event_name(pid, generation, p.n, 'u')),
                lua_string(&description(pid, &chord)),
                if consuming {
                    ""
                } else {
                    ", non_consuming=true"
                },
            ));
        }
    }
    lua.push_str("return hl.dsp.no_op() end)()");
    lua
}

/// `isocast:<pid>:<generation>:<n>:d|u`, as it arrives on the event socket.
#[derive(Debug, PartialEq)]
struct Fired {
    generation: u64,
    n: usize,
    phase: Phase,
}

fn parse_event(line: &str, pid: u32) -> Option<Fired> {
    let rest = line.strip_prefix("custom>>isocast:")?;
    let mut parts = rest.trim_end().split(':');
    if parts.next()?.parse::<u32>().ok()? != pid {
        return None;
    }
    let generation = parts.next()?.parse().ok()?;
    let n = parts.next()?.parse().ok()?;
    let phase = match parts.next()? {
        "d" => Phase::Press,
        "u" => Phase::Release,
        _ => return None,
    };
    parts.next().is_none().then_some(Fired {
        generation,
        n,
        phase,
    })
}

// ---- Registration ------------------------------------------------------

fn pid_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

/// Who already has this chord: a user bind, or another daemon's.
fn conflict(binds: &[Bind], modifiers: u32, key: &str, press_chord: &str) -> Option<String> {
    let mask = hypr_modmask(modifiers);
    let names = us_keysyms(key);
    for b in binds {
        if let Some((pid, chord)) = parse_description(&b.description) {
            if chord == press_chord && !b.release {
                return Some(format!(
                    "chord already bound by another isocast daemon (pid {pid})"
                ));
            }
            continue;
        }
        if b.release || b.mouse || !b.submap.is_empty() || b.key.is_empty() {
            continue;
        }
        if b.modmask & !(2 | 16) == mask && names.iter().any(|n| n.eq_ignore_ascii_case(&b.key)) {
            let what = if b.description.is_empty() {
                b.dispatcher.clone()
            } else {
                b.description.clone()
            };
            return Some(format!("chord already bound in Hyprland ({what})"));
        }
    }
    None
}

fn replace(keys: Vec<Key>, generation: u64, on_key: OnKey) -> Vec<Registration> {
    let refused = |keys: &[Key], warning: &str| -> Vec<Registration> {
        keys.iter()
            .map(|k| Registration {
                binding: k.binding.clone(),
                global: false,
                warning: Some(warning.to_string()),
            })
            .collect()
    };
    let mut state = lock();
    let Some(instance) = Instance::find() else {
        state.bound.clear();
        state.entries.clear();
        state.plan.clear();
        return refused(&keys, UNAVAILABLE);
    };
    state.generation = generation;
    state.on_key = Some(on_key);
    state.down.clear();
    state.entries.clear();
    state.plan.clear();
    if !state.reader {
        state.reader = true;
        drop(state);
        start_reader();
        state = lock();
    }
    let binds = match instance.binds() {
        Ok(binds) => binds,
        Err(message) => {
            return refused(&keys, &format!("{UNAVAILABLE} ({message})"));
        }
    };
    let pid = std::process::id();
    // Our own earlier binds and those of daemons that died: removed in the
    // same dispatch that installs the new ones.
    let mut remove: Vec<String> = Vec::new();
    let mut others: Vec<Bind> = Vec::new();
    for b in binds {
        match parse_description(&b.description) {
            Some((owner, chord)) if owner == pid || !pid_alive(owner) => {
                remove.push(chord.to_string());
            }
            _ => others.push(b),
        }
    }
    let mut results: Vec<Result<Planned, String>> = Vec::new();
    let mut planned = Vec::new();
    for key in &keys {
        let Some(sym) = key_string(&key.chord.key) else {
            results.push(Err(format!(
                "key {:?} has no Linux key mapping; use focused input",
                key.chord.key
            )));
            continue;
        };
        let press_chord = chord_string(key.chord.modifiers, &sym);
        if let Some(warning) = conflict(&others, key.chord.modifiers, &key.chord.key, &press_chord)
        {
            results.push(Err(warning));
            continue;
        }
        let p = Planned {
            n: planned.len(),
            binding: key.binding.clone(),
            modifiers: key.chord.modifiers,
            key: sym,
        };
        planned.push(p.clone());
        results.push(Ok(p));
    }
    remove.sort();
    remove.dedup();
    let lua = install_lua(pid, generation, &remove, &planned);
    if let Err(message) = instance.dispatch(&lua) {
        state.bound.clear();
        return refused(&keys, &format!("{UNAVAILABLE} ({message})"));
    }
    // Hyprland answers "ok" even for a bind it could not parse; read back.
    let present: HashSet<String> = instance
        .binds()
        .unwrap_or_default()
        .into_iter()
        .map(|b| b.description)
        .collect();
    let mut accepted = Vec::new();
    let mut failed = Vec::new();
    for p in planned {
        if present.contains(&description(pid, &p.press_chord())) {
            accepted.push(p);
        } else {
            failed.push(p);
        }
    }
    if !failed.is_empty() {
        let chords: Vec<String> = failed.iter().flat_map(Planned::chords).collect();
        let _ = instance.dispatch(&unbind_lua(&chords));
    }
    state.bound = accepted.iter().flat_map(Planned::chords).collect();
    state.bound.sort();
    state.bound.dedup();
    state.entries = accepted.iter().map(|p| (p.n, p.binding.clone())).collect();
    state.plan = accepted;
    for (n, binding) in &state.entries {
        debug(format_args!(
            "generation {generation}: n={n} binding={binding}"
        ));
    }
    keys.iter()
        .zip(results)
        .map(|(key, result)| {
            let warning = match result {
                Err(warning) => Some(warning),
                Ok(p) if failed.iter().any(|f| f.n == p.n) => {
                    Some("Hyprland did not accept this bind; see `hyprctl binds`".to_string())
                }
                Ok(_) => None,
            };
            Registration {
                binding: key.binding.clone(),
                global: warning.is_none(),
                warning,
            }
        })
        .collect()
}

/// Install the current plan again, after a config reload or a reconnect
/// (a reload drops dynamic binds). No conflict check: the chords were ours.
fn reapply(reason: &str) {
    let state = lock();
    if state.plan.is_empty() {
        return;
    }
    let Some(instance) = Instance::find() else {
        return;
    };
    let lua = install_lua(
        std::process::id(),
        state.generation,
        &state.bound,
        &state.plan,
    );
    match instance.dispatch(&lua) {
        Ok(()) => debug(format_args!("binds installed again ({reason})")),
        Err(message) => eprintln!("isocastd hotkeys: installing binds again ({reason}): {message}"),
    }
}

// ---- Event reader ------------------------------------------------------

fn start_reader() {
    let (ready, wait) = mpsc::sync_channel::<()>(1);
    std::thread::spawn(move || {
        let pid = std::process::id();
        let mut backoff = Duration::from_millis(500);
        let mut ready = Some(ready);
        let mut connected_before = false;
        loop {
            let stream = Instance::find().and_then(|i| UnixStream::connect(i.event_socket()).ok());
            let Some(stream) = stream else {
                if let Some(ready) = ready.take() {
                    let _ = ready.send(());
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            };
            backoff = Duration::from_millis(500);
            if let Some(ready) = ready.take() {
                let _ = ready.send(());
            }
            if connected_before {
                eprintln!("isocastd hotkeys: Hyprland event socket reconnected");
                reapply("reconnect");
            }
            connected_before = true;
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else {
                    break;
                };
                if line.starts_with("configreloaded>>") {
                    reapply("config reload");
                } else if let Some(fired) = parse_event(&line, pid) {
                    deliver(fired);
                }
            }
            eprintln!("isocastd hotkeys: Hyprland event socket closed; reconnecting");
        }
    });
    let _ = wait.recv_timeout(Duration::from_secs(2));
}

fn deliver(fired: Fired) {
    let (edge, on_key) = {
        let mut state = lock();
        if state.generation != fired.generation {
            return;
        }
        let Some(binding) = state.entries.get(&fired.n).cloned() else {
            return;
        };
        let forward = match fired.phase {
            Phase::Press => state.down.insert(fired.n),
            // Releases are also sent for every smaller modifier set, so a key
            // typed on its own produces them: only a pressed key releases.
            Phase::Release => state.down.remove(&fired.n),
        };
        let Some(on_key) = state.on_key.clone().filter(|_| forward) else {
            return;
        };
        (
            Edge {
                binding,
                generation: fired.generation,
                phase: fired.phase,
            },
            on_key,
        )
    };
    on_key(edge);
}

#[cfg(test)]
mod tests {
    use super::*;
    use isocast_protocol::hotkeys::{Chord, key_names};

    #[test]
    fn every_portable_key_maps_to_a_hyprland_key() {
        for name in key_names() {
            assert!(key_string(&name).is_some(), "no mapping for {name}");
            assert!(!us_keysyms(&name).is_empty(), "no keysyms for {name}");
        }
    }

    #[test]
    fn physical_positions_are_evdev_plus_eight() {
        assert_eq!(key_string("z").as_deref(), Some("code:52"));
        assert_eq!(key_string("a").as_deref(), Some("code:38"));
        assert_eq!(key_string("q").as_deref(), Some("code:24"));
        assert_eq!(key_string("1").as_deref(), Some("code:10"));
        assert_eq!(key_string("0").as_deref(), Some("code:19"));
        assert_eq!(key_string("f1").as_deref(), Some("code:67"));
        assert_eq!(key_string("f12").as_deref(), Some("code:96"));
        assert_eq!(key_string("f13").as_deref(), Some("code:191"));
        assert_eq!(key_string("f20").as_deref(), Some("code:198"));
        assert_eq!(key_string("numpad_0").as_deref(), Some("code:90"));
        assert_eq!(key_string("space").as_deref(), Some("code:65"));
        assert_eq!(key_string("media_mute").as_deref(), Some("XF86AudioMute"));
    }

    #[test]
    fn letters_are_distinct_positions() {
        let mut codes: Vec<u32> = ('a'..='z')
            .map(|c| evdev_code(&c.to_string()).unwrap())
            .collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), 26);
    }

    #[test]
    fn chords_use_hyprland_names() {
        assert_eq!(chord_string(1 | 2, "code:52"), "CTRL + ALT + code:52");
        assert_eq!(chord_string(8 | 4, "code:52"), "SHIFT + SUPER + code:52");
        assert_eq!(chord_string(0, "XF86AudioMute"), "XF86AudioMute");
        assert_eq!(hypr_modmask(1 | 2), 12);
        assert_eq!(hypr_modmask(8), 64);
        assert_eq!(hypr_modmask(4), 1);
    }

    #[test]
    fn release_subsets_cover_every_lift_order() {
        let chord = Chord::parse("ctrl+alt+z").unwrap();
        let p = Planned {
            n: 0,
            binding: "b".into(),
            modifiers: chord.modifiers,
            key: "code:52".into(),
        };
        assert_eq!(
            p.release_chords(),
            vec![
                ("CTRL + ALT + code:52".to_string(), true),
                ("CTRL + code:52".to_string(), false),
                ("ALT + code:52".to_string(), false),
                ("code:52".to_string(), false),
            ]
        );
        assert_eq!(p.chords().len(), 4);
    }

    #[test]
    fn events_round_trip() {
        let line = format!("custom>>{}", event_name(42, 7, 3, 'd'));
        assert_eq!(
            parse_event(&line, 42),
            Some(Fired {
                generation: 7,
                n: 3,
                phase: Phase::Press
            })
        );
        assert_eq!(
            parse_event("custom>>isocast:42:7:3:u", 42).map(|f| f.phase),
            Some(Phase::Release)
        );
        // Another daemon's token, other events, malformed lines.
        assert_eq!(parse_event("custom>>isocast:43:7:3:d", 42), None);
        assert_eq!(parse_event("custom>>other:42:7:3:d", 42), None);
        assert_eq!(parse_event("configreloaded>>", 42), None);
        assert_eq!(parse_event("custom>>isocast:42:7:x:d", 42), None);
        assert_eq!(parse_event("custom>>isocast:42:7:3:d:extra", 42), None);
    }

    #[test]
    fn descriptions_round_trip() {
        let text = description(9, "CTRL + ALT + code:52");
        assert_eq!(parse_description(&text), Some((9, "CTRL + ALT + code:52")));
        assert_eq!(parse_description("Volume up"), None);
        assert_eq!(parse_description(""), None);
    }

    #[test]
    fn install_script_binds_press_and_every_release() {
        let p = Planned {
            n: 2,
            binding: "b".into(),
            modifiers: 1,
            key: "code:52".into(),
        };
        let lua = install_lua(5, 3, &["OLD + code:1".to_string()], &[p]);
        assert!(lua.starts_with("(function() hl.unbind(\"OLD + code:1\"); "));
        assert!(lua.ends_with("return hl.dsp.no_op() end)()"));
        assert!(lua.contains(
            "hl.bind(\"CTRL + code:52\", hl.dsp.event(\"isocast:5:3:2:d\"), {description=\"isocast 5 CTRL + code:52\"});"
        ));
        assert!(lua.contains(
            "hl.bind(\"CTRL + code:52\", hl.dsp.event(\"isocast:5:3:2:u\"), {description=\"isocast 5 CTRL + code:52\", release=true});"
        ));
        assert!(lua.contains(
            "hl.bind(\"code:52\", hl.dsp.event(\"isocast:5:3:2:u\"), {description=\"isocast 5 code:52\", release=true, non_consuming=true});"
        ));
    }

    #[test]
    fn lua_strings_are_escaped() {
        assert_eq!(lua_string("a\"b\\c\nd"), "\"a\\\"b\\\\c d\"");
    }

    fn user_bind(modmask: u32, key: &str, description: &str) -> Bind {
        Bind {
            modmask,
            key: key.into(),
            description: description.into(),
            release: false,
            mouse: false,
            submap: String::new(),
            dispatcher: "__lua".into(),
        }
    }

    #[test]
    fn conflicts_name_the_existing_bind() {
        let binds = vec![
            user_bind(64, "Q", "Close window"),
            user_bind(0, "XF86AudioPlay", "Play"),
            user_bind(64 | 2, "z", "caps lock held"),
            user_bind(0, "", "a code: bind"),
            user_bind(12, "", &description(77, "CTRL + ALT + code:52")),
        ];
        let q = conflict(&binds, 8, "q", "SUPER + code:24");
        assert_eq!(
            q.as_deref(),
            Some("chord already bound in Hyprland (Close window)")
        );
        assert!(conflict(&binds, 8, "w", "SUPER + code:25").is_none());
        // Different modifiers are a different chord.
        assert!(conflict(&binds, 1 | 8, "q", "CTRL + SUPER + code:24").is_none());
        let play = conflict(&binds, 0, "media_play_pause", "XF86AudioPlay");
        assert_eq!(
            play.as_deref(),
            Some("chord already bound in Hyprland (Play)")
        );
        // Caps lock in the user's mask does not change the chord.
        assert!(conflict(&binds, 8, "z", "SUPER + code:52").is_some());
        let other = conflict(&binds, 1 | 2, "z", "CTRL + ALT + code:52");
        assert_eq!(
            other.as_deref(),
            Some("chord already bound by another isocast daemon (pid 77)")
        );
    }

    #[test]
    fn binds_json_is_read() {
        let json = br#"[{"modmask":12,"key":"","description":"isocast 1 x","release":true,
            "mouse":false,"submap":"","dispatcher":"__lua"},{"modmask":64,"key":"Q"}]"#;
        let binds = parse_binds(json).unwrap();
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0].modmask, 12);
        assert!(binds[0].release);
        assert_eq!(binds[1].key, "Q");
        assert!(parse_binds(b"not json").is_err());
    }
}
