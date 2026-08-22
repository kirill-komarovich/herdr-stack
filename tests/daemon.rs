//! The daemon as a real process, driven over its socket. Every daemon started here is killed here.

use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use herdr_dev::client::Endpoint;
use herdr_dev::{daemon, state};

const PATIENCE: Duration = Duration::from_secs(5);

fn exe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_herdr-dev"))
}

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("herdr-dev-daemon-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("scratch root");
    root
}

/// Owns the daemon it started: nothing here may outlive the test that spawned it.
struct Started {
    child: Child,
    root: PathBuf,
}

impl Started {
    fn at(root: &Path) -> Started {
        let child = Endpoint::at(root)
            .command(exe())
            .spawn()
            .expect("daemon spawns");
        Started {
            child,
            root: root.to_path_buf(),
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Runs a second `daemon` with its output captured, which the spawn path deliberately discards.
fn run_daemon_capturing(root: &Path) -> std::process::Output {
    let mut command = Endpoint::at(root).command(exe());
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command.output().expect("second daemon runs")
}

#[test]
fn a_daemon_forked_from_this_executable_answers_a_handshake_from_its_own_session() {
    let root = scratch("handshake");
    let mut started = Started::at(&root);

    let link = Endpoint::at(&root)
        .connect_within(PATIENCE)
        .expect("handshake");
    let peer = link.peer();
    assert_eq!(peer.pid, started.pid());
    assert_eq!(peer.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(peer.protocol, daemon::PROTOCOL);
    assert!(!link.skewed());
    assert!(link.footer().contains(&format!("pid {}", started.pid())));

    // Its own session is the whole reason it outlives the popup, the Herdr server and a live handoff.
    let session = unsafe { libc::getsid(started.pid() as i32) };
    assert_eq!(session, started.pid() as i32);
    assert_ne!(session, unsafe { libc::getsid(0) });

    assert!(started.alive());
}

#[test]
fn a_daemon_nobody_has_connected_to_yet_is_still_there_when_the_first_request_lands() {
    let root = scratch("grace");
    let mut started = Started::at(&root);
    std::thread::sleep(Duration::from_millis(750));
    assert!(
        started.alive(),
        "the daemon vanished before its first client"
    );
    assert!(
        Endpoint::at(&root)
            .connect_within(PATIENCE)
            .is_ok_and(|link| link.peer().pid == started.pid())
    );
}

#[test]
fn a_second_daemon_fails_the_lock_and_exits_silently_without_disturbing_the_first() {
    let root = scratch("singleton");
    let mut first = Started::at(&root);
    let held = Endpoint::at(&root)
        .connect_within(PATIENCE)
        .expect("handshake")
        .peer()
        .pid;
    assert_eq!(held, first.pid());

    let output = run_daemon_capturing(&root);
    assert!(output.status.success(), "{:?}", output.status);
    assert!(
        output.stdout.is_empty(),
        "the second daemon spoke on stdout"
    );
    assert!(
        output.stderr.is_empty(),
        "the second daemon spoke on stderr"
    );

    assert!(first.alive());
    let mut link = Endpoint::at(&root)
        .connect_within(PATIENCE)
        .expect("the first daemon still answers");
    assert_eq!(link.peer().pid, first.pid());
    assert!(link.request("handshake", json!({})).is_ok());
}

#[test]
fn a_stale_socket_file_is_unlinked_and_replaced_without_a_word() {
    let root = scratch("stale");
    let socket = state::socket_path(&root);
    drop(UnixListener::bind(&socket).expect("stale listener"));
    assert!(socket.exists());
    let stale = socket.metadata().expect("stale socket").ino();
    assert!(
        Endpoint::at(&root).connect_within(Duration::ZERO).is_err(),
        "a stale socket must fail to connect"
    );

    let mut command = Endpoint::at(&root).command(exe());
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().expect("daemon spawns");
    let mut started = Started {
        child,
        root: root.clone(),
    };

    let link = Endpoint::at(&root)
        .connect_within(PATIENCE)
        .expect("handshake over the replacement socket");
    assert_eq!(link.peer().pid, started.pid());
    assert_ne!(
        socket.metadata().expect("fresh socket").ino(),
        stale,
        "the stale socket file was reused"
    );
    assert!(started.alive());
}

#[test]
fn a_client_holding_the_link_keeps_the_daemon_past_its_idle_window() {
    let root = scratch("idle");
    let serving = {
        let root = root.clone();
        std::thread::spawn(move || daemon::serve_with_idle(&root, Duration::from_millis(200)))
    };

    let link = Endpoint::at(&root)
        .connect_within(PATIENCE)
        .expect("handshake");
    std::thread::sleep(Duration::from_millis(700));
    assert!(!serving.is_finished(), "a connected client was not enough");

    drop(link);
    let deadline = Instant::now() + PATIENCE;
    while !serving.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        serving.join().expect("serve returns").expect("served"),
        daemon::Outcome::Idle
    );
    assert!(
        !state::socket_path(&root).exists(),
        "an idle daemon left its socket behind"
    );

    std::fs::remove_dir_all(&root).expect("cleanup");
}

#[test]
fn a_daemon_that_cannot_take_the_lock_reports_itself_redundant() {
    let root = scratch("redundant");
    let started = Started::at(&root);
    Endpoint::at(&root)
        .connect_within(PATIENCE)
        .expect("handshake");

    assert_eq!(
        daemon::serve_with_idle(&root, Duration::from_millis(50)).expect("serve returns"),
        daemon::Outcome::Redundant
    );
    assert!(
        state::socket_path(&root).exists(),
        "the redundant daemon touched the socket"
    );
    drop(started);
}
