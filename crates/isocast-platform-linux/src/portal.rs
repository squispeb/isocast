//! The desktop's ScreenCast portal: the picker, the PipeWire node, and the
//! restore token that remembers the choice.
//!
//! Wayland hands no program the screen directly. The compositor's portal
//! asks the user once, answers with a PipeWire node holding the screen's
//! pictures and (with `persist_mode` 2) a restore token; presenting that
//! token later skips the picker and restores the same monitor. The token
//! captures the screen without a prompt, so it is kept where the secrets
//! are (`keychain`, a 0600 file beside the socket).
//!
//! The portal decides *which* monitor: a client asks for "a monitor" and
//! the picker chooses. The first screen source on a machine therefore
//! shows the picker, and every screen source afterwards follows that
//! choice (per-source choices are a follow-up; see docs/notes.md).

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use zbus::blocking::proxy::SignalIterator;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{Array, ObjectPath, OwnedFd as PortalFd, OwnedValue, Structure, Value};

use crate::keychain;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";

/// The secrets entry the restore token is kept under.
const TOKEN: (&str, &str) = ("portal", "screen-cast");

/// A monitor the desktop is sharing.
pub(crate) struct Stream {
    /// The PipeWire remote the stream's node lives on; connecting to this
    /// fd is what makes the node reachable.
    pub fd: OwnedFd,
    /// The node holding the screen's pictures.
    pub node: u32,
    /// The client connection the session lives on, held for as long as
    /// the stream is: xdg-desktop-portal closes a session as soon as the
    /// bus connection that made it goes away, and the compositor then
    /// stops sharing (its node goes with it). One connection per capture
    /// is what makes the session's life exactly the capture's.
    _session: Connection,
}

/// Start sharing a monitor: the picker the first time on this machine,
/// the choice the token remembers afterwards. A source's open asks the
/// desktop at most once on its own: after a picker that failed (closed,
/// crashed, refused) it waits with that said, until someone asks again
/// (`request_permissions`).
pub(crate) fn open(cursor: bool) -> Result<Stream> {
    if !has_token() && refused().is_some() {
        // `source_permission` says what happened and what to do.
        bail!("not asking the desktop again by itself after its picker failed");
    }
    let _turn = Turn::take(SOURCE_WAIT)?;
    let mut portal = Portal::connect()?;
    let (session, node) = portal.share(cursor)?;
    let fd = portal.pipe_wire_remote(&session)?;
    Ok(Stream {
        fd,
        node,
        _session: portal.conn,
    })
}

/// Whether this machine has answered the picker before: with a token a
/// capture needs no prompt.
pub(crate) fn has_token() -> bool {
    matches!(keychain::get(TOKEN.0, TOKEN.1), Ok(Some(token)) if !token.is_empty())
}

/// Answer the picker now, and keep the token: a take's own open is
/// bounded to 8 s by the engine, which is not long enough to ask a person
/// to choose a screen. Used by `request_permissions`; the session is
/// closed again, since this only wants the token. Asking is what clears a
/// failed picker, so sources may open again.
pub(crate) fn prepare(cursor: bool) -> Result<()> {
    let _turn = Turn::take(REQUEST_WAIT)?;
    let mut portal = Portal::connect()?;
    let (session, _node) = portal.share(cursor)?;
    portal.close(&session)
}

/// Forget the remembered choice (and a failed picker): the next screen
/// capture, or `request_permissions`, shows the picker again. Returns
/// whether there was anything to forget.
pub(crate) fn reset() -> Result<bool> {
    let _turn = Turn::take(SOURCE_WAIT)?;
    let token = keychain::remove(TOKEN.0, TOKEN.1).context("forgetting the screen choice")?;
    let failed = keychain::remove(REFUSED.0, REFUSED.1).context("forgetting the screen choice")?;
    Ok(token || failed)
}

/// The secrets entry a failed picker is kept under, as "<unix seconds>
/// <what happened>": every Isocast process sharing this secrets file then
/// waits instead of asking again.
const REFUSED: (&str, &str) = ("portal", "screen-cast-refused");
/// How long a source's open waits for another share in progress (its
/// own open is bounded to 8 s by the engine).
const SOURCE_WAIT: Duration = Duration::from_secs(3);
/// How long `request_permissions` waits for one: a picker open for
/// another share is the same picker, and answering it answers both.
const REQUEST_WAIT: Duration = Duration::from_secs(120);

/// A picker that did not end in a choice.
pub(crate) struct Refused {
    /// Unix seconds.
    pub at: u64,
    pub why: String,
}

impl Refused {
    /// What a screen source that will not ask again says.
    pub(crate) fn waiting(&self) -> String {
        format!(
            "the desktop's screen-sharing picker did not give a screen at {} ({}); screen \
             sources wait instead of asking again by themselves: click Allow… in the app's \
             permissions or run `isocast permissions --request screen_recording`",
            local_time(self.at),
            self.why
        )
    }
}

/// The failed picker remembered, if the last one failed.
pub(crate) fn refused() -> Option<Refused> {
    let text = keychain::get(REFUSED.0, REFUSED.1).ok().flatten()?;
    let (at, why) = text.split_once(' ')?;
    Some(Refused {
        at: at.parse().ok()?,
        why: why.to_string(),
    })
}

/// One share at a time across every Isocast process of this user: a
/// restore token is good for one `Start`, so two shares presenting it at
/// once make the second prompt; and a picker waiting for a choice must not
/// be joined by a second one. Held from reading the token to keeping the
/// new one. The lock is an flock, so a process that dies frees it.
struct Turn(#[allow(dead_code)] std::fs::File);

impl Turn {
    fn take(wait: Duration) -> Result<Self> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = dir.join("isocast-screen-cast.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let deadline = Instant::now() + wait;
        loop {
            // SAFETY: an open descriptor; flock only reads it.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self(file));
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(err).with_context(|| format!("locking {}", path.display()));
            }
            if Instant::now() >= deadline {
                bail!(
                    "another request to share a screen is still waiting for an answer (the \
                     desktop's picker may be open, or another Isocast process is asking); \
                     answer it, then try again (a screen source opens with that choice by \
                     itself)"
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// What the remembered choice shares, from the portal's own record of the
/// token (xdg-desktop-portal's permission store; the backend's data), in
/// words: "DP-3 (the whole screen)". `None` when there is no token or the
/// record says nothing this knows how to read.
pub(crate) fn choice() -> Option<String> {
    let token = keychain::get(TOKEN.0, TOKEN.1).ok().flatten()?;
    let conn = Connection::session().ok()?;
    let data = restore_data(&conn, &token)?;
    describe_choice(&data)
}

/// The portal's record of a restore token: the backend's restore data.
fn restore_data(conn: &Connection, token: &str) -> Option<OwnedValue> {
    let reply = conn
        .call_method(
            Some(PERMISSION_STORE),
            PERMISSION_STORE_PATH,
            Some(PERMISSION_STORE),
            "Lookup",
            &(SCREENCAST_TABLE, token),
        )
        .ok()?;
    let body = reply.body();
    let (_, data): (HashMap<String, Vec<String>>, OwnedValue) = body.deserialize().ok()?;
    Some(data)
}

const PERMISSION_STORE: &str = "org.freedesktop.impl.portal.PermissionStore";
const PERMISSION_STORE_PATH: &str = "/org/freedesktop/impl/portal/PermissionStore";
const SCREENCAST_TABLE: &str = "screencast";

/// Share a remembered "region" that is the whole of its output as the
/// output itself. xdg-desktop-portal-hyprland restores a choice that has a
/// `geometry` as a region capture (wlr-screencopy capture_output_region),
/// and Hyprland 0.56 keeps one capture session per region, matching the
/// box asked for (layout coordinates) against the one it stored (pixels):
/// on a scaled monitor nothing matches, so every frame of the share leaves
/// a session behind that the compositor walks on every frame it draws,
/// until it restarts (docs/notes.md, 2026-10-24). The picker records "the
/// whole screen" that way. Without the geometry the same share is an
/// output capture, which keeps one session. Runs inside a `Turn`, before
/// the token is presented; returns whether the record was changed.
fn share_whole_output(conn: &Connection, token: &str) -> Result<bool> {
    let Some(data) = restore_data(conn, token) else {
        return Ok(false);
    };
    let Some(record) = HyprlandRecord::read(&data) else {
        return Ok(false);
    };
    let Some((x, y, w, h)) = record.region() else {
        return Ok(false);
    };
    let Some(output) = record.text("output") else {
        return Ok(false);
    };
    let monitor = crate::hyprland::monitors()
        .unwrap_or_default()
        .into_iter()
        .find(|m| m.name == output);
    if (x, y) != (0, 0) || !monitor.as_ref().is_some_and(|m| covers(m, w, h)) {
        if monitor.is_some_and(|m| m.scale != 1.0) {
            isocast_diagnostics::diagnostic!(
                "warn";
                "screen sharing: the remembered choice is a {w}x{h} region of {output}, which is \
                 scaled; Hyprland 0.56 slows down with every frame of a region share on a scaled \
                 monitor until it restarts. Choose the whole screen instead (Choose again… in \
                 the app, or `isocast permissions --reset` then `--request screen_recording`)"
            );
        }
        return Ok(false);
    }
    let rewritten = record.without_geometry()?;
    conn.call_method(
        Some(PERMISSION_STORE),
        PERMISSION_STORE_PATH,
        Some(PERMISSION_STORE),
        "SetValue",
        &(SCREENCAST_TABLE, false, token, rewritten),
    )
    .context("rewriting the remembered screen choice in the portal's permission store")?;
    Ok(true)
}

/// Whether a `w`x`h` box in layout coordinates is all of the monitor.
fn covers(monitor: &crate::hyprland::Monitor, w: u32, h: u32) -> bool {
    let logical = |pixels: u32| (f64::from(pixels) / monitor.scale).round() as u32;
    logical(monitor.width).abs_diff(w) <= 1 && logical(monitor.height).abs_diff(h) <= 1
}

/// xdg-desktop-portal-hyprland's restore data: `(backend, version, a{sv})`
/// with the output, a window, or a region of an output (`geometry`, in
/// layout coordinates).
struct HyprlandRecord<'a> {
    fields: &'a [Value<'a>],
    details: &'a zbus::zvariant::Dict<'a, 'a>,
}

impl<'a> HyprlandRecord<'a> {
    fn read(data: &'a Value<'a>) -> Option<Self> {
        let Value::Structure(record) = unwrap(data) else {
            return None;
        };
        let fields = record.fields();
        let Value::Dict(details) = unwrap(fields.get(2)?) else {
            return None;
        };
        (fields.len() == 3).then_some(Self { fields, details })
    }

    /// The same record, `(su a{sv})` as the backend wrote it, without its
    /// `geometry`.
    fn without_geometry(&self) -> Result<Value<'static>> {
        let (Value::Str(backend), Value::U32(version)) =
            (unwrap(&self.fields[0]), unwrap(&self.fields[1]))
        else {
            bail!("the portal's record of the screen choice is not (backend, version, details)");
        };
        let mut details: HashMap<String, Value<'static>> = HashMap::new();
        for (name, value) in self.details.iter() {
            if let Value::Str(name) = name
                && name.as_str() != "geometry"
            {
                details.insert(name.to_string(), unwrap(value).try_to_owned()?.into());
            }
        }
        Ok(Value::from(Structure::from((
            backend.to_string(),
            *version,
            Value::from(details),
        ))))
    }

    fn get(&self, key: &str) -> Option<&'a Value<'a>> {
        self.details
            .iter()
            .find(|(name, _)| matches!(name, Value::Str(name) if name.as_str() == key))
            .map(|(_, value)| unwrap(value))
    }

    fn text(&self, key: &str) -> Option<String> {
        match self.get(key) {
            Some(Value::Str(text)) if !text.is_empty() => Some(text.to_string()),
            _ => None,
        }
    }

    fn region(&self) -> Option<(u32, u32, u32, u32)> {
        let Some(Value::Structure(geometry)) = self.get("geometry") else {
            return None;
        };
        let numbers: Vec<u32> = geometry
            .fields()
            .iter()
            .filter_map(|field| match field {
                Value::U32(n) => Some(*n),
                _ => None,
            })
            .collect();
        match numbers[..] {
            [x, y, w, h] => Some((x, y, w, h)),
            _ => None,
        }
    }
}

/// A variant's content (the store wraps values in variants, sometimes twice).
fn unwrap<'a>(value: &'a Value<'a>) -> &'a Value<'a> {
    match value {
        Value::Value(inner) => unwrap(inner),
        other => other,
    }
}

fn describe_choice(data: &Value<'_>) -> Option<String> {
    let record = HyprlandRecord::read(data)?;
    let cursor = match record.get("withCursor") {
        Some(Value::U32(2)) => ", with the cursor",
        Some(Value::U32(_)) => ", without the cursor",
        _ => "",
    };
    let chosen = match record.get("timeIssued") {
        Some(Value::U64(at)) => format!(", chosen {}", local_time(*at)),
        _ => String::new(),
    };
    if let Some(class) = record.text("windowClass") {
        return Some(format!("a window of {class}{cursor}{chosen}"));
    }
    let output = record.text("output")?;
    let whole = |w: u32, h: u32| {
        crate::hyprland::monitors()
            .unwrap_or_default()
            .iter()
            .find(|m| m.name == output)
            .is_some_and(|m| covers(m, w, h))
    };
    Some(match record.region() {
        Some((x, y, w, h)) if (x, y) != (0, 0) || !whole(w, h) => {
            format!("a {w}x{h} region of {output} at {x},{y}{cursor}{chosen}")
        }
        _ => format!("{output} (the whole screen){cursor}{chosen}"),
    })
}

/// Unix seconds as local "YYYY-MM-DD HH:MM".
fn local_time(at: u64) -> String {
    let time = at as libc::time_t;
    // SAFETY: localtime_r writes the struct it is given.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return format!("{at} (unix time)");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// A session with the desktop's portal, and the bus name its request
/// paths are built from.
struct Portal {
    conn: Connection,
    /// This process's bus name without the punctuation an object path
    /// cannot carry (`:1.42` → `1_42`), as the portal's own protocol
    /// prescribes for the paths a caller predicts.
    sender: String,
    /// Distinguishes this run's requests from each other.
    next: u32,
}

impl Portal {
    fn connect() -> Result<Self> {
        let conn = Connection::session()
            .context("connecting to the session bus (is a desktop session running?)")?;
        let unique = conn
            .unique_name()
            .context("the session bus gave this process no name")?
            .to_string();
        let sender = unique.trim_start_matches(':').replace('.', "_");
        Ok(Self {
            conn,
            sender,
            next: 0,
        })
    }

    /// Create a session, select a monitor, start sharing it, and keep the
    /// token that remembers the choice. Returns the session and the
    /// node's id. Runs inside a `Turn`. A picker that ends without a
    /// choice is remembered (`refused`); one that ends in a choice clears
    /// that. A presented token that ended in a picker no longer restores
    /// anything, so it is forgotten.
    fn share(&mut self, cursor: bool) -> Result<(ObjectPath<'static>, u32)> {
        let token = keychain::get(TOKEN.0, TOKEN.1)
            .ok()
            .flatten()
            .filter(|token| !token.is_empty());
        if let Some(token) = token.as_deref() {
            match share_whole_output(&self.conn, token) {
                Ok(true) => isocast_diagnostics::diagnostic!(
                    "info";
                    "screen sharing: the remembered whole-screen region is now shared as the \
                     output (a region share costs Hyprland 0.56 a session every frame on a \
                     scaled monitor)"
                ),
                Ok(false) => {}
                Err(err) => isocast_diagnostics::diagnostic!("warn"; "screen sharing: {err:#}"),
            }
        }
        let session = self.new_session()?;
        let shared = self
            .select_sources(&session, cursor, token.as_deref())
            .and_then(|()| self.start(&session, token.is_some()));
        match &shared {
            Ok(_) => {
                let _ = keychain::remove(REFUSED.0, REFUSED.1);
            }
            Err(err) => {
                if let Some(unanswered) = err.downcast_ref::<Unanswered>() {
                    let why = if token.is_some() {
                        format!(
                            "{unanswered}, after the remembered choice did not restore (the \
                             desktop showed its picker instead)"
                        )
                    } else {
                        unanswered.to_string()
                    };
                    if token.is_some() {
                        let _ = keychain::remove(TOKEN.0, TOKEN.1);
                    }
                    if let Err(err) =
                        keychain::set(REFUSED.0, REFUSED.1, &format!("{} {why}", now()))
                    {
                        isocast_diagnostics::diagnostic!(
                            "warn"; "the failed screen-sharing picker could not be remembered: {err:#}"
                        );
                    }
                    isocast_diagnostics::diagnostic!(
                        "warn"; "screen sharing: {why}; screen sources wait until asked again"
                    );
                }
            }
        }
        let node = shared?;
        Ok((session, node))
    }

    /// `CreateSession`: no picker, and the only call a fresh machine can
    /// make without a person in front of it.
    fn new_session(&mut self) -> Result<ObjectPath<'static>> {
        let handle = self.mint();
        let session_token = self.mint();
        let (_, mut answers) = self.request(&handle)?;
        let options = options([
            ("handle_token", Value::from(handle)),
            ("session_handle_token", Value::from(session_token.clone())),
        ]);
        self.screencast()?
            .call_method("CreateSession", &(options,))
            .context("asking the desktop for a screen-capture session")?;
        let results = answer(&mut answers)?;
        // The protocol fixes the session's path from the token asked for;
        // the answer names it too, and that name wins when it differs.
        let said: Option<&str> = results
            .get("session_handle")
            .and_then(|v| v.try_into().ok());
        match said {
            Some(path) => ObjectPath::try_from(path)
                .map(ObjectPath::into_owned)
                .context("the desktop named a session that is not an object path"),
            None => ObjectPath::try_from(self.session_path(&session_token))
                .context("building the session's path"),
        }
    }

    /// `SelectSources`: monitors only, whether the pointer is drawn into
    /// the pictures, and that the choice is to be remembered. The picker
    /// appears here, unless the token already covers it.
    fn select_sources(
        &mut self,
        session: &ObjectPath<'_>,
        cursor: bool,
        token: Option<&str>,
    ) -> Result<()> {
        let handle = self.mint();
        let (_, mut answers) = self.request(&handle)?;
        let mut pairs = vec![
            ("handle_token", Value::from(handle)),
            // 1 = monitors, the compositor's outputs. Windows (2) and
            // virtual (4) sources are the follow-up.
            ("types", Value::from(1u32)),
            ("multiple", Value::from(false)),
            // 1 = hidden, 2 = drawn into the picture.
            ("cursor_mode", Value::from(if cursor { 2u32 } else { 1u32 })),
            // 2 = remember the choice, and answer with a token.
            ("persist_mode", Value::from(2u32)),
        ];
        if let Some(token) = token {
            pairs.push(("restore_token", Value::from(token)));
        }
        self.screencast()?
            .call_method("SelectSources", &(session, options(pairs)))
            .context("asking the desktop which screen to share")?;
        answer(&mut answers)?;
        Ok(())
    }

    /// `Start`: the picker is answered (by the token or by the user), and
    /// this is where the stream's node id and the fresh token arrive. The
    /// token presented is spent either way: the new one replaces it, and
    /// with none coming back the old one is forgotten rather than
    /// presented again.
    fn start(&mut self, session: &ObjectPath<'_>, presented: bool) -> Result<u32> {
        let handle = self.mint();
        let (_, mut answers) = self.request(&handle)?;
        let options = options([("handle_token", Value::from(handle))]);
        self.screencast()?
            .call_method("Start", &(session, "", options))
            .context("starting the desktop's screen share")?;
        let results = answer(&mut answers)?;
        let token: Option<&str> = results.get("restore_token").and_then(|v| v.try_into().ok());
        let kept = match token {
            Some(token) => keychain::set(TOKEN.0, TOKEN.1, token),
            None if presented => keychain::remove(TOKEN.0, TOKEN.1).map(|_| ()),
            None => Ok(()),
        };
        if let Err(err) = kept {
            // A token that cannot be kept costs one picker next
            // time; it does not fail the capture.
            isocast_diagnostics::diagnostic!(
                "warn"; "the screen-capture token could not be kept (the picker will show \
                 again): {err:#}"
            );
        }
        stream_nodes(&results)?
            .first()
            .copied()
            .context("the desktop started sharing no stream")
    }

    /// `OpenPipeWireRemote`: the remote the stream's node lives on.
    fn pipe_wire_remote(&self, session: &ObjectPath<'_>) -> Result<OwnedFd> {
        let fd: PortalFd = self
            .screencast()?
            .call("OpenPipeWireRemote", &(session, options([])))
            .context("opening the desktop's screen stream")?;
        Ok(fd.into())
    }

    /// `Session.Close`: a session made only for its token does not go on
    /// holding the compositor's stream.
    fn close(&self, session: &ObjectPath<'_>) -> Result<()> {
        Proxy::new(&self.conn, PORTAL, session.clone(), SESSION)?
            .call_noreply("Close", &())
            .context("closing the screen-capture session")
    }

    /// The proxy for the next portal call.
    fn screencast(&self) -> Result<Proxy<'static>> {
        Proxy::new(&self.conn, PORTAL, PORTAL_PATH, SCREENCAST)
            .context("this desktop has no screen-capture portal")
    }

    /// Subscribe to the answer of the request named `handle` *before* the
    /// call that produces it: the portal answers at once, and a signal
    /// missed is a call that waits forever.
    fn request(&self, handle: &str) -> Result<(Proxy<'static>, SignalIterator<'static>)> {
        let proxy = Proxy::new(&self.conn, PORTAL, self.request_path(handle), REQUEST)
            .context("subscribing to the desktop's answer")?;
        let answers = proxy
            .receive_signal("Response")
            .context("subscribing to the desktop's answer")?;
        Ok((proxy, answers))
    }

    /// A token for one request or session, unique within this run and
    /// valid as an object path element.
    fn mint(&mut self) -> String {
        self.next += 1;
        format!("isocast_{}_{}", std::process::id(), self.next)
    }

    fn request_path(&self, handle: &str) -> String {
        format!(
            "/org/freedesktop/portal/desktop/request/{}/{handle}",
            self.sender
        )
    }

    fn session_path(&self, token: &str) -> String {
        format!(
            "/org/freedesktop/portal/desktop/session/{}/{token}",
            self.sender
        )
    }
}

/// The `a{sv}` options the portal's calls take.
fn options<'a, I>(pairs: I) -> HashMap<&'a str, Value<'a>>
where
    I: IntoIterator<Item = (&'a str, Value<'a>)>,
{
    pairs.into_iter().collect()
}

/// A request the desktop ended without an answer (its picker closed,
/// crashed or refused): what a screen source does not ask again after.
#[derive(Debug)]
struct Unanswered(&'static str);

impl std::fmt::Display for Unanswered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Unanswered {}

/// Read a request's answer: the portal's response code, and its results.
fn answer(answers: &mut SignalIterator<'static>) -> Result<HashMap<String, OwnedValue>> {
    let message = answers
        .next()
        .context("the desktop's portal answered nothing (is it still running?)")?;
    let body = message.body();
    let (code, results): (u32, HashMap<String, OwnedValue>) = body
        .deserialize_unchecked()
        .context("reading the desktop's answer")?;
    match code {
        0 => Ok(results),
        // The portal answers the same when its picker crashes, so say so.
        1 => Err(Unanswered(
            "the desktop's screen-sharing picker closed without a choice; if no picker \
             appeared, it crashed (`coredumpctl list hyprland-preview-share-picker` shows it)",
        )
        .into()),
        2 => Err(Unanswered("the desktop ended the screen-capture request").into()),
        _ => Err(Unanswered("the desktop refused the screen-capture request").into()),
    }
}

/// The node ids the portal started, in the order it named them: each
/// stream is `(u32 node, a{sv} properties)`.
fn stream_nodes(results: &HashMap<String, OwnedValue>) -> Result<Vec<u32>> {
    let Some(streams) = results.get("streams") else {
        return Ok(Vec::new());
    };
    let streams: &Array = streams
        .try_into()
        .context("the desktop's streams are not a list")?;
    let mut nodes = Vec::new();
    for index in 0..streams.len() {
        let stream: Option<Structure> = streams
            .get(index)
            .with_context(|| format!("reading the desktop's stream {index}"))?;
        if let Some(node) = stream.and_then(|s| match s.fields().first() {
            Some(Value::U32(node)) => Some(*node),
            _ => None,
        }) {
            nodes.push(node);
        }
    }
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The portal's stream list is `a(ua{sv})`; the node id is the first
    /// field of each stream, and the properties beside it are none of our
    /// business (the PipeWire negotiation describes the picture).
    #[test]
    fn stream_nodes_reads_the_ids_out_of_the_stream_list() {
        let properties: HashMap<&str, Value> =
            HashMap::from([("size", Value::from(2u32)), ("position", Value::from(0u32))]);
        let stream = Value::from(Structure::from((1u32, properties)));
        let results: HashMap<String, OwnedValue> = HashMap::from([(
            "streams".to_string(),
            Value::from(Array::from(vec![
                stream,
                Value::from(Structure::from((7u32, HashMap::<&str, Value>::new()))),
            ]))
            .try_to_owned()
            .expect("an array of streams can be owned"),
        )]);
        assert_eq!(stream_nodes(&results).unwrap(), vec![1, 7]);
    }

    /// xdg-desktop-portal-hyprland's restore data as the permission
    /// store returns it: `v` holding `(s, u, v a{sv})`. Without Hyprland
    /// to say the monitor's size, a geometry off the origin is a region.
    #[test]
    fn describe_choice_reads_hyprland_restore_data() {
        let details: HashMap<&str, Value> = HashMap::from([
            ("output", Value::from("DP-3")),
            ("withCursor", Value::from(2u32)),
            (
                "geometry",
                Value::from(Structure::from((235u32, 75u32, 1604u32, 1063u32))),
            ),
        ]);
        let record = Value::from(Structure::from(("hyprland", 3u32, Value::from(details))));
        assert_eq!(
            describe_choice(&Value::Value(Box::new(record))).as_deref(),
            Some("a 1604x1063 region of DP-3 at 235,75, with the cursor")
        );
        let window: HashMap<&str, Value> = HashMap::from([("windowClass", Value::from("foot"))]);
        let record = Value::from(Structure::from(("hyprland", 3u32, Value::from(window))));
        assert_eq!(
            describe_choice(&record).as_deref(),
            Some("a window of foot")
        );
    }

    /// A whole-screen region goes back to the store as the backend wrote
    /// it, `(su a{sv})`, without the geometry: xdg-desktop-portal-hyprland
    /// treats any other shape as no record and shows its picker.
    #[test]
    fn without_geometry_keeps_the_record_shape() {
        let details: HashMap<&str, Value> = HashMap::from([
            ("output", Value::from("DP-3")),
            ("withCursor", Value::from(2u32)),
            ("timeIssued", Value::from(1792848863u64)),
            (
                "geometry",
                Value::from(Structure::from((0u32, 0u32, 2048u32, 1152u32))),
            ),
        ]);
        let stored = Value::Value(Box::new(Value::from(Structure::from((
            "hyprland",
            3u32,
            Value::from(details),
        )))));
        let record = HyprlandRecord::read(&stored).unwrap();
        let rewritten = record.without_geometry().unwrap();
        assert_eq!(rewritten.value_signature().to_string(), "(suv)");
        let again = HyprlandRecord::read(&rewritten).unwrap();
        assert_eq!(again.region(), None);
        assert_eq!(again.text("output").as_deref(), Some("DP-3"));
        assert!(matches!(again.get("withCursor"), Some(Value::U32(2))));
        assert!(matches!(
            again.get("timeIssued"),
            Some(Value::U64(1792848863))
        ));
    }

    /// A request that came back with no streams at all is not a node.
    #[test]
    fn stream_nodes_without_streams_is_empty() {
        assert!(stream_nodes(&HashMap::new()).unwrap().is_empty());
    }
}
