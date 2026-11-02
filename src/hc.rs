//! Client for Herdr 2's core, `hc`: JSON-RPC 2.0, one message per line, on the socket hc hands each of
//! its terminals as `HC_TERMINAL_SOCKET`.
//!
//! Spoken directly rather than through the `hc` binary, which starts a core whenever none answers: a
//! core started from here would inherit this process's environment for every terminal it ever runs.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

pub const SOCKET_ENV: &str = "HC_TERMINAL_SOCKET";
pub const TERMINAL_ENV: &str = "HC_TERMINAL_ID";

/// Where a window shows a terminal. herdr-app adopts a terminal carrying a space into that space, and
/// into the tab named too, else a new tab of it; one carrying neither stays hidden.
pub const SPACE_LABEL: &str = "herdr.space";
pub const TAB_LABEL: &str = "herdr.tab";
pub const UNIT_LABEL: &str = "herdr-stack.unit";
pub const VIEW_LABEL: &str = "herdr-stack.view";

const TIMEOUT: Duration = Duration::from_secs(5);

/// The core this process runs under, and the terminal it runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hc {
    pub socket: PathBuf,
    pub terminal: String,
}

impl Hc {
    pub fn of(var: impl Fn(&str) -> Option<String>) -> Option<Hc> {
        let set = |key: &str| var(key).filter(|value| !value.is_empty());
        Some(Hc {
            socket: PathBuf::from(set(SOCKET_ENV)?),
            terminal: set(TERMINAL_ENV)?,
        })
    }

    pub fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let stream = UnixStream::connect(&self.socket)
            .map_err(|error| format!("hc socket {}: {error}", self.socket.display()))?;
        let io = |error: std::io::Error| format!("hc socket: {error}");
        stream.set_read_timeout(Some(TIMEOUT)).map_err(io)?;
        stream.set_write_timeout(Some(TIMEOUT)).map_err(io)?;
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut writer = &stream;
        writeln!(writer, "{line}").map_err(io)?;
        writer.flush().map_err(io)?;

        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply).map_err(io)?;
        if reply.trim().is_empty() {
            return Err(format!("hc socket: {method}: no reply"));
        }
        let reply: Value = serde_json::from_str(&reply)
            .map_err(|error| format!("hc socket: {method}: {error}"))?;
        if let Some(error) = reply.get("error") {
            let kind = error
                .pointer("/data/kind")
                .and_then(Value::as_str)
                .unwrap_or("error");
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(no message)");
            return Err(format!("hc refused {method}: {kind}: {message}"));
        }
        reply
            .get("result")
            .cloned()
            .ok_or_else(|| format!("hc socket: {method}: reply carries neither result nor error"))
    }

    pub fn own(&self) -> Result<Value, String> {
        self.request("terminal.get", json!({"terminal": self.terminal}))
    }

    pub fn place(&self) -> Result<Place, String> {
        self.own().map(|own| Place::of(&own))
    }

    pub fn space_mates(&self, space: &str) -> Result<Value, String> {
        self.request("terminal.list", json!({"labels": {SPACE_LABEL: space}}))
    }
}

/// The space and tab a terminal sits in, as its labels say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Place {
    pub space: Option<String>,
    pub tab: Option<String>,
}

impl Place {
    pub fn of(terminal: &Value) -> Place {
        let label = |key: &str| {
            terminal
                .get("labels")
                .and_then(|labels| labels.get(key))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        Place {
            space: label(SPACE_LABEL),
            tab: label(TAB_LABEL),
        }
    }

    pub fn labels(&self) -> serde_json::Map<String, Value> {
        let mut labels = serde_json::Map::new();
        if let Some(space) = &self.space {
            labels.insert(SPACE_LABEL.into(), space.clone().into());
        }
        if let Some(tab) = &self.tab {
            labels.insert(TAB_LABEL.into(), tab.clone().into());
        }
        labels
    }
}

/// The directories the terminals of a `terminal.list` reply sit in, where each one's program is now
/// before where it started.
pub fn cwds(listed: &Value) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let terminals = listed
        .get("terminals")
        .and_then(Value::as_array)
        .map(|terminals| terminals.iter())
        .unwrap_or_default();
    for terminal in terminals {
        let cwd = terminal
            .pointer("/foreground/cwd")
            .or_else(|| terminal.get("cwd"))
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty());
        if let Some(dir) = cwd.map(PathBuf::from)
            && !dirs.contains(&dir)
        {
            dirs.push(dir);
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::net::UnixListener;

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn a_terminal_of_hc_is_known_by_its_socket_and_its_address() {
        assert_eq!(
            Hc::of(vars(&[(SOCKET_ENV, "/run/hc.sock"), (TERMINAL_ENV, "t7")])),
            Some(Hc {
                socket: PathBuf::from("/run/hc.sock"),
                terminal: "t7".into(),
            })
        );
        assert_eq!(Hc::of(vars(&[(SOCKET_ENV, "/run/hc.sock")])), None);
        assert_eq!(
            Hc::of(vars(&[(SOCKET_ENV, ""), (TERMINAL_ENV, "t7")])),
            None
        );
    }

    #[test]
    fn a_place_is_read_from_the_window_labels() {
        let own =
            json!({"address": "t3", "labels": {SPACE_LABEL: "sp1", TAB_LABEL: "tb2", "x": "y"}});
        let place = Place::of(&own);
        assert_eq!(place.space.as_deref(), Some("sp1"));
        assert_eq!(place.tab.as_deref(), Some("tb2"));
        assert_eq!(
            Value::Object(place.labels()),
            json!({SPACE_LABEL: "sp1", TAB_LABEL: "tb2"})
        );
        assert_eq!(Place::of(&json!({"labels": {}})), Place::default());
    }

    #[test]
    fn a_program_s_own_directory_comes_before_where_its_terminal_started() {
        let listed = json!({"terminals": [
            {"cwd": "/repos/a", "foreground": {"cwd": "/repos/a/lib"}},
            {"cwd": "/repos/b", "foreground": null},
            {"cwd": "/repos/b"},
        ]});
        assert_eq!(
            cwds(&listed),
            [PathBuf::from("/repos/a/lib"), PathBuf::from("/repos/b")]
        );
        assert!(cwds(&json!({})).is_empty());
    }

    fn core(name: &str, reply: Value) -> (Hc, std::thread::JoinHandle<Value>) {
        let socket = std::env::temp_dir().join(format!("hs-hc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("a socket");
        let asked = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("a caller");
            let mut line = String::new();
            BufReader::new(&stream)
                .read_line(&mut line)
                .expect("a line");
            let asked: Value = serde_json::from_str(&line).expect("json");
            let mut answer = reply;
            answer["id"] = asked["id"].clone();
            answer["jsonrpc"] = "2.0".into();
            writeln!(&stream, "{answer}").expect("reply");
            asked
        });
        let hc = Hc {
            socket,
            terminal: "t1".into(),
        };
        (hc, asked)
    }

    #[test]
    fn a_request_is_json_rpc_and_its_result_comes_back() {
        let (hc, asked) = core("result", json!({"result": {"address": "t1"}}));
        assert_eq!(hc.own(), Ok(json!({"address": "t1"})));
        let asked = asked.join().expect("asked");
        assert_eq!(asked["jsonrpc"], "2.0");
        assert_eq!(asked["method"], "terminal.get");
        assert_eq!(asked["params"], json!({"terminal": "t1"}));
        let _ = std::fs::remove_file(&hc.socket);
    }

    #[test]
    fn a_refusal_says_its_kind_and_message() {
        let (hc, asked) = core(
            "refusal",
            json!({"error": {"code": -32000, "message": "no terminal \"t1\"", "data": {"kind": "not_found"}}}),
        );
        assert_eq!(
            hc.own(),
            Err("hc refused terminal.get: not_found: no terminal \"t1\"".to_string())
        );
        asked.join().expect("asked");
        let _ = std::fs::remove_file(&hc.socket);
    }
}
