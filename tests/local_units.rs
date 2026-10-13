//! Local units as real processes, driven over the daemon's socket.
//!
//! Every unit here is a `sleep` or a shell loop of this file's own making, in a throwaway project
//! under the temporary directory, under a state root of its own — never the spelled-out one, whose
//! records name processes this test did not spawn. The only pids signalled directly are ones it
//! started itself, and every test leaves its own process group empty.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use herdr_stack::client::{Endpoint, Link, Target};
use herdr_stack::local;
use herdr_stack::manifest::Project;
use herdr_stack::store::{Identity, LOG_LINK, Record, Store};
use herdr_stack::unit::{self, Exit, State, Status};

const PATIENCE: Duration = Duration::from_secs(15);
/// The grace, which the escalation test measures rather than assumes.
const GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

fn exe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_herdr-stack"))
}

fn until(patience: Duration, mut settled: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + patience;
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

/// A temporary directory holding both the state root and the projects the units live in.
struct Scratch {
    root: PathBuf,
    trusted: Vec<PathBuf>,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = std::env::temp_dir().join(format!("herdr-stack-local-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch");
        Scratch {
            root,
            trusted: Vec::new(),
        }
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn project(&self, name: &str, manifest: &str) -> Project {
        let dir = self.root.join(name);
        std::fs::create_dir_all(&dir).expect("project dir");
        let path = dir.join(".herdr-stack.toml");
        std::fs::write(&path, manifest).expect("manifest");
        Project::load(&path).expect("manifest parses")
    }

    /// mise reads a `mise.toml` only from a directory it has been told to trust.
    fn trust(&mut self, project: &Project, mise: &str) {
        std::fs::write(project.root.join("mise.toml"), mise).expect("mise.toml");
        let trusted = Command::new(local::mise_path())
            .arg("trust")
            .arg(&project.root)
            .output()
            .expect("mise trust runs");
        assert!(trusted.status.success(), "{trusted:?}");
        self.trusted.push(project.root.clone());
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in &self.trusted {
            let _ = Command::new(local::mise_path())
                .arg("trust")
                .arg("--untrust")
                .arg(path)
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
    fn serving(root: &Path) -> Daemon {
        let child = Endpoint::at(root)
            .command(exe())
            .spawn()
            .expect("daemon spawns");
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

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn signal(&mut self, signal: i32) {
        assert_eq!(unsafe { libc::kill(self.pid() as i32, signal) }, 0);
    }

    /// Waits for the daemon to go, which is also waiting for kill-on-exit to finish.
    fn gone(&mut self) -> bool {
        until(PATIENCE, || !self.alive())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.alive() {
            self.signal(libc::SIGTERM);
            if !self.gone() {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
    }
}

fn unit_of<'a>(project: &'a Project, name: &str) -> Target<'a> {
    Target::of(project, unit::LOCAL, name).expect("the manifest declares the unit")
}

fn identity(project: &Project) -> Identity {
    Identity {
        path: project.root.clone(),
        name: project.name.clone(),
    }
}

fn record(state: &Path, project: &Project, name: &str) -> Option<Record> {
    Store::at(state)
        .slot(&identity(project))
        .record(&unit::key(unit::LOCAL, name))
}

fn pid_of(state: &Path, project: &Project, name: &str) -> u32 {
    record(state, project, name)
        .expect("a record")
        .pid
        .expect("a running unit has a pid")
}

fn status_of(link: &mut Link, project: &Project, name: &str) -> Status {
    link.status(project)
        .expect("status")
        .remove(&unit::key(unit::LOCAL, name))
        .unwrap_or_else(|| Status::of(State::Down))
}

fn log_of(state: &Path, project: &Project, name: &str) -> String {
    let path = Store::at(state)
        .slot(&identity(project))
        .log_path(&unit::key(unit::LOCAL, name));
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The pids a wrapper unit reported forking, read out of its own log.
fn children(log: &str) -> Vec<u32> {
    log.lines()
        .filter_map(|line| line.strip_prefix("child "))
        .filter_map(|pid| pid.trim().parse().ok())
        .collect()
}

#[test]
fn a_started_unit_runs_under_mise_in_its_own_session_with_its_output_in_the_log() {
    let mut scratch = Scratch::new("spawn");
    let project = scratch.project(
        "storefront",
        "[env]\n\
         HD_TOP = \"from-top\"\n\
         HD_UNIT = \"from-top\"\n\
         \n\
         [local.probe]\n\
         cmd = [\"sh\", \"-c\", \"echo mise=$HD_FROM_MISE top=$HD_TOP unit=$HD_UNIT; cat; echo stdin-closed; sleep 30\"]\n\
         \n\
         [local.probe.env]\n\
         HD_UNIT = \"from-unit\"\n",
    );
    scratch.trust(&project, "[env]\nHD_FROM_MISE = \"from-mise\"\n");
    let state = scratch.state();
    let daemon = Daemon::serving(&state);
    let mut link = daemon.link();

    assert_eq!(
        link.start(&project, &unit_of(&project, "probe"))
            .expect("start"),
        None
    );
    assert!(
        until(PATIENCE, || log_of(&state, &project, "probe")
            .contains("stdin-closed")),
        "the log never showed the unit running: {:?}",
        log_of(&state, &project, "probe")
    );

    let log = log_of(&state, &project, "probe");
    // mise supplied the repo's env, which is what keeps every port out of the manifest.
    assert!(log.contains("mise=from-mise"), "{log:?}");
    assert!(log.contains("top=from-top"), "{log:?}");
    // Innermost last: the unit's own env beats the manifest's `[env]`.
    assert!(log.contains("unit=from-unit"), "{log:?}");
    // `cat` returning at once is stdin coming from `/dev/null` rather than a terminal.
    assert!(log.contains("stdin-closed"), "{log:?}");

    let pid = pid_of(&state, &project, "probe");
    assert_eq!(
        unsafe { libc::getsid(pid as i32) },
        pid as i32,
        "a unit that does not lead its own session dies with the pane it was launched from"
    );

    let status = status_of(&mut link, &project, "probe");
    assert_eq!(status.state, State::Up);
    assert!(status.uptime.is_some());

    // The symlink is what makes `tail -f .herdr-stack-logs/local-probe.log` work from the project.
    let through_link = project.root.join(LOG_LINK).join("local-probe.log");
    assert!(
        std::fs::read_to_string(&through_link)
            .expect("the log reads through the project-root symlink")
            .contains("stdin-closed")
    );

    assert_eq!(link.stop(&project, &unit_of(&project, "probe")), Ok(None));
    assert!(
        local::group_empty(pid),
        "the unit's group outlived its stop"
    );
}

#[test]
fn a_unit_that_dies_at_once_keeps_the_code_it_exited_with_through_being_stopped() {
    let scratch = Scratch::new("boom");
    let project = scratch.project(
        "storefront",
        "[local.boom]\ncmd = [\"sh\", \"-c\", \"echo about to fail >&2; exit 7\"]\n",
    );
    let state = scratch.state();
    let daemon = Daemon::serving(&state);
    let mut link = daemon.link();
    let boom = unit_of(&project, "boom");

    link.start(&project, &boom).expect("start");
    assert!(
        until(PATIENCE, || status_of(&mut link, &project, "boom").state
            == State::Dead),
        "the daemon never witnessed the unit dying"
    );

    let dead = status_of(&mut link, &project, "boom");
    assert_eq!(dead.exit, Some(Exit::Code(7)));
    assert_eq!(dead.timing(), "exit 7");
    // stderr shares the log with stdout.
    assert!(log_of(&state, &project, "boom").contains("about to fail"));

    // Stopping something that already crashed must not erase how it crashed.
    assert_eq!(
        link.stop(&project, &boom).expect("stop"),
        Some("boom is not running".to_string())
    );
    let after = status_of(&mut link, &project, "boom");
    assert_eq!(
        (after.state, after.exit),
        (State::Dead, Some(Exit::Code(7)))
    );
}

#[test]
fn a_term_to_the_leader_orphans_the_children_that_a_group_stop_takes_with_it() {
    let scratch = Scratch::new("group");
    let wrapper = "sleep 30 & echo child $!; sleep 30 & echo child $!; wait";
    let project = scratch.project(
        "storefront",
        &format!("[local.wrapper]\ncmd = [\"sh\", \"-c\", \"{wrapper}\"]\n"),
    );
    let state = scratch.state();
    let daemon = Daemon::serving(&state);
    let mut link = daemon.link();
    let unit = unit_of(&project, "wrapper");

    // First generation: signal the leader alone, the way a wrapper-unaware stop would.
    link.start(&project, &unit).expect("start");
    assert!(
        until(PATIENCE, || children(&log_of(&state, &project, "wrapper"))
            .len()
            == 2),
        "the wrapper never reported its children"
    );
    let orphans = children(&log_of(&state, &project, "wrapper"));
    let leader = pid_of(&state, &project, "wrapper");
    assert_eq!(unsafe { libc::kill(leader as i32, libc::SIGTERM) }, 0);
    assert!(
        until(PATIENCE, || status_of(&mut link, &project, "wrapper").state
            != State::Up),
        "the leader survived its own SIGTERM"
    );
    assert!(
        orphans.iter().all(|pid| local::alive(*pid)),
        "the children were expected to be left orphaned: {orphans:?}"
    );
    // Cleaning up the orphans is what the group signal does; here it is done by hand.
    local::kill_group(leader).expect("the group is still ours to signal");
    assert!(until(PATIENCE, || local::group_empty(leader)));

    // Second generation: the same tree, stopped through the daemon.
    link.start(&project, &unit).expect("restart");
    assert!(
        until(PATIENCE, || children(&log_of(&state, &project, "wrapper"))
            .len()
            == 2),
        "the second generation never reported its children"
    );
    let tree = children(&log_of(&state, &project, "wrapper"));
    let leader = pid_of(&state, &project, "wrapper");
    assert_eq!(link.stop(&project, &unit), Ok(None));
    assert!(
        tree.iter().all(|pid| !local::alive(*pid)),
        "the group stop left children behind: {tree:?}"
    );
    assert!(local::group_empty(leader));
}

#[test]
fn a_unit_that_ignores_sigterm_is_killed_after_the_five_second_grace() {
    let scratch = Scratch::new("stubborn");
    let loop_ignoring_term = "trap '' TERM; echo ignoring; while :; do sleep 0.2; done";
    let project = scratch.project(
        "storefront",
        &format!("[local.stubborn]\ncmd = [\"sh\", \"-c\", \"{loop_ignoring_term}\"]\n"),
    );
    let state = scratch.state();
    let daemon = Daemon::serving(&state);
    let mut link = daemon.link();
    let unit = unit_of(&project, "stubborn");

    link.start(&project, &unit).expect("start");
    assert!(
        until(PATIENCE, || log_of(&state, &project, "stubborn")
            .contains("ignoring")),
        "the unit never installed its trap"
    );
    let leader = pid_of(&state, &project, "stubborn");

    let began = Instant::now();
    let note = link.stop(&project, &unit).expect("stop").expect("a note");
    let took = began.elapsed();

    assert!(took >= GRACE, "the grace was cut short at {took:?}");
    assert!(took < GRACE + Duration::from_secs(5), "{took:?}");
    assert!(note.contains("killed"), "{note}");
    assert!(local::group_empty(leader), "the group survived the SIGKILL");
    assert_eq!(
        status_of(&mut link, &project, "stubborn").state,
        State::Down
    );
}

#[test]
fn killing_the_daemon_takes_every_unit_with_it_and_leaves_no_pid_in_the_records() {
    let scratch = Scratch::new("kill-on-exit");
    let project = scratch.project(
        "storefront",
        "[local.sleeper]\ncmd = [\"sh\", \"-c\", \"echo up; sleep 30\"]\n",
    );
    let state = scratch.state();
    let mut daemon = Daemon::serving(&state);
    let mut link = daemon.link();

    link.start(&project, &unit_of(&project, "sleeper"))
        .expect("start");
    assert!(until(PATIENCE, || log_of(&state, &project, "sleeper")
        .contains("up")));
    let leader = pid_of(&state, &project, "sleeper");
    drop(link);

    daemon.signal(libc::SIGTERM);
    assert!(daemon.gone(), "the daemon ignored its own SIGTERM");
    assert!(
        local::group_empty(leader),
        "a unit outlived the daemon that was its parent"
    );
    assert_eq!(
        record(&state, &project, "sleeper").expect("record").pid,
        None
    );
}

#[test]
fn a_daemon_started_after_a_sigkilled_predecessor_kills_the_leftovers_and_puts_them_back() {
    let scratch = Scratch::new("leftovers");
    let project = scratch.project(
        "storefront",
        "[local.sleeper]\ncmd = [\"sh\", \"-c\", \"echo up; sleep 30\"]\n",
    );
    let state = scratch.state();
    let leader = {
        let mut daemon = Daemon::serving(&state);
        let mut link = daemon.link();
        link.start(&project, &unit_of(&project, "sleeper"))
            .expect("start");
        assert!(until(PATIENCE, || log_of(&state, &project, "sleeper")
            .contains("up")));
        let leader = pid_of(&state, &project, "sleeper");
        drop(link);

        // A daemon that cannot run its own cleanup is exactly the case leftovers exist for.
        daemon.child.kill().expect("SIGKILL the daemon");
        daemon.child.wait().expect("reaped");
        assert!(
            local::alive(leader),
            "macOS orphans children rather than killing them, so this unit should still be up"
        );
        leader
    };
    assert!(
        local::alive(leader),
        "the leftover went before the successor ran"
    );

    let successor = Daemon::serving(&state);
    let mut link = successor.link();
    assert!(
        until(PATIENCE, || local::group_empty(leader)),
        "the successor left its predecessor's unit running"
    );
    // Killed and then started again, never reclaimed: what is running is the successor's own child,
    // under a pid of its own, and the orphan it inherited is gone all the same.
    assert_eq!(status_of(&mut link, &project, "sleeper").state, State::Up);
    let second = pid_of(&state, &project, "sleeper");
    assert_ne!(
        second, leader,
        "the leftover was adopted rather than replaced"
    );
    assert_eq!(
        record(&state, &project, "sleeper").expect("record").restore,
        None,
        "a unit still marked would be started again by every daemon after this one"
    );
    assert_eq!(link.stop(&project, &unit_of(&project, "sleeper")), Ok(None));
}

#[test]
fn a_replaced_daemon_puts_back_what_it_was_running_and_leaves_what_was_stopped_alone() {
    let scratch = Scratch::new("restore");
    let project = scratch.project(
        "storefront",
        "[local.keeper]\ncmd = [\"sh\", \"-c\", \"echo up; sleep 30\"]\n\
         [local.by_hand]\ncmd = [\"sh\", \"-c\", \"echo up; sleep 30\"]\n\
         [local.crasher]\ncmd = [\"sh\", \"-c\", \"exit 7\"]\n",
    );
    let state = scratch.state();
    let first = {
        let mut daemon = Daemon::serving(&state);
        let mut link = daemon.link();
        for name in ["keeper", "by_hand", "crasher"] {
            link.start(&project, &unit_of(&project, name))
                .expect("start");
        }
        assert!(until(PATIENCE, || log_of(&state, &project, "keeper")
            .contains("up")));
        assert!(until(PATIENCE, || status_of(
            &mut link, &project, "crasher"
        )
        .state
            == State::Dead));
        // Stopped on purpose, which is the one thing a restore must not undo.
        assert_eq!(link.stop(&project, &unit_of(&project, "by_hand")), Ok(None));
        let first = pid_of(&state, &project, "keeper");
        drop(link);

        daemon.signal(libc::SIGTERM);
        assert!(daemon.gone(), "the daemon ignored its own SIGTERM");
        first
    };
    assert!(
        local::group_empty(first),
        "the stack outlived the daemon that owned it"
    );

    let successor = Daemon::serving(&state);
    let mut link = successor.link();
    assert_eq!(status_of(&mut link, &project, "keeper").state, State::Up);
    assert_ne!(
        pid_of(&state, &project, "keeper"),
        first,
        "the pid did not change, so nothing was actually respawned"
    );
    assert_eq!(status_of(&mut link, &project, "by_hand").state, State::Down);
    assert_eq!(status_of(&mut link, &project, "crasher").state, State::Dead);
    assert_eq!(
        status_of(&mut link, &project, "crasher").exit,
        Some(Exit::Code(7)),
        "a crash that was never restored still says how it ended"
    );
    assert_eq!(link.stop(&project, &unit_of(&project, "keeper")), Ok(None));
}

#[test]
fn two_projects_that_name_a_unit_alike_each_run_their_own_with_its_own_log() {
    let scratch = Scratch::new("alike");
    let manifest = "[local.rails]\ncmd = [\"sh\", \"-c\", \"echo up; sleep 30\"]\n";
    let one = scratch.project("storefront", manifest);
    let two = scratch.project("accounts", manifest);
    let state = scratch.state();
    let daemon = Daemon::serving(&state);
    let mut link = daemon.link();

    for project in [&one, &two] {
        assert_eq!(link.start(project, &unit_of(project, "rails")), Ok(None));
    }
    assert!(until(PATIENCE, || log_of(&state, &one, "rails")
        .contains("up")
        && log_of(&state, &two, "rails").contains("up")));
    let leaders = [pid_of(&state, &one, "rails"), pid_of(&state, &two, "rails")];
    assert_ne!(leaders[0], leaders[1], "one process answered for both");
    for project in [&one, &two] {
        assert_eq!(status_of(&mut link, project, "rails").state, State::Up);
    }

    // Stopping one leaves the other alone: the name they share is not a thing they share.
    assert_eq!(link.stop(&one, &unit_of(&one, "rails")), Ok(None));
    assert!(local::group_empty(leaders[0]));
    assert!(local::alive(leaders[1]));
    assert_eq!(status_of(&mut link, &two, "rails").state, State::Up);
    assert_eq!(status_of(&mut link, &one, "rails").state, State::Down);
}

#[test]
fn a_unit_this_project_already_runs_is_refused_by_the_pid_it_is_running_as() {
    let scratch = Scratch::new("twice");
    let project = scratch.project(
        "storefront",
        "[local.sleeper]\ncmd = [\"sh\", \"-c\", \"echo up; sleep 30\"]\n",
    );
    let state = scratch.state();
    let daemon = Daemon::serving(&state);
    let mut link = daemon.link();

    link.start(&project, &unit_of(&project, "sleeper"))
        .expect("start");
    assert!(until(PATIENCE, || log_of(&state, &project, "sleeper")
        .contains("up")));
    let leader = pid_of(&state, &project, "sleeper");

    let refusal = link
        .start(&project, &unit_of(&project, "sleeper"))
        .expect("the second start is answered, not failed")
        .expect("a refusal");
    assert_eq!(refusal, format!("sleeper is already running, pid {leader}"));

    assert_eq!(link.stop(&project, &unit_of(&project, "sleeper")), Ok(None));
    assert!(local::group_empty(leader));
}

#[test]
fn a_restart_spawns_into_the_same_log_truncated_with_one_generation_kept() {
    let scratch = Scratch::new("restart");
    let project = scratch.project(
        "storefront",
        "[local.chatty]\ncmd = [\"sh\", \"-c\", \"echo generation; sleep 30\"]\n",
    );
    let state = scratch.state();
    let daemon = Daemon::serving(&state);
    let mut link = daemon.link();
    let unit = unit_of(&project, "chatty");

    link.start(&project, &unit).expect("start");
    assert!(until(PATIENCE, || log_of(&state, &project, "chatty")
        .contains("generation")));
    let first = pid_of(&state, &project, "chatty");
    // A line of the caller's own, so the previous generation can be told apart from the fresh one.
    let log = Store::at(&state)
        .slot(&identity(&project))
        .log_path(&unit::key(unit::LOCAL, "chatty"));
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .expect("log"),
        "first generation"
    )
    .expect("append");

    assert_eq!(link.restart(&project, &unit), Ok(None));
    let second = pid_of(&state, &project, "chatty");
    assert_ne!(first, second);
    assert!(
        local::group_empty(first),
        "the old generation is still running"
    );

    assert!(until(PATIENCE, || log_of(&state, &project, "chatty")
        .contains("generation")));
    let fresh = log_of(&state, &project, "chatty");
    assert!(
        !fresh.contains("first generation"),
        "the log was not truncated: {fresh:?}"
    );
    let previous = std::fs::read_to_string(log.with_file_name("local-chatty.log.1"))
        .expect("one previous generation");
    assert!(previous.contains("first generation"), "{previous:?}");
    assert_eq!(
        std::fs::read_dir(log.parent().expect("logs"))
            .expect("logs")
            .count(),
        2,
        "more than one previous generation was kept"
    );

    assert_eq!(link.stop(&project, &unit), Ok(None));
    assert!(local::group_empty(second));
}

#[test]
fn a_daemon_stays_for_as_long_as_it_owns_a_unit_and_the_last_stop_is_its_shutdown() {
    let scratch = Scratch::new("ownership");
    let project = scratch.project(
        "storefront",
        "[local.sleeper]\ncmd = [\"sh\", \"-c\", \"echo up; sleep 30\"]\n",
    );
    let state = scratch.state();
    // Served in this process so the idle window can be short enough to watch.
    let idle = Duration::from_millis(150);
    let serving = {
        let state = state.clone();
        std::thread::spawn(move || herdr_stack::daemon::serve_with_idle(&state, idle))
    };

    let mut link = Endpoint::at(&state)
        .connect_within(PATIENCE)
        .expect("a link to the daemon");
    link.start(&project, &unit_of(&project, "sleeper"))
        .expect("start");
    assert!(until(PATIENCE, || log_of(&state, &project, "sleeper")
        .contains("up")));
    let leader = pid_of(&state, &project, "sleeper");
    drop(link);

    std::thread::sleep(idle * 6);
    assert!(
        !serving.is_finished(),
        "the daemon exited while it still owned a unit"
    );

    let mut link = Endpoint::at(&state)
        .connect_within(PATIENCE)
        .expect("a second link");
    assert_eq!(link.stop(&project, &unit_of(&project, "sleeper")), Ok(None));
    drop(link);

    assert!(
        until(PATIENCE, || serving.is_finished()),
        "the last unit stopping was not the daemon's shutdown"
    );
    serving.join().expect("serve returns").expect("served");
    assert!(local::group_empty(leader));
}

#[test]
fn a_project_whose_directory_is_gone_is_dropped_by_the_next_daemon() {
    let scratch = Scratch::new("vanished");
    let project = scratch.project(
        "storefront",
        "[local.brief]\ncmd = [\"sh\", \"-c\", \"echo done\"]\n",
    );
    let state = scratch.state();
    let slot = Store::at(&state).slot(&identity(&project));
    {
        let mut daemon = Daemon::serving(&state);
        let mut link = daemon.link();
        link.start(&project, &unit_of(&project, "brief"))
            .expect("start");
        assert!(until(PATIENCE, || log_of(&state, &project, "brief")
            .contains("done")));
        drop(link);
        daemon.signal(libc::SIGTERM);
        assert!(daemon.gone());
    }
    assert!(slot.dir().exists(), "the project was never recorded");

    std::fs::remove_dir_all(&project.root).expect("remove the checkout");
    let successor = Daemon::serving(&state);
    successor.link();
    assert!(
        !slot.dir().exists(),
        "a project whose path is gone was kept: {}",
        slot.dir().display()
    );
}
