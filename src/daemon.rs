//! The daemon: one per machine, started by the TUI, sole owner of the stack.
//!
//! It speaks line-delimited JSON on `daemon.sock` in the state root — the same `{id, method, params}`
//! request and `{id, result | error}` reply shape as Herdr's own socket, so one wire format serves the
//! whole plugin.
//!
//! Singleton-ness is an exclusive `flock` on `daemon.lock` held for the process's whole life. Herdr
//! provides no such guard, and a second daemon must not disturb the first, so failing the lock is a
//! silent success.
//!
//! It is also the parent of every local unit, which is what makes *daemon alive ⇔ stack alive* true:
//! leftovers of a previous life are killed before it serves a single request, and everything it owns
//! is killed on the way out.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::local::Spec;
use crate::store::Identity;
use crate::supervisor::{GRACE, Supervisor};
use crate::{docker, state, unit};

/// Bumped only on a wire-breaking change. A rebuild changes `version`, never this.
pub const PROTOCOL: u64 = 2;

/// §7 says the daemon exits when it owns nothing, but at startup it owns nothing by definition and
/// the TUI that just forked it has not connected yet. So "owns nothing" is measured over a window,
/// and a connected client counts as ownership: a fresh daemon has this long to receive its first
/// request, and a popup left open never times out underneath itself.
const IDLE_WINDOW: Duration = Duration::from_secs(60);

const ACCEPT_POLL: Duration = Duration::from_millis(25);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Served until it owned nothing for a whole idle window.
    Idle,
    /// Another daemon holds the lock; this process is redundant and says nothing.
    Redundant,
    /// Signalled, and took the stack with it.
    Signalled,
}

/// A signal handler may do nothing but this, so the accept loop is what actually shuts the stack down.
static SIGNALLED: AtomicBool = AtomicBool::new(false);

extern "C" fn note_signal(_signal: i32) {
    SIGNALLED.store(true, Ordering::SeqCst);
}

/// `SIGKILL` cannot be caught, which is exactly why kill-leftovers-on-start exists as well.
fn catch_signals() {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe { libc::signal(signal, note_signal as *const () as libc::sighandler_t) };
    }
}

pub fn serve(root: &Path) -> std::io::Result<Outcome> {
    serve_with_idle(root, IDLE_WINDOW)
}

pub fn serve_with_idle(root: &Path, idle: Duration) -> std::io::Result<Outcome> {
    std::fs::create_dir_all(root)?;
    let Some(_lock) = claim(&state::lock_path(root))? else {
        return Ok(Outcome::Redundant);
    };
    catch_signals();

    // Before anything else: whatever the predecessor left running dies, and the projects that have
    // since been deleted go with it.
    let supervisor = Supervisor::new(root, GRACE);
    supervisor.kill_leftovers();
    supervisor.store().drop_vanished();

    let socket = state::socket_path(root);
    // Holding the lock means any socket file here is the leftover of a daemon that died without
    // unlinking, so replacing it is safe by construction rather than by inspection.
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;

    let ledger = Arc::new(Ledger::new());
    let mut outcome = Outcome::Idle;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let ledger = Arc::clone(&ledger);
                let supervisor = Arc::clone(&supervisor);
                ledger.arrive();
                std::thread::spawn(move || {
                    converse(stream, &supervisor);
                    ledger.depart();
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if SIGNALLED.load(Ordering::SeqCst) {
                    outcome = Outcome::Signalled;
                    break;
                }
                // A running unit is ownership too, so the last unit stopping is the shutdown — but
                // only once no client is holding the popup open either.
                if ledger.owned_nothing_for(idle) && !supervisor.busy() {
                    break;
                }
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(error) => return Err(error),
        }
    }

    supervisor.kill_all();
    let _ = std::fs::remove_file(&socket);
    Ok(outcome)
}

/// `None` when another daemon holds the lock. The returned file must outlive the daemon's work: the
/// lock is released by closing it.
fn claim(path: &Path) -> std::io::Result<Option<File>> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

fn handshake() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": PROTOCOL,
        "pid": std::process::id(),
    })
}

fn converse(stream: UnixStream, supervisor: &Arc<Supervisor>) {
    // BSD hands an accepted socket the listener's `O_NONBLOCK`, which would turn the first blocking
    // read into an error and hang up on a client that is merely idle.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(CLIENT_TIMEOUT));
    let Ok(reading) = stream.try_clone() else {
        return;
    };
    let mut writer = &stream;
    for line in BufReader::new(reading).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if writeln!(writer, "{}", answer(&line, supervisor)).is_err() {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

fn answer(line: &str, supervisor: &Arc<Supervisor>) -> Value {
    let request: Value = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(error) => return failure(Value::Null, "malformed", &error.to_string()),
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    match request.get("method").and_then(Value::as_str) {
        Some("handshake") => json!({"id": id, "result": handshake()}),
        Some("start") => verb(id, &params, Act::Start, supervisor),
        Some("restart") => verb(id, &params, Act::Restart, supervisor),
        Some("stop") => verb(id, &params, Act::Stop, supervisor),
        Some("status") => match project(&params) {
            Err(complaint) => failure(id, "malformed", &complaint),
            Ok(project) => {
                // §9's two liveness authorities meet here and nowhere else: this daemon's own
                // `wait()` for the units it forked, `compose ps --all` for what docker holds.
                let mut statuses = supervisor.status(&project);
                statuses.extend(docker::statuses(
                    supervisor.store(),
                    &project,
                    &services(&params),
                ));
                let units: BTreeMap<String, Value> = statuses
                    .iter()
                    .map(|(unit, status)| (unit.clone(), status.to_value()))
                    .collect();
                json!({"id": id, "result": {"units": units}})
            }
        },
        Some(method) => failure(id, "unknown_method", &format!("no method `{method}`")),
        None => failure(id, "malformed", "request carries no method"),
    }
}

#[derive(Debug, Clone, Copy)]
enum Act {
    Start,
    Stop,
    Restart,
}

/// Which half of §9 a verb lands on. The daemon is the parent of a local unit and merely a caller of
/// `docker compose` for a service, so the two share a wire and nothing else.
enum Target {
    Local(Spec),
    Docker(docker::Service),
}

/// Every verb names a project and a unit, so they are parsed once here; a verb that is refused rather
/// than performed — a unit another project holds, a service docker would not start — is a normal
/// answer with a note, not an error.
fn verb(id: Value, params: &Value, act: Act, supervisor: &Arc<Supervisor>) -> Value {
    let asked = project(params).and_then(|project| Ok((project, target(params)?)));
    let (project, target) = match asked {
        Err(complaint) => return failure(id, "malformed", &complaint),
        Ok(asked) => asked,
    };
    let done = match (&target, act) {
        (Target::Local(spec), Act::Start) => supervisor.start(&project, spec),
        (Target::Local(spec), Act::Stop) => supervisor.stop(&project, &spec.name),
        (Target::Local(spec), Act::Restart) => supervisor.restart(&project, spec),
        (Target::Docker(service), Act::Start) => docker::start(&project.path, service),
        (Target::Docker(service), Act::Stop) => docker::stop(&project.path, service),
        (Target::Docker(service), Act::Restart) => docker::restart(&project.path, service),
    };
    match done {
        Err(complaint) => failure(id, target.code(), &complaint),
        Ok(verdict) => json!({"id": id, "result": {"note": verdict.note}}),
    }
}

impl Target {
    fn code(&self) -> &'static str {
        match self {
            Target::Local(_) => "spawn",
            Target::Docker(_) => "docker",
        }
    }
}

fn project(params: &Value) -> Result<Identity, String> {
    let project = params.get("project").ok_or("no project")?;
    let path = project
        .get("path")
        .and_then(Value::as_str)
        .ok_or("project without a path")?;
    Ok(Identity {
        path: PathBuf::from(path),
        name: project
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// The kind is required rather than defaulted: a service taken for a local unit would be spawned as a
/// process with no command at all.
fn target(params: &Value) -> Result<Target, String> {
    let unit = params.get("unit").ok_or("no unit")?;
    let name = unit
        .get("name")
        .and_then(Value::as_str)
        .ok_or("unit without a name")?
        .to_string();
    match unit.get("kind").and_then(Value::as_str) {
        None => Err("unit without a kind".to_string()),
        Some(unit::DOCKER) => Ok(Target::Docker(docker::Service {
            name,
            one_shot: one_shot(unit),
        })),
        Some(unit::LOCAL) => Ok(Target::Local(Spec {
            name,
            cmd: strings(unit.get("cmd")),
            cwd: PathBuf::from(unit.get("cwd").and_then(Value::as_str).unwrap_or_default()),
            env: match unit.get("env").and_then(Value::as_object) {
                None => BTreeMap::new(),
                Some(env) => env
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                    .collect(),
            },
        })),
        Some(kind) => Err(format!("unit of unknown kind `{kind}`")),
    }
}

/// The declared services a status read is about. A project with none is never asked about docker at
/// all, which is what keeps a purely local stack off the compose path.
fn services(params: &Value) -> Vec<docker::Service> {
    params
        .get("docker")
        .and_then(Value::as_array)
        .map(|declared| {
            declared
                .iter()
                .filter_map(|service| {
                    Some(docker::Service {
                        name: service.get("name")?.as_str()?.to_string(),
                        one_shot: one_shot(service),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn one_shot(service: &Value) -> bool {
    service
        .get("one_shot")
        .and_then(Value::as_bool)
        .unwrap_or_default()
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|words| {
            words
                .iter()
                .filter_map(|word| word.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn failure(id: Value, code: &str, message: &str) -> Value {
    json!({"id": id, "error": {"code": code, "message": message}})
}

/// What the daemon owns, and since when it owned nothing.
struct Ledger {
    clients: AtomicUsize,
    empty_since: Mutex<Option<Instant>>,
}

impl Ledger {
    fn new() -> Ledger {
        Ledger {
            clients: AtomicUsize::new(0),
            empty_since: Mutex::new(Some(Instant::now())),
        }
    }

    fn arrive(&self) {
        self.clients.fetch_add(1, Ordering::SeqCst);
        *self.empty_since.lock().expect("ledger") = None;
    }

    fn depart(&self) {
        if self.clients.fetch_sub(1, Ordering::SeqCst) == 1 {
            *self.empty_since.lock().expect("ledger") = Some(Instant::now());
        }
    }

    fn owned_nothing_for(&self, window: Duration) -> bool {
        matches!(*self.empty_since.lock().expect("ledger"), Some(since) if since.elapsed() >= window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every request in this module is answered without a project, so no unit is ever spawned.
    fn supervisor() -> Arc<Supervisor> {
        Supervisor::new(
            &std::env::temp_dir().join(format!("herdr-dev-answer-{}", std::process::id())),
            GRACE,
        )
    }

    #[test]
    fn a_handshake_carries_this_build_its_protocol_and_the_daemon_pid() {
        let reply = answer(
            &json!({"id": "t1", "method": "handshake"}).to_string(),
            &supervisor(),
        );
        assert_eq!(reply["id"], "t1");
        assert_eq!(reply["result"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(reply["result"]["protocol"], PROTOCOL);
        assert_eq!(reply["result"]["pid"], std::process::id());
    }

    #[test]
    fn an_unknown_method_is_refused_by_name_rather_than_dropped() {
        let reply = answer(
            &json!({"id": 7, "method": "levitate"}).to_string(),
            &supervisor(),
        );
        assert_eq!(reply["error"]["code"], "unknown_method");
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("levitate")
        );
        assert_eq!(reply["id"], 7);
    }

    #[test]
    fn a_line_that_is_not_a_request_still_gets_a_reply() {
        let supervisor = supervisor();
        assert_eq!(
            answer("{not json", &supervisor)["error"]["code"],
            "malformed"
        );
        assert_eq!(
            answer("{\"id\": 1}", &supervisor)["error"]["code"],
            "malformed"
        );
    }

    #[test]
    fn a_verb_without_a_kind_is_refused_rather_than_taken_for_either_half() {
        let supervisor = supervisor();
        // Only the shapes that fail to parse are asked here: a well-formed `local` would spawn.
        for unit in [
            json!({"name": "db"}),
            json!({"name": "db", "kind": "container"}),
        ] {
            let request = json!({
                "id": 1,
                "method": "start",
                "params": {"project": {"path": "/repos/storefront"}, "unit": unit},
            });
            let reply = answer(&request.to_string(), &supervisor);
            assert_eq!(reply["error"]["code"], "malformed", "{reply}");
        }
    }

    #[test]
    fn a_status_read_is_told_which_services_it_is_about_and_which_of_them_are_one_shots() {
        assert_eq!(
            services(&json!({"docker": [{"name": "db"}, {"name": "seed", "one_shot": true}]})),
            vec![
                docker::Service {
                    name: "db".into(),
                    one_shot: false
                },
                docker::Service {
                    name: "seed".into(),
                    one_shot: true
                },
            ]
        );
        assert!(services(&json!({})).is_empty());
    }

    #[test]
    fn the_second_claim_on_a_lockfile_fails_while_the_first_is_held() {
        let dir = std::env::temp_dir().join(format!("herdr-dev-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("daemon.lock");

        let first = claim(&path).expect("first claim").expect("first holds it");
        assert!(claim(&path).expect("second claim").is_none());
        drop(first);
        // A lock can outlive the descriptor that took it by the width of a concurrent fork: a child
        // inherits the descriptor until its own `exec` closes it, and other tests here spawn units.
        let released = (0..100).any(|_| {
            let taken = claim(&path).expect("third claim").is_some();
            if !taken {
                std::thread::sleep(Duration::from_millis(20));
            }
            taken
        });
        assert!(released, "the lock outlived the descriptor that took it");

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_client_holds_the_idle_window_open_and_releasing_it_starts_the_clock() {
        let ledger = Ledger::new();
        let window = Duration::from_millis(60);
        std::thread::sleep(Duration::from_millis(80));
        assert!(ledger.owned_nothing_for(window));

        ledger.arrive();
        std::thread::sleep(Duration::from_millis(80));
        assert!(!ledger.owned_nothing_for(window));

        ledger.depart();
        assert!(!ledger.owned_nothing_for(window));
        std::thread::sleep(Duration::from_millis(80));
        assert!(ledger.owned_nothing_for(window));
    }

    #[test]
    fn two_clients_leaving_one_at_a_time_do_not_start_the_clock_early() {
        let ledger = Ledger::new();
        ledger.arrive();
        ledger.arrive();
        ledger.depart();
        assert!(!ledger.owned_nothing_for(Duration::ZERO));
        ledger.depart();
        assert!(ledger.owned_nothing_for(Duration::ZERO));
    }
}
