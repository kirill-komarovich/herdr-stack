//! Units started in real fish, zsh and bash, each with real `mise activate` and `direnv hook` lines in
//! an rc this file writes, so the env a unit gets is whatever those hooks export in its directory.
//!
//! Only inside the container `tests/container/Dockerfile` builds, where all three shells and both tools
//! are installed, and only under a `HOME` of each test's own, so the trust mise and direnv are given
//! here is never the developer's.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use herdr_stack::client::{Endpoint, Link, Target};
use herdr_stack::manifest::Project;
use herdr_stack::store::{Identity, Store};
use herdr_stack::unit;

const PATIENCE: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(50);

const PROBE: &str = "echo mise=$FROM_MISE direnv=$FROM_DIRENV; sleep 30";

struct Scratch {
    root: PathBuf,
    daemon: Option<Child>,
}

impl Scratch {
    fn new(shell: &str) -> Scratch {
        assert!(
            std::env::var_os("HERDR_STACK_IN_CONTAINER").is_some()
                && Path::new("/.dockerenv").exists(),
            "refusing to trust and allow directories outside the test container"
        );
        let root = PathBuf::from("/tmp/shells").join(shell);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home/.config/herdr")).expect("home");
        std::fs::create_dir_all(root.join("home/.config/fish")).expect("fish config dir");
        Scratch { root, daemon: None }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    /// The shell Herdr's panes would run, and that shell's rc hooking in both tools.
    fn shell(&self, program: &str, rc: &str, hooks: &str) {
        std::fs::write(
            self.home().join(".config/herdr/config.toml"),
            format!("[terminal]\ndefault_shell = \"{program}\"\nshell_mode = \"non_login\"\n"),
        )
        .expect("herdr config");
        std::fs::write(self.home().join(rc), hooks).expect("rc");
    }

    fn project(&self, manifest: &str) -> Project {
        let dir = self.root.join("proj");
        std::fs::create_dir_all(&dir).expect("project dir");
        std::fs::write(dir.join("mise.toml"), "[env]\nFROM_MISE = \"mise-ok\"\n")
            .expect("mise.toml");
        std::fs::write(dir.join(".envrc"), "export FROM_DIRENV=direnv-ok\n").expect(".envrc");
        for (tool, verb) in [("mise", "trust"), ("direnv", "allow")] {
            let done = Command::new(tool)
                .arg(verb)
                .arg(&dir)
                .env("HOME", self.home())
                .output()
                .expect("the tool runs");
            assert!(done.status.success(), "{tool} {verb}: {done:?}");
        }
        let path = dir.join(".herdr-stack.toml");
        std::fs::write(&path, manifest).expect("manifest");
        Project::load(&path).expect("manifest parses")
    }

    fn serve(&mut self) -> Link {
        let child = Endpoint::at(self.state())
            .command(Path::new(env!("CARGO_BIN_EXE_herdr-stack")))
            .env("HOME", self.home())
            .spawn()
            .expect("daemon spawns");
        self.daemon = Some(child);
        Endpoint::at(self.state())
            .connect_within(PATIENCE)
            .expect("a link to the daemon")
    }

    fn log(&self, project: &Project, name: &str) -> String {
        let identity = Identity {
            path: project.root.clone(),
            name: project.name.clone(),
        };
        std::fs::read_to_string(
            Store::at(self.state())
                .slot(&identity)
                .log_path(&unit::key(unit::LOCAL, name)),
        )
        .unwrap_or_default()
    }

    fn wait_for_log(&self, project: &Project, name: &str, text: &str) -> String {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let log = self.log(project, name);
            if log.contains(text) {
                return log;
            }
            assert!(
                Instant::now() < deadline,
                "`{text}` never reached {name}'s log: {log:?}"
            );
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // SIGTERM rather than kill, so the daemon takes its units down with it on the way out.
        if let Some(mut daemon) = self.daemon.take() {
            let _ = unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
            let _ = daemon.wait();
        }
    }
}

fn unit_of<'a>(project: &'a Project, name: &str) -> Target<'a> {
    Target::of(project, unit::LOCAL, name).expect("the manifest declares the unit")
}

/// A unit on pipes and one on a terminal, both started in `program` with both tools hooked in.
fn both_tools_reach_units_started_in(program: &str, rc: &str, hooks: &str) {
    let mut scratch = Scratch::new(Path::new(program).file_name().unwrap().to_str().unwrap());
    scratch.shell(program, rc, hooks);
    let project = scratch.project(&format!(
        "[local.piped]\ncmd = [\"sh\", \"-c\", \"{PROBE}\"]\n\
         \n[local.terminal]\ncmd = [\"sh\", \"-c\", \"{PROBE}\"]\ntty = true\n"
    ));
    let mut link = scratch.serve();

    for name in ["piped", "terminal"] {
        link.start(&project, &unit_of(&project, name))
            .expect("start");
    }
    for name in ["piped", "terminal"] {
        let log = scratch.wait_for_log(&project, name, "mise=mise-ok direnv=direnv-ok");
        // What the hooks said about the env, then the unit: nothing of the shell's own start-up.
        let foreign: Vec<&str> = log
            .lines()
            .filter(|line| !line.contains("direnv: ") && !line.starts_with("mise="))
            .collect();
        assert!(foreign.is_empty(), "{name}: {log:?}");
    }
}

#[test]
fn fish_runs_its_prompt_hooks_before_becoming_the_unit() {
    both_tools_reach_units_started_in(
        "/usr/bin/fish",
        ".config/fish/config.fish",
        "mise activate fish | source\ndirenv hook fish | source\n",
    );
}

#[test]
fn zsh_runs_its_prompt_hooks_before_becoming_the_unit() {
    both_tools_reach_units_started_in(
        "/usr/bin/zsh",
        ".zshrc",
        "eval \"$(mise activate zsh)\"\neval \"$(direnv hook zsh)\"\n",
    );
}

#[test]
fn bash_runs_its_prompt_hooks_before_becoming_the_unit() {
    both_tools_reach_units_started_in(
        "/bin/bash",
        ".bashrc",
        "eval \"$(mise activate bash)\"\neval \"$(direnv hook bash)\"\n",
    );
}
