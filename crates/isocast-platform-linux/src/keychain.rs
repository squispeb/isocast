//! The Keychain stand-in: a 0600 JSON file in the support folder,
//! atomically rewritten (`tmp` then `rename`). The OS's own store
//! (secret-service, kwallet) is a later task; the file lives inside
//! `~/.isocast` next to the socket, and its permissions mean only the
//! user reads it.
//!
//! A locked OS keyring would show a prompt, and the [`Platform`]
//! contract says never to: the file store never does.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

/// The secrets file: `{ service: { account: value } }`.
type Store = BTreeMap<String, BTreeMap<String, String>>;

/// The socket's folder doubles as the secrets' home: the protocol
/// keeps that layout (`~/.isocast` on Linux) in one place.
fn path() -> Result<PathBuf> {
    isocast_protocol::api::socket_path()
        .parent()
        .map(|parent| parent.join("secrets.json"))
        .context("the support folder does not have a parent")
}

fn read() -> Result<Store> {
    match std::fs::read_to_string(path()?) {
        Ok(text) => {
            let value: Value =
                serde_json::from_str(&text).context("the secrets file is not valid JSON")?;
            serde_json::from_value(value).context("the secrets file's shape changed")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error).context("reading the secrets file"),
    }
}

fn write(store: &Store) -> Result<()> {
    let path = path()?;
    let text = serde_json::to_string(store).context("serializing the secrets file")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let temp = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("opening {}", temp.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|_| file.sync_all())
        .with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, &path)
        .with_context(|| format!("moving {} into place", path.display()))?;
    Ok(())
}

pub(crate) fn contains(service: &str, account: &str) -> Result<bool> {
    Ok(read()?
        .get(service)
        .is_some_and(|accounts| accounts.contains_key(account)))
}

pub(crate) fn get(service: &str, account: &str) -> Result<Option<String>> {
    Ok(read()?
        .get(service)
        .and_then(|accounts| accounts.get(account))
        .cloned())
}

pub(crate) fn set(service: &str, account: &str, secret: &str) -> Result<()> {
    let mut store = read()?;
    store
        .entry(service.to_string())
        .or_default()
        .insert(account.to_string(), secret.to_string());
    write(&store)
}

pub(crate) fn remove(service: &str, account: &str) -> Result<bool> {
    let mut store = read()?;
    let removed = store
        .get_mut(service)
        .is_some_and(|accounts| accounts.remove(account).is_some());
    if removed {
        write(&store)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::{get, path, remove, set};

    /// The store lives beside the socket, and the test points the socket
    /// at a scratch folder: a test must never touch the user's own
    /// `~/.isocast/secrets.json` (the first version of this test wrote
    /// there and left empty entries behind).
    #[test]
    fn secret_set_get_remove_round_trip() {
        let scratch = std::env::temp_dir().join(format!(
            "isocast-keychain-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        // SAFETY: the socket path is read from the environment, and this
        // is the only test in the binary that reads it (the frame pool
        // tests don't): nothing here races another thread's read.
        unsafe { std::env::set_var("ISOCAST_SOCKET", scratch.join("isocastd.sock")) };
        std::fs::create_dir_all(&scratch).unwrap();
        assert!(
            path().unwrap().starts_with(&scratch),
            "the secrets file must land in the scratch folder"
        );

        let unique = format!("isocast-test-{}", std::process::id());
        set(&unique, "round-trip", "secret-value").unwrap();
        assert_eq!(
            get(&unique, "round-trip").unwrap().as_deref(),
            Some("secret-value")
        );
        assert!(remove(&unique, "round-trip").unwrap());
        assert_eq!(get(&unique, "round-trip").unwrap(), None);

        std::fs::remove_dir_all(&scratch).ok();
    }
}
