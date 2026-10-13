//! A unit that asked for a terminal, as a real process, driven over the daemon's socket.
//!
//! Everything here is a shell of this file's own making, in a throwaway project under a state root of
//! its own — never the spelled-out one, whose records name processes this test did not spawn.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use herdr_stack::client::{Endpoint, Link, Target};
use herdr_stack::local;
use herdr_stack::manifest::Project;
use herdr_stack::store::{Identity, Record, Store};
use herdr_stack::unit::{self, State, Status};

const PATIENCE: Duration = Duration::from_secs(15);
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

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = std::env::temp_dir().join(format!("herdr-stack-tty-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch");
        Scratch { root }
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Daemon {
    child: Child,
    root: PathBuf,
}

impl Daemon {
    fn serving(root: &Path) -> Daemon {
        let child = Endpoint::at(root)
            .command(exe())
            // A unit runs in the user's shell, config and all; this one has none of either.
            .env("HOME", root)
            .env("SHELL", "/bin/sh")
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

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.alive() {
            let _ = unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
            if !until(PATIENCE, || matches!(self.child.try_wait(), Ok(Some(_)))) {
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

fn log_of(state: &Path, project: &Project, name: &str) -> String {
    let path = Store::at(state)
        .slot(&identity(project))
        .log_path(&unit::key(unit::LOCAL, name));
    std::fs::read_to_string(path).unwrap_or_default()
}

fn status_of(link: &mut Link, project: &Project, name: &str) -> Status {
    link.status(project)
        .expect("status")
        .remove(&unit::key(unit::LOCAL, name))
        .unwrap_or_else(|| Status::of(State::Down))
}

/// What `ps` says a process's controlling terminal is: a name for one that has it, `??` for one that
/// does not.
/// `ps` names a process's terminal (`ttys003` on macOS, `pts/2` on Linux) and says `??` or `?` of a
/// process that has none.
fn controlling_terminal(pid: u32) -> Option<String> {
    let output = Command::new("/bin/ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("tty=")
        .output()
        .expect("ps runs");
    let tty = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!tty.trim_matches('?').is_empty()).then_some(tty)
}

#[test]
fn a_tty_unit_runs_on_a_terminal_of_its_own_and_a_plain_one_still_does_not() {
    let scratch = Scratch::new("terminal");
    let probe = "if [ -t 0 ] && [ -t 1 ]; then echo on-a-terminal; else echo on-a-pipe; fi; \
                 echo term=$TERM; sleep 30";
    let project = scratch.project(
        "storefront",
        &format!(
            "[local.interactive]\ncmd = [\"sh\", \"-c\", \"{probe}\"]\ntty = true\n\
             \n[local.plain]\ncmd = [\"sh\", \"-c\", \"{probe}\"]\n"
        ),
    );
    let state = scratch.state();
    let daemon = Daemon::serving(&state);
    let mut link = daemon.link();

    for name in ["interactive", "plain"] {
        link.start(&project, &unit_of(&project, name))
            .expect("start");
    }
    assert!(
        until(PATIENCE, || ["interactive", "plain"]
            .iter()
            .all(|name| log_of(&state, &project, name).contains("term="))),
        "one of the units never reported: {:?}",
        log_of(&state, &project, "interactive")
    );

    let interactive = log_of(&state, &project, "interactive");
    assert!(interactive.contains("on-a-terminal"), "{interactive:?}");
    assert!(
        interactive.contains("term=xterm-256color"),
        "{interactive:?}"
    );

    let plain = log_of(&state, &project, "plain");
    assert!(plain.contains("on-a-pipe"), "{plain:?}");

    let terminal = controlling_terminal(pid_of(&state, &project, "interactive"));
    assert!(terminal.is_some(), "{terminal:?}");
    assert_eq!(
        controlling_terminal(pid_of(&state, &project, "plain")),
        None
    );

    for name in ["interactive", "plain"] {
        assert_eq!(link.stop(&project, &unit_of(&project, name)), Ok(None));
    }
}

#[test]
fn a_tty_units_log_reads_as_plain_text_whatever_the_program_drew() {
    let scratch = Scratch::new("readable");
    // Colour, a bar rewritten in place, and a tab: everything the three readers of a log cannot
    // render between them.
    let script = scratch.root.join("drawing.sh");
    std::fs::write(
        &script,
        "printf '\\033[32mgreen\\033[0m done\\n'\n\
         printf '[   ] 0%%\\r[## ] 50%%\\r[###] 100%%\\n'\n\
         printf 'a\\tb\\n'\n\
         printf 'last line with no newline'\n\
         sleep 30\n",
    )
    .expect("script");
    let project = scratch.project(
        "storefront",
        &format!(
            "[local.drawing]\ncmd = [\"sh\", \"{}\"]\ntty = true\n",
            script.display()
        ),
    );
    let state = scratch.state();
    let daemon = Daemon::serving(&state);
    let mut link = daemon.link();
    let unit = unit_of(&project, "drawing");

    link.start(&project, &unit).expect("start");
    assert!(
        until(PATIENCE, || log_of(&state, &project, "drawing")
            .contains("a    b")),
        "the unit never drew: {:?}",
        log_of(&state, &project, "drawing")
    );

    let log = log_of(&state, &project, "drawing");
    let lines: Vec<&str> = log.lines().collect();
    assert!(
        !log.contains('\u{1b}'),
        "an escape sequence reached the log: {log:?}"
    );
    assert_eq!(lines[0], "green done");
    assert_eq!(
        lines[1], "[###] 100%",
        "every frame of the bar reached the log"
    );
    assert_eq!(lines[2], "a    b");

    // The half-written line is the unit's last word, and it reaches the log when the unit ends.
    assert_eq!(link.stop(&project, &unit), Ok(None));
    assert!(
        until(PATIENCE, || log_of(&state, &project, "drawing")
            .contains("last line with no newline")),
        "{:?}",
        log_of(&state, &project, "drawing")
    );
}

#[test]
fn a_tty_unit_leads_its_own_session_and_a_stop_still_takes_its_whole_group() {
    let scratch = Scratch::new("group");
    let wrapper = "sleep 30 & echo child $!; sleep 30 & echo child $!; wait";
    let project = scratch.project(
        "storefront",
        &format!("[local.wrapper]\ncmd = [\"sh\", \"-c\", \"{wrapper}\"]\ntty = true\n"),
    );
    let state = scratch.state();
    let daemon = Daemon::serving(&state);
    let mut link = daemon.link();
    let unit = unit_of(&project, "wrapper");

    link.start(&project, &unit).expect("start");
    assert!(
        until(PATIENCE, || children(&log_of(&state, &project, "wrapper"))
            .len()
            == 2),
        "the wrapper never reported its children: {:?}",
        log_of(&state, &project, "wrapper")
    );
    let tree = children(&log_of(&state, &project, "wrapper"));
    let leader = pid_of(&state, &project, "wrapper");
    // A session of its own is what lets the unit outlive the popup it was started from.
    assert_eq!(unsafe { libc::getsid(leader as i32) }, leader as i32);

    assert_eq!(link.stop(&project, &unit), Ok(None));
    assert!(
        tree.iter().all(|pid| !local::alive(*pid)),
        "the group stop left children behind: {tree:?}"
    );
    assert!(local::group_empty(leader));
    assert_eq!(status_of(&mut link, &project, "wrapper").state, State::Down);

    // And a restart gives it a fresh terminal rather than the one that just went.
    link.restart(&project, &unit).expect("restart");
    assert!(
        until(PATIENCE, || children(&log_of(&state, &project, "wrapper"))
            .len()
            == 2),
        "the second generation never reported its children"
    );
    let second = pid_of(&state, &project, "wrapper");
    assert_ne!(second, leader);
    assert!(controlling_terminal(second).is_some());
    assert_eq!(link.stop(&project, &unit), Ok(None));
}

/// The pids a wrapper unit reported forking, read out of its own log.
fn children(log: &str) -> Vec<u32> {
    log.lines()
        .filter_map(|line| line.strip_prefix("child "))
        .filter_map(|pid| pid.trim().parse().ok())
        .collect()
}
