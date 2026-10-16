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
use std::os::fd::OwnedFd;

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
/// the choice the token remembers afterwards.
pub(crate) fn open(cursor: bool) -> Result<Stream> {
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
/// closed again, since this only wants the token.
pub(crate) fn prepare(cursor: bool) -> Result<()> {
    let mut portal = Portal::connect()?;
    let (session, _node) = portal.share(cursor)?;
    portal.close(&session)
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
    /// node's id.
    fn share(&mut self, cursor: bool) -> Result<(ObjectPath<'static>, u32)> {
        let token = keychain::get(TOKEN.0, TOKEN.1).ok().flatten();
        let session = self.new_session()?;
        self.select_sources(&session, cursor, token.as_deref())?;
        let node = self.start(&session)?;
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
    /// this is where the stream's node id and the fresh token arrive.
    fn start(&mut self, session: &ObjectPath<'_>) -> Result<u32> {
        let handle = self.mint();
        let (_, mut answers) = self.request(&handle)?;
        let options = options([("handle_token", Value::from(handle))]);
        self.screencast()?
            .call_method("Start", &(session, "", options))
            .context("starting the desktop's screen share")?;
        let results = answer(&mut answers)?;
        if let Some(token) = results.get("restore_token").and_then(|v| v.try_into().ok()) {
            let token: &str = token;
            if let Err(err) = keychain::set(TOKEN.0, TOKEN.1, token) {
                // A token that cannot be kept costs one picker next
                // time; it does not fail the capture.
                isocast_diagnostics::diagnostic!(
                    "warn"; "the screen-capture token could not be kept (the picker will show \
                     again): {err:#}"
                );
            }
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
        1 => bail!("the desktop's screen-capture picker was dismissed"),
        2 => bail!("the desktop ended the screen-capture request"),
        other => bail!("the desktop refused the screen-capture request (response {other})"),
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

    /// A request that came back with no streams at all is not a node.
    #[test]
    fn stream_nodes_without_streams_is_empty() {
        assert!(stream_nodes(&HashMap::new()).unwrap().is_empty());
    }
}
