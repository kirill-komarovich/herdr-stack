//! Docker rows over the daemon's socket.
//!
//! The tests that run by default create nothing: they point the daemon at a docker socket that does
//! not exist, which is how `unknown` and its stale cache are exercised without a container
//! anywhere near them.
//!
//! The test marked `#[ignore]` does create containers — a throwaway compose project of its own under
//! the temporary directory, from `alpine:latest` in the local image store, pulling nothing — and takes
//! it down again on the way out. Run it with `cargo test -- --ignored`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use herdr_stack::client::{Endpoint, Link, Target};
use herdr_stack::docker;
use herdr_stack::manifest::Project;
use herdr_stack::store::{Cache, Identity, Store};
use herdr_stack::unit::{self, State, Status};

const PATIENCE: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(100);

fn exe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_herdr-stack"))
}

/// A temporary directory holding the state root and the project the rows are about.
struct Scratch {
    root: PathBuf,
    composed: Option<PathBuf>,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = std::env::temp_dir().join(format!("herdr-stack-docker-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch");
        Scratch {
            root,
            composed: None,
        }
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    /// The directory basename is what compose would take its project name from, and compose addresses
    /// a project by *name* rather than by path: a directory called `storefront` here would let `down`
    /// reach the real storefront. So the basename is made unmistakably this process's.
    fn project(&self, manifest: &str) -> Project {
        let dir = self.root.join(format!("herdrstack{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("project dir");
        let path = dir.join(".herdr-stack.toml");
        std::fs::write(&path, manifest).expect("manifest");
        Project::load(&path).expect("manifest parses")
    }

    /// The compose file goes to the project root and is found by default discovery: nothing anywhere
    /// passes `-f`, which is as much a part of what is being tested as the state mapping. The project
    /// name is pinned rather than left to fall out of the directory, belt and braces.
    fn compose(&mut self, project: &Project, services: &str) {
        let yaml = format!("name: {}\n{services}", project.name);
        std::fs::write(project.root.join("docker-compose.yml"), yaml).expect("compose file");
        self.composed = Some(project.root.clone());
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(root) = self.composed.take() {
            let _ = Command::new(docker::path())
                .arg("compose")
                .args(["down", "--remove-orphans", "-v"])
                .current_dir(&root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The daemon this test started, and nothing it did not.
struct Daemon {
    child: Child,
    root: PathBuf,
}

impl Daemon {
    /// `docker_host` is what the daemon will see; `None` leaves the real one alone.
    fn serving(root: &Path, docker_host: Option<&str>) -> Daemon {
        let mut command = Endpoint::at(root).command(exe());
        if let Some(host) = docker_host {
            command.env("DOCKER_HOST", host);
        }
        let child = command.spawn().expect("daemon spawns");
        let daemon = Daemon {
            child,
            root: root.to_path_buf(),
        };
        daemon.link();
        daemon
    }

    fn link(&self) -> Link {
        Endpoint::at(&self.root)
            .connect_within(PATIENCE)
            .expect("a link to the daemon")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn identity(project: &Project) -> Identity {
    Identity {
        path: project.root.clone(),
        name: project.name.clone(),
    }
}

fn status_of(link: &mut Link, project: &Project, name: &str) -> Status {
    link.status(project)
        .expect("status")
        .remove(&unit::key(unit::DOCKER, name))
        .unwrap_or_else(|| Status::of(State::Down))
}

fn service<'a>(project: &'a Project, name: &str) -> Target<'a> {
    Target::of(project, unit::DOCKER, name).expect("the manifest declares the service")
}

/// Reads until the state asked for, because a healthcheck settles on docker's clock and not on ours.
fn settles(link: &mut Link, project: &Project, name: &str, wanted: State) -> Status {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let status = status_of(link, project, name);
        if status.state == wanted || Instant::now() >= deadline {
            return status;
        }
        std::thread::sleep(POLL);
    }
}

fn nowhere() -> String {
    format!(
        "unix://{}",
        std::env::temp_dir()
            .join(format!("herdr-stack-no-docker-{}.sock", std::process::id()))
            .display()
    )
}

const MANIFEST: &str = "[docker]\n\
                        names = [\"plain\", \"checked\", \"sick\", \"oneshot\", \"boom\", \"never\"]\n\
                        one_shot = [\"oneshot\"]\n\
                        [docker.notes]\n\
                        plain = \"no healthcheck at all\"\n";

/// Every awkward case the state model has a row for: a service with no healthcheck, one that passes a healthcheck,
/// one that fails it while plainly running, a one-shot that exits 0, one that exits non-zero, and one
/// that is declared and never created.
const SERVICES: &str = r#"services:
  plain:
    image: alpine:latest
    command: ["sleep", "100000"]
  checked:
    image: alpine:latest
    command: ["sleep", "100000"]
    healthcheck:
      test: ["CMD", "true"]
      interval: 1s
      retries: 1
      start_period: 0s
  sick:
    image: alpine:latest
    command: ["sleep", "100000"]
    healthcheck:
      test: ["CMD", "false"]
      interval: 1s
      retries: 1
      start_period: 0s
  oneshot:
    image: alpine:latest
    command: ["true"]
  boom:
    image: alpine:latest
    command: ["sh", "-c", "exit 7"]
  never:
    image: alpine:latest
    command: ["sleep", "100000"]
"#;

#[test]
fn a_docker_row_the_daemon_cannot_read_is_unknown_rather_than_guessed_at() {
    let mut scratch = Scratch::new("unreachable");
    let project = scratch.project(MANIFEST);
    scratch.compose(&project, SERVICES);
    let daemon = Daemon::serving(&scratch.state(), Some(&nowhere()));
    let mut link = daemon.link();

    let units = link.status(&project).expect("status");
    assert_eq!(units.len(), project.docker.len());
    for service in &project.docker {
        let status = &units[&unit::key(unit::DOCKER, &service.name)];
        assert_eq!(status.state, State::Unknown, "{}", service.name);
        assert_eq!(status.uptime, None, "{}", service.name);
        assert!(
            status
                .note
                .as_deref()
                .is_some_and(|note| note.to_lowercase().contains("docker")),
            "{status:?}"
        );
    }
}

#[test]
fn a_row_from_the_cache_is_marked_stale_and_never_pretended_live() {
    let mut scratch = Scratch::new("stale");
    let project = scratch.project(MANIFEST);
    scratch.compose(&project, SERVICES);
    let state = scratch.state();
    let slot = Store::at(&state).open(&identity(&project)).expect("slot");
    slot.write_cache(
        &unit::key(unit::DOCKER, "plain"),
        &Cache {
            state: "running".into(),
            health: "healthy".into(),
            exit_code: 0,
            seen_at: SystemTime::now() - Duration::from_secs(90),
            uptime: Some(Duration::from_secs(2460)),
        },
    )
    .expect("a cached reading");

    let daemon = Daemon::serving(&state, Some(&nowhere()));
    let mut link = daemon.link();
    let status = status_of(&mut link, &project, "plain");

    assert_eq!(status.state, State::Unknown);
    assert_eq!(
        status.timing(),
        "",
        "a stale row claims no uptime of its own"
    );
    let note = status.note.expect("a stale note");
    assert!(note.starts_with("stale: up 41m00s, seen 1m3"), "{note}");
}

#[test]
fn a_verb_on_a_docker_row_is_answered_by_the_daemon_rather_than_refused() {
    let mut scratch = Scratch::new("verb");
    let project = scratch.project(MANIFEST);
    scratch.compose(&project, SERVICES);
    let daemon = Daemon::serving(&scratch.state(), Some(&nowhere()));
    let mut link = daemon.link();

    // Docker is not there, so each verb comes back as a note about the connection — an answer, which
    // is what a docker row never got while the verbs were local-only.
    for verb in ["start", "stop", "restart"] {
        let asked = match verb {
            "start" => link.start(&project, &service(&project, "plain")),
            "stop" => link.stop(&project, &service(&project, "plain")),
            _ => link.restart(&project, &service(&project, "plain")),
        };
        let note = asked.expect("an answer").expect("a note");
        assert!(note.contains("connect"), "{verb}: {note}");
    }
}

#[test]
#[ignore = "creates and destroys its own compose project; needs docker and alpine:latest"]
fn every_row_of_the_state_mapping_reads_the_way_docker_actually_reports_it() {
    let mut scratch = Scratch::new("mapping");
    let project = scratch.project(MANIFEST);
    scratch.compose(&project, SERVICES);
    assert!(
        Command::new(docker::path())
            .args(["image", "inspect", "alpine:latest"])
            .output()
            .is_ok_and(|output| output.status.success()),
        "alpine:latest must already be in the local image store; nothing here pulls"
    );
    let daemon = Daemon::serving(&scratch.state(), None);
    let mut link = daemon.link();

    for name in ["plain", "checked", "sick", "boom", "oneshot"] {
        link.start(&project, &service(&project, name))
            .unwrap_or_else(|error| panic!("start {name}: {error}"));
    }

    let plain = settles(&mut link, &project, "plain", State::Up);
    assert_eq!(
        plain.state,
        State::Up,
        "a service with no healthcheck is up"
    );
    assert!(plain.uptime.is_some_and(|uptime| uptime < PATIENCE));
    assert_eq!(plain.note, None);

    assert_eq!(
        settles(&mut link, &project, "checked", State::Up).state,
        State::Up,
        "a service that passes its healthcheck is up"
    );

    let sick = settles(&mut link, &project, "sick", State::Up);
    assert_eq!(sick.state, State::Up, "an unhealthy service is still up");
    assert_eq!(sick.note.as_deref(), Some("unhealthy"));
    assert!(sick.uptime.is_some());

    let done = settles(&mut link, &project, "oneshot", State::Done);
    assert_eq!(done.state, State::Done, "a declared one-shot that exited");
    assert_eq!(done.timing(), "exit 0");
    assert_eq!(done.uptime, None);

    let boom = settles(&mut link, &project, "boom", State::Down);
    assert_eq!(
        boom.state,
        State::Down,
        "an exited service is not a one-shot"
    );
    assert_eq!(boom.timing(), "exit 7");

    let never = status_of(&mut link, &project, "never");
    assert_eq!(never.state, State::Down, "a service never created is down");
    assert_eq!(never.timing(), "");

    // There is no `dead` for docker: `compose stop` exits 137, indistinguishable from a kill or an OOM.
    assert_eq!(
        link.stop(&project, &service(&project, "plain")),
        Ok(None),
        "stopping one service is not a refusal"
    );
    let stopped = settles(&mut link, &project, "plain", State::Down);
    assert_eq!(stopped.state, State::Down);
    assert_eq!(stopped.timing(), "exit 137");
    assert_eq!(
        status_of(&mut link, &project, "checked").state,
        State::Up,
        "a stop must never take the rest of the project with it"
    );

    link.restart(&project, &service(&project, "plain"))
        .expect("restart");
    let restarted = settles(&mut link, &project, "plain", State::Up);
    assert_eq!(restarted.state, State::Up, "a restart starts it again");
    assert!(restarted.uptime.is_some_and(|uptime| uptime < PATIENCE));

    // The display cache: the reading, not the row, and written on every read.
    let cached = Store::at(scratch.state())
        .slot(&identity(&project))
        .cache(&unit::key(unit::DOCKER, "plain"))
        .expect("a cached reading");
    assert_eq!(cached.state, "running");
    assert!(cached.age() < PATIENCE);
}
