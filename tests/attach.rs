//! The attach pane as a real process on a real terminal, typing at a real unit.
//!
//! The pane under test is this binary under its `attach` argv, given a `HOME` of this file's own
//! making — so the daemon it finds is the one started here, at a state root under that home, and never
//! the spelled-out one.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Instant;

use herdr_stack::attach::{PROJECT_ENV, UNIT_ENV};
use herdr_stack::client::{Endpoint, Link, Target};
use herdr_stack::manifest::Project;
use herdr_stack::unit::{self, State, Status};

mod support;

use support::{PATIENCE, Pty, STEP, staging};

/// Where the pane's own `Endpoint::spelled_out()` lands, given the home it is handed.
fn state_root(home: &Path) -> PathBuf {
    home.join(".local/state/herdr/plugins/herdr-stack")
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
        std::thread::sleep(STEP);
    }
}

/// A home, a state root under it, and a project with one `tty` unit in it.
struct Stage {
    home: PathBuf,
    project: Project,
    daemon: Option<Child>,
}

impl Stage {
    /// `script` is the shell the unit runs, written to a file so the manifest stays plain argv.
    fn new(name: &str, script: &str) -> Stage {
        let home = staging(&format!("herdr-stack-attach-{name}"));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(state_root(&home)).expect("state root");

        let root = home.join("storefront");
        std::fs::create_dir_all(&root).expect("project dir");
        let path = root.join("probe.sh");
        std::fs::write(&path, script).expect("script");
        let manifest = root.join(".herdr-stack.toml");
        std::fs::write(
            &manifest,
            format!(
                "[local.probe]\ncmd = [\"sh\", \"{}\"]\ntty = true\n\
                 \n[local.piped]\ncmd = [\"sleep\", \"30\"]\n",
                path.display()
            ),
        )
        .expect("manifest");

        let daemon = Endpoint::at(state_root(&home))
            .command(Path::new(env!("CARGO_BIN_EXE_herdr-stack")))
            .spawn()
            .expect("daemon spawns");
        let stage = Stage {
            home,
            project: Project::load(&manifest).expect("manifest parses"),
            daemon: Some(daemon),
        };
        stage.link();
        stage
    }

    fn link(&self) -> Link {
        Endpoint::at(state_root(&self.home))
            .connect_within(PATIENCE)
            .expect("a link to the daemon")
    }

    fn start(&self, name: &str) {
        let unit = Target::of(&self.project, unit::LOCAL, name).expect("a declared unit");
        self.link().start(&self.project, &unit).expect("start");
    }

    fn status(&self, name: &str) -> Status {
        self.link()
            .status(&self.project)
            .expect("status")
            .remove(&unit::key(unit::LOCAL, name))
            .unwrap_or_else(|| Status::of(State::Down))
    }

    /// A pane of `columns` wide, attached to the unit named.
    fn attach(&self, name: &str, columns: u16) -> Pty {
        Pty::running(
            &["attach"],
            &[
                (PROJECT_ENV, &self.project.root.to_string_lossy()),
                (UNIT_ENV, name),
            ],
            &self.home,
            &self.project.root,
            columns,
        )
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
            let _ = until(|| matches!(daemon.try_wait(), Ok(Some(_))));
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// A prompt, an echo of whatever is typed at it, and something to say when interrupted: everything an
/// interactive program raised mid-request does, without needing one. An interrupt fails the `read` it
/// lands in as end of input does, so the trap marks it and the loop carries on as a REPL would.
const PROMPT: &str = "trap 'interrupted=1; printf \"interrupted\\n\"' INT\n\
                      printf 'ready> '\n\
                      while :; do\n\
                        interrupted=\n\
                        if ! IFS= read -r line; then\n\
                          [ -n \"$interrupted\" ] && continue\n\
                          break\n\
                        fi\n\
                        printf '\\nyou said %s\\nready> ' \"$line\"\n\
                      done\n";

/// Being attached, proved by typing rather than by waiting for a banner: the prompt is printed once at
/// startup and says nothing to a pane that arrived after it, while whatever is typed sits in the
/// terminal's input queue until the unit gets round to reading it.
fn greeted(pane: &mut Pty, word: &str) {
    pane.press(&format!("{word}\n"));
    pane.wait_for(&format!("you said {word}"));
}

#[test]
fn keystrokes_reach_the_process_and_ctrl_c_interrupts_the_unit_rather_than_the_pane() {
    let stage = Stage::new("typing", PROMPT);
    stage.start("probe");

    let mut pane = stage.attach("probe", 80);
    // Echoed by the unit's own line discipline, then answered by the unit itself.
    greeted(&mut pane, "1 + 1");

    pane.press("\u{3}");
    pane.wait_for("interrupted");
    // The pane took the interrupt to the unit rather than dying of it.
    assert!(pane.alive());
    assert_eq!(stage.status("probe").state, State::Up);

    pane.press("still here\n");
    pane.wait_for("you said still here");
}

#[test]
fn detaching_leaves_the_unit_running_and_attaching_again_reaches_it() {
    let stage = Stage::new("detach", PROMPT);
    stage.start("probe");

    let mut pane = stage.attach("probe", 80);
    greeted(&mut pane, "first");

    pane.press("\u{1d}");
    pane.wait_for("detached from the unit");
    assert!(until(|| !pane.alive()), "the pane never left");
    assert_eq!(stage.status("probe").state, State::Up);

    let mut again = stage.attach("probe", 80);
    greeted(&mut again, "second");
    assert_eq!(stage.status("probe").state, State::Up);
}

#[test]
fn closing_the_pane_leaves_the_unit_running_and_the_unit_exiting_says_so_in_the_pane() {
    let stage = Stage::new("closing", PROMPT);
    stage.start("probe");

    let mut pane = stage.attach("probe", 80);
    greeted(&mut pane, "hello");
    pane.close();
    assert_eq!(stage.status("probe").state, State::Up);

    // Attached again, and this time the unit goes first. A unit that has already said its piece says
    // nothing to a pane that has just arrived, so the prompt has to be asked for.
    let mut watching = stage.attach("probe", 80);
    greeted(&mut watching, "still-there");
    let unit = Target::of(&stage.project, unit::LOCAL, "probe").expect("a declared unit");
    stage.link().stop(&stage.project, &unit).expect("stop");
    watching.wait_for("probe has exited");
    assert!(until(|| !watching.alive()), "the pane outlived the unit");
}

#[test]
fn two_panes_at_once_both_reach_the_process_and_both_see_the_output() {
    let stage = Stage::new("two", PROMPT);
    stage.start("probe");

    let mut one = stage.attach("probe", 80);
    let mut two = stage.attach("probe", 80);

    one.press("from one\n");
    for pane in [&mut one, &mut two] {
        pane.wait_for("you said from one");
    }
    two.press("from two\n");
    for pane in [&mut one, &mut two] {
        pane.wait_for("you said from two");
    }
}

#[test]
fn the_panes_size_is_the_units_size_and_a_unit_nobody_attached_to_still_has_one() {
    let stage = Stage::new("size", "while :; do stty size; sleep 0.3; done\n");
    stage.start("probe");

    // Nobody has attached, so the unit is the size a terminal is opened at rather than nothing.
    assert!(
        until(|| log_of(&stage, "probe").contains("24 80")),
        "the unit never reported a size: {:?}",
        log_of(&stage, "probe")
    );

    let mut pane = stage.attach("probe", 100);
    pane.wait_for("24 100");

    // Made wider, and the program draws for the new size rather than the one it started at.
    pane.resize(40, 132);
    pane.wait_for("40 132");

    pane.press("\u{1d}");
    assert!(until(|| !pane.alive()), "the pane never left");

    // Attached again from a pane of another size, and the unit is that size now.
    let mut narrower = stage.attach("probe", 90);
    narrower.wait_for("24 90");
}

/// What the log cannot carry and the pane must: the program moves the cursor back over lines it has
/// already written, which is what a prompt redrawing itself does.
#[test]
fn cursor_movement_renders_in_the_pane_while_the_log_keeps_its_readable_copy() {
    // An attached pane sees output from the moment it attaches and none from before, so the unit draws
    // nothing until the pane has sent it a key, and says in its log when it is ready for one.
    let stage = Stage::new(
        "redraw",
        "stty -echo\n         echo waiting\n         IFS= read -r go\n         printf 'one\\ntwo\\nthree\\n'\n         sleep 1\n         printf '\\033[2A\\033[K\\033[32mredrawn\\033[0m\\n'\n         while :; do sleep 0.2; done\n",
    );
    stage.start("probe");
    assert!(until(|| log_of(&stage, "probe").contains("waiting")));
    let mut pane = stage.attach("probe", 80);
    pane.wait_for("attached");
    pane.press("\n");

    let screen = pane.wait_for("redrawn");
    let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
    // The three lines, with the middle one written over in place.
    let at = lines
        .iter()
        .position(|line| *line == "one")
        .expect("the first line");
    assert_eq!(&lines[at..at + 3], ["one", "redrawn", "three"], "{lines:?}");

    // The log has the same output as the plain text it always was: every line, no colour, in order.
    assert!(
        until(|| log_of(&stage, "probe").contains("redrawn")),
        "{:?}",
        log_of(&stage, "probe")
    );
    let log = log_of(&stage, "probe");
    assert!(
        !log.contains('\u{1b}'),
        "an escape sequence reached the log: {log:?}"
    );
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        ["waiting", "one", "two", "three", "redrawn"]
    );
}

#[test]
fn stopping_and_restarting_still_work_while_a_pane_is_attached() {
    let stage = Stage::new("verbs", PROMPT);
    stage.start("probe");
    let mut pane = stage.attach("probe", 80);
    greeted(&mut pane, "before");

    let unit = Target::of(&stage.project, unit::LOCAL, "probe").expect("a declared unit");
    stage
        .link()
        .restart(&stage.project, &unit)
        .expect("restart");
    assert_eq!(stage.status("probe").state, State::Up);
    // The terminal the pane was holding went with the generation that owned it.
    pane.wait_for("probe has exited");

    // And the fresh generation takes a pane of its own.
    let mut again = stage.attach("probe", 80);
    greeted(&mut again, "after");
    stage.link().stop(&stage.project, &unit).expect("stop");
    assert_eq!(stage.status("probe").state, State::Down);
}

#[test]
fn a_unit_that_never_asked_for_a_terminal_refuses_the_pane_by_name() {
    let stage = Stage::new("piped", PROMPT);
    stage.start("piped");

    let mut pane = stage.attach("piped", 80);
    pane.wait_for("runs on a pipe");
}

#[test]
fn typing_at_a_unit_that_is_not_running_says_so_rather_than_opening_onto_nothing() {
    let stage = Stage::new("stopped", PROMPT);
    let mut pane = stage.attach("probe", 80);
    pane.wait_for("probe is not running");
}

fn log_of(stage: &Stage, name: &str) -> String {
    let path = herdr_stack::store::Store::at(state_root(&stage.home))
        .slot(&herdr_stack::store::Identity {
            path: stage.project.root.clone(),
            name: stage.project.name.clone(),
        })
        .log_path(&unit::key(unit::LOCAL, name));
    std::fs::read_to_string(path).unwrap_or_default()
}
