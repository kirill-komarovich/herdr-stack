//! The agent-facing CLI as an agent meets it: the binary, argv, and one JSON document on stdout.
//!
//! The unit under test is a python listener of this file's own making, on a port the kernel just
//! handed out, in a throwaway project under a state root of its own — never the spelled-out one,
//! whose daemon is the user's own stack.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::Value;

use herdr_stack::client::Endpoint;

const PATIENCE: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(100);

fn exe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_herdr-stack"))
}

/// Not `TMPDIR`: macOS puts it under `/var/folders/…`, and a socket path built from it outgrows
/// `SUN_LEN` before it reaches `daemon.sock`.
fn staging(name: &str) -> PathBuf {
    PathBuf::from("/tmp").join(name)
}

/// A port the kernel says is free, given back before the unit takes it: a fixed number would collide
/// with whatever the machine running the test happens to be serving.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("a free port")
        .local_addr()
        .expect("an address")
        .port()
}

fn listener(port: u16) -> String {
    format!(
        "import socket, time\n\
         s = socket.socket()\n\
         s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n\
         s.bind(('127.0.0.1', {port}))\n\
         s.listen()\n\
         time.sleep(600)\n"
    )
}

struct Scratch {
    root: PathBuf,
    project: PathBuf,
}

impl Scratch {
    fn new(name: &str, manifest: &str) -> Scratch {
        let root = staging(&format!("herdr-stack-api-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("storefront");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(project.join(".herdr-stack.toml"), manifest).expect("manifest");
        Scratch { root, project }
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The daemon this test started, so the verbs reach one it can also take away again. A verb would
/// start one of its own otherwise, and nothing would be left holding the child to kill.
struct Daemon {
    child: Child,
}

impl Daemon {
    fn serving(state: &Path) -> Daemon {
        std::fs::create_dir_all(state).expect("state root");
        let child = Endpoint::at(state)
            .command(exe())
            .spawn()
            .expect("daemon spawns");
        Endpoint::at(state)
            .connect_within(PATIENCE)
            .expect("a link to the daemon");
        Daemon { child }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

struct Said {
    json: Value,
    code: i32,
}

/// The flags come before the units, which is the argv an agent is told to write: everything after a
/// verb's flags is a unit name.
fn run(scratch: &Scratch, mode: &str, units: &[&str]) -> Said {
    let output = Command::new(exe())
        .arg(mode)
        .arg("--project")
        .arg(&scratch.project)
        .arg("--state-root")
        .arg(scratch.state())
        .args(units)
        .output()
        .expect("herdr-stack runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    Said {
        json: serde_json::from_str(&stdout).unwrap_or_else(|error| {
            panic!(
                "{mode} {units:?} printed no JSON ({error}): {stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            )
        }),
        code: output.status.code().expect("an exit status"),
    }
}

fn unit<'a>(said: &'a Said, name: &str) -> &'a Value {
    said.json["units"]
        .as_array()
        .expect("units")
        .iter()
        .find(|unit| unit["unit"] == name)
        .unwrap_or_else(|| panic!("no unit {name} in {}", said.json))
}

fn until(mut settled: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if settled() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

#[test]
fn a_status_read_lists_every_declared_unit_without_starting_a_daemon() {
    let scratch = Scratch::new(
        "read",
        "[local.web]\ncmd = [\"sleep\", \"600\"]\n[docker]\nnames = [\"db\"]\n[docker.notes]\ndb = \"needs migrate\"\n",
    );

    let said = run(&scratch, "status", &[]);
    assert_eq!(said.code, 0);
    assert_eq!(said.json["daemon"], Value::Null);
    assert_eq!(said.json["project"]["name"], "storefront");

    let web = unit(&said, "local-web");
    assert_eq!(web["state"], "down");
    assert_eq!(web["kind"], "local");
    assert_eq!(web["cmd"], serde_json::json!(["sleep", "600"]));
    assert!(web["log"].as_str().expect("a log path").ends_with(".log"));

    let db = unit(&said, "docker-db");
    assert_eq!(db["manifest_note"], "needs migrate");

    assert!(
        !scratch.state().join("daemon.sock").exists(),
        "a read started a daemon"
    );
}

#[test]
fn a_unit_started_through_the_cli_reports_its_pid_and_the_port_it_is_listening_on() {
    let port = free_port();
    let scratch = Scratch::new(
        "ports",
        &format!(
            "[local.web]\ncmd = [\"python3\", \"-c\", {}]\n",
            serde_json::to_string(&listener(port)).expect("a quoted script")
        ),
    );
    let _daemon = Daemon::serving(&scratch.state());

    let started = run(&scratch, "start", &["web"]);
    assert_eq!(started.code, 0, "{}", started.json);
    assert_eq!(started.json["results"][0]["ok"], true);

    let listening = until(|| {
        let web = run(&scratch, "status", &[]);
        unit(&web, "local-web")["ports"] == serde_json::json!([port])
    });
    let said = run(&scratch, "status", &[]);
    let web = unit(&said, "local-web");
    assert!(listening, "no port on {web}");
    assert_eq!(web["state"], "up");
    assert!(web["pid"].as_u64().expect("a pid") > 1);
    assert!(said.json["daemon"]["pid"].as_u64().is_some());

    let stopped = run(&scratch, "stop", &["web"]);
    assert_eq!(stopped.code, 0, "{}", stopped.json);
    let said = run(&scratch, "status", &[]);
    let web = unit(&said, "local-web");
    assert_eq!(web["state"], "down");
    assert_eq!(
        web["ports"],
        serde_json::json!([]),
        "a stopped unit still holds a port"
    );
}

#[test]
fn a_selector_nothing_matches_fails_that_selector_alone() {
    let scratch = Scratch::new(
        "selector",
        "[local.web]\ncmd = [\"sleep\", \"600\"]\n[local.worker]\ncmd = [\"sleep\", \"600\"]\n",
    );
    let _daemon = Daemon::serving(&scratch.state());

    let said = run(&scratch, "stop", &["nope", "web"]);
    assert_eq!(said.code, 1);
    assert_eq!(said.json["results"][0]["ok"], false);
    assert!(
        said.json["results"][0]["error"]
            .as_str()
            .expect("a complaint")
            .contains("nope")
    );
    assert_eq!(said.json["results"][1]["ok"], true);
}
