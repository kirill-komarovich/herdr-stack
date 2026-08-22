//! Client for Herdr's control socket: line-delimited JSON, one `{id, method, params}` per request.
//!
//! Spoken directly rather than through the `herdr` binary, which a popup has no reliable way to
//! find: it inherits neither `HERDR_BIN_PATH` nor a `PATH` worth trusting.

use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(5);

pub fn socket_path() -> PathBuf {
    home().join(".config/herdr/herdr.sock")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

#[derive(Debug)]
pub enum Error {
    Connect {
        path: PathBuf,
        source: std::io::Error,
    },
    Io(std::io::Error),
    Malformed(String),
    Remote {
        code: String,
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Connect { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Io(source) => write!(f, "herdr socket: {source}"),
            Error::Malformed(what) => write!(f, "herdr socket: {what}"),
            Error::Remote { code, message } => write!(f, "herdr refused: {code}: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Connect { source, .. } | Error::Io(source) => Some(source),
            _ => None,
        }
    }
}

pub fn request(method: &str, params: Value) -> Result<Value, Error> {
    request_with_timeout(method, params, TIMEOUT)
}

pub fn request_with_timeout(
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, Error> {
    let path = socket_path();
    let stream = UnixStream::connect(&path).map_err(|source| Error::Connect {
        path: path.clone(),
        source,
    })?;
    stream.set_read_timeout(Some(timeout)).map_err(Error::Io)?;
    stream.set_write_timeout(Some(TIMEOUT)).map_err(Error::Io)?;

    let line = json!({"id": format!("herdr-dev-{}", std::process::id()), "method": method, "params": params});
    let mut writer = &stream;
    writeln!(writer, "{line}").map_err(Error::Io)?;
    writer.flush().map_err(Error::Io)?;

    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .map_err(Error::Io)?;
    if reply.trim().is_empty() {
        return Err(Error::Malformed(format!("{method}: no reply")));
    }
    let reply: Value = serde_json::from_str(&reply)
        .map_err(|source| Error::Malformed(format!("{method}: {source}")))?;

    if let Some(error) = reply.get("error") {
        return Err(Error::Remote {
            code: text(error, "code").unwrap_or("error").to_string(),
            message: text(error, "message").unwrap_or("(no message)").to_string(),
        });
    }
    reply.get("result").cloned().ok_or_else(|| {
        Error::Malformed(format!("{method}: reply carries neither result nor error"))
    })
}

/// The session model as Herdr reports it. A live popup is absent from it, so the focused pane is
/// the one the user was looking at when they pressed the key.
#[derive(Debug, Clone)]
pub struct Snapshot(Value);

impl Snapshot {
    pub fn fetch() -> Result<Snapshot, Error> {
        let result = request("session.snapshot", json!({}))?;
        result
            .get("snapshot")
            .cloned()
            .map(Snapshot)
            .ok_or_else(|| Error::Malformed("session.snapshot: no snapshot in result".into()))
    }

    pub fn from_value(value: Value) -> Snapshot {
        Snapshot(value)
    }

    pub fn focused_pane_cwd(&self) -> Option<PathBuf> {
        let id = text(&self.0, "focused_pane_id")?;
        let pane = self
            .array("panes")
            .find(|pane| text(pane, "pane_id") == Some(id))?;
        text(pane, "cwd").map(expand_tilde)
    }

    /// `WorkspaceInfo` carries no cwd of its own, so the workspace's directory is taken from its
    /// worktree checkout when Herdr knows of one, and otherwise from the directories its panes are
    /// sitting in.
    pub fn focused_workspace_cwds(&self) -> Vec<PathBuf> {
        let Some(id) = text(&self.0, "focused_workspace_id") else {
            return Vec::new();
        };
        let mut dirs = Vec::new();
        if let Some(workspace) = self
            .array("workspaces")
            .find(|workspace| text(workspace, "workspace_id") == Some(id))
        {
            let worktree = workspace.get("worktree");
            for key in ["checkout_path", "repo_root"] {
                if let Some(dir) = worktree.and_then(|worktree| text(worktree, key)) {
                    push_unique(&mut dirs, expand_tilde(dir));
                }
            }
        }
        for pane in self.array("panes") {
            if text(pane, "workspace_id") != Some(id) {
                continue;
            }
            if let Some(cwd) = text(pane, "cwd") {
                push_unique(&mut dirs, expand_tilde(cwd));
            }
        }
        dirs
    }

    fn array(&self, key: &str) -> impl Iterator<Item = &Value> {
        self.0
            .get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter())
            .unwrap_or_default()
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

fn push_unique(dirs: &mut Vec<PathBuf>, dir: PathBuf) {
    if !dirs.contains(&dir) {
        dirs.push(dir);
    }
}

pub fn expand_tilde(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    match path.strip_prefix("~") {
        Ok(rest) => home().join(rest),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot::from_value(json!({
            "focused_workspace_id": "w7",
            "focused_pane_id": "w7:p6",
            "workspaces": [
                {"workspace_id": "w5", "worktree": {"checkout_path": "/repos/other"}},
                {"workspace_id": "w7"},
            ],
            "panes": [
                {"pane_id": "w5:p1", "workspace_id": "w5", "cwd": "/repos/other"},
                {"pane_id": "w7:p2", "workspace_id": "w7", "cwd": "/repos/storefront"},
                {"pane_id": "w7:p6", "workspace_id": "w7", "cwd": "/repos/storefront/app/models"},
            ],
        }))
    }

    #[test]
    fn the_focused_pane_supplies_its_own_directory() {
        assert_eq!(
            snapshot().focused_pane_cwd(),
            Some(PathBuf::from("/repos/storefront/app/models"))
        );
    }

    #[test]
    fn a_pane_that_vanished_between_snapshots_is_not_fatal() {
        let snapshot = Snapshot::from_value(json!({
            "focused_pane_id": "w7:p6",
            "panes": [{"pane_id": "w7:p2", "cwd": "/repos/storefront"}],
        }));
        assert_eq!(snapshot.focused_pane_cwd(), None);
    }

    #[test]
    fn the_workspace_fallback_stays_inside_the_focused_workspace() {
        assert_eq!(
            snapshot().focused_workspace_cwds(),
            vec![
                PathBuf::from("/repos/storefront"),
                PathBuf::from("/repos/storefront/app/models"),
            ]
        );
    }

    #[test]
    fn a_worktree_checkout_leads_the_workspace_fallback() {
        let snapshot = Snapshot::from_value(json!({
            "focused_workspace_id": "w7",
            "workspaces": [{"workspace_id": "w7", "worktree": {
                "checkout_path": "/repos/storefront-wt2",
                "repo_root": "/repos/storefront",
            }}],
            "panes": [{"pane_id": "w7:p1", "workspace_id": "w7", "cwd": "/tmp"}],
        }));
        assert_eq!(
            snapshot.focused_workspace_cwds(),
            vec![
                PathBuf::from("/repos/storefront-wt2"),
                PathBuf::from("/repos/storefront"),
                PathBuf::from("/tmp"),
            ]
        );
    }

    #[test]
    fn an_empty_snapshot_yields_no_candidates() {
        let snapshot = Snapshot::from_value(json!({}));
        assert_eq!(snapshot.focused_pane_cwd(), None);
        assert!(snapshot.focused_workspace_cwds().is_empty());
    }
}
