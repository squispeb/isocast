//! What Hyprland knows about windows and monitors, for window capture.
//!
//! The standard toplevel protocols say which windows exist (app id, title,
//! a stable identifier) but not where they are or how big, so ranking
//! several matches by size and placing an app's windows on a display's
//! canvas take the compositor's own IPC. The socket is asked directly
//! (a `hyprctl` child process per question would stall a capture loop that
//! asks several times a second); `hyprctl` is the fallback when the
//! daemon was not started from the session.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One window, from `hyprctl clients -j`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Client {
    /// Hyprland's `stableId`: the same text the toplevel protocol's
    /// `identifier` carries.
    pub stable_id: String,
    pub class: String,
    pub title: String,
    /// Position and size in the layout's (logical) coordinates.
    pub at: (i32, i32),
    pub size: (i32, i32),
    /// The monitor's id.
    pub monitor: i64,
    /// The workspace's id (negative for a special workspace).
    pub workspace: i64,
    /// 0 for the window that has the focus, higher the longer ago.
    pub focus_history: i64,
    pub floating: bool,
}

/// One monitor, from `hyprctl monitors -j`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Monitor {
    pub id: i64,
    pub name: String,
    /// Pixels, as the mode says.
    pub width: u32,
    pub height: u32,
    /// Position in the layout's (logical) coordinates.
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    /// The workspaces the monitor shows: the active one, and a special
    /// one that is open over it (0 when none).
    pub shown: (i64, i64),
    /// False while the monitor sleeps (DPMS off): nothing is rendered.
    pub awake: bool,
}

/// The mapped windows, or `None` when there is no Hyprland to ask.
pub(crate) fn clients() -> Option<Vec<Client>> {
    Some(parse_clients(&request("j/clients")?))
}

/// The monitors, or `None` when there is no Hyprland to ask.
pub(crate) fn monitors() -> Option<Vec<Monitor>> {
    Some(parse_monitors(&request("j/monitors")?))
}

/// The answer to one IPC command: through the instance's socket, else
/// through `hyprctl` (which finds the instance itself).
fn request(command: &str) -> Option<Vec<u8>> {
    if let Some(answer) = socket_request(command) {
        return Some(answer);
    }
    // A `hyprctl` child that failed once (no instance to find) would fail
    // again; asking every few hundred milliseconds would only spawn
    // processes from the capture loop.
    static FAILED_AT: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let failed = FAILED_AT.lock().ok()?;
        if failed.is_some_and(|at| at.elapsed() < HYPRCTL_RETRY) {
            return None;
        }
    }
    let answer = hyprctl_request(command);
    if answer.is_none()
        && let Ok(mut failed) = FAILED_AT.lock()
    {
        *failed = Some(Instant::now());
    }
    answer
}

/// How long after `hyprctl` failed it is not tried again.
const HYPRCTL_RETRY: Duration = Duration::from_secs(30);

fn hyprctl_request(command: &str) -> Option<Vec<u8>> {
    let (flag, name) = command.split_once('/')?;
    let mut hyprctl = Command::new("hyprctl");
    if flag == "j" {
        hyprctl.arg(name).arg("-j");
    } else {
        hyprctl.arg(name);
    }
    let output = hyprctl.output().ok()?;
    output.status.success().then_some(output.stdout)
}

fn socket_request(command: &str) -> Option<Vec<u8>> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let path = std::path::Path::new(&runtime)
        .join("hypr")
        .join(signature)
        .join(".socket.sock");
    let mut socket = UnixStream::connect(path).ok()?;
    let timeout = Some(Duration::from_secs(2));
    socket.set_read_timeout(timeout).ok()?;
    socket.set_write_timeout(timeout).ok()?;
    socket.write_all(command.as_bytes()).ok()?;
    let mut answer = Vec::new();
    socket.read_to_end(&mut answer).ok()?;
    (!answer.is_empty()).then_some(answer)
}

fn pair(value: &serde_json::Value) -> Option<(i32, i32)> {
    Some((value[0].as_i64()? as i32, value[1].as_i64()? as i32))
}

fn parse_clients(json: &[u8]) -> Vec<Client> {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(json) else {
        return Vec::new();
    };
    parsed
        .as_array()
        .into_iter()
        .flatten()
        .filter(|client| client["mapped"].as_bool().unwrap_or(true))
        .filter_map(|client| {
            Some(Client {
                stable_id: client["stableId"].as_str().unwrap_or_default().to_string(),
                class: client["class"].as_str()?.to_string(),
                title: client["title"].as_str().unwrap_or_default().to_string(),
                at: pair(&client["at"])?,
                size: pair(&client["size"])?,
                monitor: client["monitor"].as_i64().unwrap_or(-1),
                workspace: client["workspace"]["id"].as_i64().unwrap_or(0),
                focus_history: client["focusHistoryID"].as_i64().unwrap_or(i64::MAX),
                floating: client["floating"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

fn parse_monitors(json: &[u8]) -> Vec<Monitor> {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(json) else {
        return Vec::new();
    };
    parsed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|monitor| {
            Some(Monitor {
                id: monitor["id"].as_i64()?,
                name: monitor["name"].as_str()?.to_string(),
                width: monitor["width"].as_u64()? as u32,
                height: monitor["height"].as_u64()? as u32,
                x: monitor["x"].as_i64().unwrap_or(0) as i32,
                y: monitor["y"].as_i64().unwrap_or(0) as i32,
                scale: monitor["scale"].as_f64().unwrap_or(1.0),
                shown: (
                    monitor["activeWorkspace"]["id"].as_i64().unwrap_or(0),
                    monitor["specialWorkspace"]["id"].as_i64().unwrap_or(0),
                ),
                awake: monitor["dpmsStatus"].as_bool().unwrap_or(true),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This box's own `hyprctl` shapes, trimmed to the fields read.
    #[test]
    fn clients_and_monitors_parse() {
        let clients = br#"[
            {"mapped": true, "at": [13, 39], "size": [2022, 1100], "monitor": 1,
             "class": "foot", "title": "one", "floating": false,
             "workspace": {"id": 3, "name": "3"},
             "focusHistoryID": 1, "stableId": "1800000e"},
            {"mapped": false, "at": [0, 0], "size": [1, 1], "monitor": 1,
             "class": "gone", "title": "", "stableId": "1"},
            {"mapped": true, "at": [0, 0], "size": [10, 10], "class": "x"}
        ]"#;
        let parsed = parse_clients(clients);
        assert_eq!(parsed.len(), 2, "an unmapped window is not capturable");
        assert_eq!(parsed[0].stable_id, "1800000e");
        assert_eq!(parsed[0].size, (2022, 1100));
        assert_eq!(parsed[0].focus_history, 1);
        assert_eq!(parsed[0].workspace, 3);
        assert_eq!(parsed[1].monitor, -1);
        assert!(parse_clients(b"not json").is_empty());

        let monitors = br#"[{"id": 1, "name": "DP-3", "width": 2560, "height": 1440,
            "x": 0, "y": 0, "scale": 1.25, "dpmsStatus": false,
            "activeWorkspace": {"id": 3, "name": "3"},
            "specialWorkspace": {"id": 0, "name": ""}}]"#;
        let parsed = parse_monitors(monitors);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "DP-3");
        assert!(!parsed[0].awake);
        assert_eq!(parsed[0].scale, 1.25);
        assert_eq!(parsed[0].shown, (3, 0));
    }
}
