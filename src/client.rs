//! The TUI's end of the daemon socket: find a daemon, start one if nothing answers, then keep the
//! connection for as long as the popup lives.
//!
//! The connection is held rather than reopened per request because a connected client is what stops an
//! idle daemon exiting underneath the popup.
//!
//! Skew is decided by `protocol` alone: a rebuild changes `version` every time and must not read as a
//! broken daemon.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::manifest::{DockerService, LocalUnit, Project};
use crate::unit::{self, Status};
use crate::{daemon, state, supervisor};

/// A stop is allowed to take the whole `SIGTERM` grace and the reap after the `SIGKILL` that follows
/// it, so the patience for a reply is the daemon's worst case rather than a round-trip guess.
const REQUEST_TIMEOUT: Duration = supervisor::GRACE.saturating_add(Duration::from_secs(10));
const STARTUP_WAIT: Duration = Duration::from_secs(5);
const STARTUP_POLL: Duration = Duration::from_millis(20);

/// Where a daemon is reached, and how one is started if none answers.
#[derive(Debug, Clone)]
pub struct Endpoint {
    root: PathBuf,
}

impl Endpoint {
    /// The one endpoint that exists in production: the state root spelled out in §8.
    pub fn spelled_out() -> Endpoint {
        Endpoint {
            root: state::root(),
        }
    }

    pub fn at(root: impl Into<PathBuf>) -> Endpoint {
        Endpoint { root: root.into() }
    }

    pub fn open(&self) -> Result<Link, String> {
        match self.dial() {
            Ok(link) => Ok(link),
            Err(Failure::Unreachable) => {
                self.start()?;
                self.connect_within(STARTUP_WAIT)
            }
            Err(Failure::Refused(complaint)) => Err(complaint),
        }
    }

    /// Fork, `setsid(2)`, exec — the executable being this very one, as `current_exe()` reports it and
    /// unresolved: it survives the binary being replaced mid-run, so this always starts the newest
    /// build, and an `Err` from it is a hard error rather than an invitation to guess a path.
    pub fn command(&self, exe: &Path) -> Command {
        let mut command = Command::new(exe);
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // A daemon told to serve a scratch root has to be told which one; the spelled-out root needs
        // no argument and so the production argv stays exactly `daemon`.
        if self.root != state::root() {
            command.arg("--state-root").arg(&self.root);
        }
        unsafe {
            command.pre_exec(|| match libc::setsid() {
                -1 => Err(std::io::Error::last_os_error()),
                _ => Ok(()),
            });
        }
        command
    }

    fn socket_path(&self) -> PathBuf {
        state::socket_path(&self.root)
    }

    fn start(&self) -> Result<(), String> {
        let exe = std::env::current_exe()
            .map_err(|error| format!("cannot locate our own executable: {error}"))?;
        self.command(&exe)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("cannot start a daemon: {error}"))
    }

    /// Connects to a daemon that is already running, waiting up to `patience` for one to appear.
    pub fn connect_within(&self, patience: Duration) -> Result<Link, String> {
        let deadline = Instant::now() + patience;
        loop {
            match self.dial() {
                Ok(link) => return Ok(link),
                Err(Failure::Refused(complaint)) => return Err(complaint),
                Err(Failure::Unreachable) if Instant::now() >= deadline => {
                    return Err(format!(
                        "no daemon answered {}",
                        self.socket_path().display()
                    ));
                }
                Err(Failure::Unreachable) => std::thread::sleep(STARTUP_POLL),
            }
        }
    }

    fn dial(&self) -> Result<Link, Failure> {
        let path = self.socket_path();
        // A socket file whose connect fails is the leftover of a daemon that died without unlinking;
        // the daemon started next replaces it, which is why nothing is said about it here.
        let stream = UnixStream::connect(&path).map_err(|_| Failure::Unreachable)?;
        Link::greet(stream).map_err(Failure::Refused)
    }
}

/// What a verb acts on, resolved from the row under the cursor against the manifest it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target<'a> {
    Local(&'a LocalUnit),
    Docker(&'a DockerService),
}

impl<'a> Target<'a> {
    /// A row carries only a kind and a name; the manifest holds what the daemon has to be told. A unit
    /// the manifest could not make sense of refuses with its own complaint rather than crossing the
    /// wire as half a command.
    pub fn of(project: &'a Project, kind: &str, name: &str) -> Result<Target<'a>, String> {
        match kind {
            unit::DOCKER => project
                .docker
                .iter()
                .find(|service| service.name == name)
                .map(Target::Docker)
                .ok_or_else(|| format!("no docker service named {name}")),
            unit::LOCAL => {
                let unit = project
                    .local
                    .iter()
                    .find(|candidate| candidate.name == name)
                    .ok_or_else(|| format!("no local unit named {name}"))?;
                match &unit.problem {
                    Some(problem) => Err(problem.clone()),
                    None => Ok(Target::Local(unit)),
                }
            }
            kind => Err(format!("nothing a verb can act on in a `{kind}` row")),
        }
    }
}

enum Failure {
    /// Nothing is listening: start a daemon.
    Unreachable,
    /// Something is listening but the conversation went wrong: starting another would not help.
    Refused(String),
}

/// What the daemon said it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub version: String,
    pub protocol: u64,
    pub pid: u32,
}

impl Peer {
    fn read(handshake: &Value) -> Result<Peer, String> {
        Ok(Peer {
            version: handshake
                .get("version")
                .and_then(Value::as_str)
                .ok_or("handshake: no version")?
                .to_string(),
            protocol: handshake
                .get("protocol")
                .and_then(Value::as_u64)
                .ok_or("handshake: no protocol")?,
            pid: handshake
                .get("pid")
                .and_then(Value::as_u64)
                .ok_or("handshake: no pid")? as u32,
        })
    }
}

/// The connection itself, held open for the popup's whole life.
#[derive(Debug)]
struct Wire {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Wire {
    fn open(stream: UnixStream) -> Result<Wire, String> {
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|error| format!("daemon socket: {error}"))?;
        stream
            .set_write_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|error| format!("daemon socket: {error}"))?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|error| format!("daemon socket: {error}"))?,
        );
        Ok(Wire {
            stream,
            reader,
            next_id: 1,
        })
    }

    fn roundtrip(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"id": id, "method": method, "params": params});
        let mut writer = &self.stream;
        writeln!(writer, "{line}").map_err(|error| format!("daemon socket: {error}"))?;
        writer
            .flush()
            .map_err(|error| format!("daemon socket: {error}"))?;

        let mut reply = String::new();
        self.reader
            .read_line(&mut reply)
            .map_err(|error| format!("daemon socket: {error}"))?;
        if reply.trim().is_empty() {
            return Err(format!("{method}: the daemon said nothing"));
        }
        let reply: Value =
            serde_json::from_str(&reply).map_err(|error| format!("{method}: {error}"))?;
        if let Some(error) = reply.get("error") {
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string();
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(no message)");
            return Err(format!("daemon refused: {code}: {message}"));
        }
        reply
            .get("result")
            .cloned()
            .ok_or_else(|| format!("{method}: reply carries neither result nor error"))
    }
}

#[derive(Debug)]
pub struct Link {
    wire: Wire,
    peer: Peer,
}

impl Link {
    fn greet(stream: UnixStream) -> Result<Link, String> {
        let mut wire = Wire::open(stream)?;
        let peer = Peer::read(&wire.roundtrip("handshake", json!({}))?)?;
        Ok(Link { wire, peer })
    }

    pub fn peer(&self) -> &Peer {
        &self.peer
    }

    pub fn skewed(&self) -> bool {
        self.peer.protocol != daemon::PROTOCOL
    }

    /// Refuses without touching the socket when the daemon speaks another protocol: a wire we cannot
    /// read is worse than an unanswered verb, and kill-and-respawn would silently drop a live stack.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        if self.skewed() {
            return Err(self.footer());
        }
        self.wire.roundtrip(method, params)
    }

    /// Every verb answers with a note or with nothing: a refusal — a unit another project holds, a
    /// service docker would not bring up — is an answer, and only a genuine failure comes back as
    /// `Err`.
    pub fn start(&mut self, project: &Project, target: &Target) -> Result<Option<String>, String> {
        self.verb("start", project, target)
    }

    pub fn stop(&mut self, project: &Project, target: &Target) -> Result<Option<String>, String> {
        self.verb("stop", project, target)
    }

    pub fn restart(
        &mut self,
        project: &Project,
        target: &Target,
    ) -> Result<Option<String>, String> {
        self.verb("restart", project, target)
    }

    fn verb(
        &mut self,
        method: &str,
        project: &Project,
        target: &Target,
    ) -> Result<Option<String>, String> {
        let params = json!({"project": describe(project), "unit": spell(target)});
        let reply = self.request(method, params)?;
        Ok(reply
            .get("note")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    /// Keyed by unit key, so a local and a docker unit of one name stay apart.
    pub fn status(&mut self, project: &Project) -> Result<BTreeMap<String, Status>, String> {
        let params = json!({"project": describe(project), "docker": declared(project)});
        let reply = self.request("status", params)?;
        let units = reply
            .get("units")
            .and_then(Value::as_object)
            .ok_or("status: no units")?;
        units
            .iter()
            .map(|(unit, status)| Status::read(status).map(|status| (unit.clone(), status)))
            .collect()
    }

    /// One line for the footer: what is running the stack, or — under skew — which process to kill.
    pub fn footer(&self) -> String {
        let Peer {
            version,
            protocol,
            pid,
        } = &self.peer;
        if self.skewed() {
            format!(
                "daemon {version} pid {pid} speaks protocol {protocol}, this build speaks {} — kill {pid} to clear it",
                daemon::PROTOCOL
            )
        } else {
            format!("daemon {version}  pid {pid}")
        }
    }
}

fn describe(project: &Project) -> Value {
    json!({"path": project.root.to_string_lossy(), "name": project.name})
}

/// The daemon is told what to run and nothing about manifests, so a unit crosses the wire whole. The
/// env here is the manifest's layers only: the process layer under it is the daemon's own. A service
/// carries its `one_shot` because that is what decides whether a start waits for readiness.
fn spell(target: &Target) -> Value {
    match target {
        Target::Local(unit) => json!({
            "kind": unit::LOCAL,
            "name": unit.name,
            "cmd": unit.cmd,
            "cwd": unit.cwd.to_string_lossy(),
            "env": unit.env,
        }),
        Target::Docker(service) => json!({
            "kind": unit::DOCKER,
            "name": service.name,
            "one_shot": service.one_shot,
        }),
    }
}

/// The services a status read is about, in the manifest's own order.
fn declared(project: &Project) -> Value {
    Value::Array(
        project
            .docker
            .iter()
            .map(|service| json!({"name": service.name, "one_shot": service.one_shot}))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::OsString;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("herdr-dev-client-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp root");
        root
    }

    /// Answers every line with a handshake reply of the caller's choosing, so both skew and a merely
    /// rebuilt daemon can be staged without a second build, and counts what the client sent.
    fn fake_daemon(root: &Path, protocol: u64, version: &str, pid: u32) -> mpsc::Receiver<usize> {
        let listener = UnixListener::bind(state::socket_path(root)).expect("bind");
        let (lines_seen, seen) = mpsc::channel();
        let version = version.to_string();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let reading = stream.try_clone().expect("clone");
            let mut writer = &stream;
            let mut count = 0;
            for line in BufReader::new(reading).lines() {
                if line.is_err() {
                    break;
                }
                count += 1;
                let _ = lines_seen.send(count);
                let reply = json!({
                    "id": count,
                    "result": {"version": version, "protocol": protocol, "pid": pid},
                });
                if writeln!(writer, "{reply}").is_err() {
                    break;
                }
            }
        });
        seen
    }

    fn argv(endpoint: &Endpoint) -> Vec<OsString> {
        endpoint
            .command(Path::new("/opt/herdr-dev"))
            .get_args()
            .map(OsString::from)
            .collect()
    }

    #[test]
    fn the_production_argv_is_exactly_daemon() {
        assert_eq!(
            argv(&Endpoint::spelled_out()),
            vec![OsString::from("daemon")]
        );
    }

    #[test]
    fn a_scratch_root_is_named_on_the_argv_because_only_the_default_is_spelled_out() {
        assert_eq!(
            argv(&Endpoint::at("/tmp/scratch")),
            vec![
                OsString::from("daemon"),
                OsString::from("--state-root"),
                OsString::from("/tmp/scratch"),
            ]
        );
    }

    #[test]
    fn a_matching_protocol_shows_the_version_and_the_pid() {
        let root = temp_root("ready");
        let _seen = fake_daemon(&root, daemon::PROTOCOL, "0.1.0", 4242);
        let link = Endpoint::at(&root).open().expect("link");
        assert!(!link.skewed());
        assert_eq!(link.peer().pid, 4242);
        assert_eq!(link.footer(), "daemon 0.1.0  pid 4242");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn a_daemon_from_another_build_of_the_same_protocol_is_not_skew() {
        let root = temp_root("rebuild");
        let _seen = fake_daemon(&root, daemon::PROTOCOL, "0.9.9", 51);
        let mut link = Endpoint::at(&root).open().expect("link");
        assert!(!link.skewed());
        assert!(link.footer().contains("0.9.9"));
        assert!(!link.footer().contains("kill"));
        assert!(link.request("handshake", json!({})).is_ok());
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn a_different_protocol_names_the_pid_and_the_kill_and_sends_nothing_further() {
        let root = temp_root("skew");
        let seen = fake_daemon(&root, daemon::PROTOCOL + 1, "0.2.0", 777);
        let mut link = Endpoint::at(&root).open().expect("link");
        assert!(link.skewed());

        let line = link.footer();
        assert!(line.contains("0.2.0"), "{line}");
        assert!(line.contains("777"), "{line}");
        assert!(line.contains("kill 777"), "{line}");
        assert_eq!(line.lines().count(), 1);

        assert_eq!(link.request("start", json!({"unit": "vite"})), Err(line));
        assert_eq!(seen.recv().expect("handshake seen"), 1);
        assert!(seen.try_recv().is_err(), "a request went out under skew");

        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn nothing_listening_and_nothing_to_start_complains_rather_than_guessing_a_path() {
        let root = temp_root("silent");
        let endpoint = Endpoint::at(&root);
        let complaint = endpoint.connect_within(Duration::ZERO).unwrap_err();
        assert!(complaint.contains("daemon.sock"), "{complaint}");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
