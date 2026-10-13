//! Against a real Herdr rather than a stand-in for its socket: the manifest it links, the plugin panes
//! it launches, and the popup driving it over `session.snapshot`, `plugin.pane.open`, `tab.rename`,
//! `pane.split` and `pane.send_input`.
//!
//! Each test runs a `herdr server` of its own under a `HOME` of its own, and only inside the container
//! `tests/herdr/Dockerfile` builds: a `herdr` command run on a desktop picks up the calling pane's
//! `HERDR_*` env, and a stray `server stop` then stops the user's real server.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const PATIENCE: Duration = Duration::from_secs(15);
const STEP: Duration = Duration::from_millis(100);

fn plugin_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The binary the manifest's panes run, so the popup under test is the one Herdr would launch.
fn stack_exe() -> PathBuf {
    let exe = plugin_root().join("target/release/herdr-stack");
    assert!(
        exe.exists(),
        "{} is missing: build --release first",
        exe.display()
    );
    exe
}

struct Herdr {
    home: PathBuf,
    server: Child,
}

impl Herdr {
    fn start(name: &str) -> Herdr {
        assert!(
            std::env::var_os("HERDR_STACK_IN_CONTAINER").is_some()
                && Path::new("/.dockerenv").exists(),
            "refusing to start a herdr server outside the test container"
        );
        let home = PathBuf::from("/tmp/herdr-tests").join(name);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".local/bin")).expect("home");
        let mise = home.join(".local/bin/mise");
        std::fs::write(&mise, "#!/bin/sh\nshift 2\nexec \"$@\"\n").expect("mise");
        std::fs::set_permissions(&mise, std::fs::Permissions::from_mode(0o755)).expect("mise");

        let server = isolated(Command::new("herdr"), &home)
            .arg("server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("herdr server");
        let herdr = Herdr { home, server };
        let socket = herdr.home.join(".config/herdr/herdr.sock");
        settle("herdr's socket", || socket.exists().then_some(()));
        herdr.cli(&["plugin", "link", &plugin_root().to_string_lossy()]);
        herdr
    }

    fn cli(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(output.status.success(), "herdr {args:?}: {}", said(&output));
        serde_json::from_slice::<Value>(&output.stdout)
            .map(|reply| reply["result"].clone())
            .unwrap_or(Value::Null)
    }

    fn run(&self, args: &[&str]) -> Output {
        isolated(Command::new("herdr"), &self.home)
            .args(args)
            .output()
            .expect("herdr")
    }

    fn stack(&self, args: &[&str]) -> Output {
        isolated(Command::new(stack_exe()), &self.home)
            .args(args)
            .output()
            .expect("herdr-stack")
    }

    fn project(&self, name: &str, manifest: Option<&str>) -> PathBuf {
        let dir = self.home.join(name);
        std::fs::create_dir_all(&dir).expect("project");
        if let Some(manifest) = manifest {
            std::fs::write(dir.join(".herdr-stack.toml"), manifest).expect("manifest");
        }
        dir
    }

    /// A workspace whose one pane sits in `cwd`, which is what the popup reads its project from.
    fn workspace(&self, cwd: &Path) -> String {
        let created = self.cli(&[
            "workspace",
            "create",
            "--cwd",
            &cwd.to_string_lossy(),
            "--focus",
        ]);
        created["root_pane"]["pane_id"]
            .as_str()
            .expect("a root pane")
            .to_string()
    }

    fn popup(&self, pane: &str) {
        self.cli(&["pane", "run", pane, &stack_exe().to_string_lossy()]);
    }

    /// Matched across line ends, because a narrow pane wraps a long line wherever it runs out.
    fn wait_for(&self, pane: &str, text: &str) {
        self.wait_screen(pane, text, |screen| screen.replace('\n', "").contains(text));
    }

    /// The popup draws a row before the daemon has said how the unit is, and `O` on a unit that is not
    /// up yet opens its log rather than a pane to type at.
    fn wait_until_up(&self, pane: &str, unit: &str) {
        self.wait_screen(pane, &format!("{unit} up"), |screen| {
            screen.lines().any(|line| {
                let words: Vec<&str> = line.split_whitespace().collect();
                words.windows(3).any(|row| row == [unit, "local", "up"])
            })
        });
    }

    /// Polled through `pane read` rather than `pane wait-output`, whose arguments 0.8.0 misparses.
    fn wait_screen(&self, pane: &str, what: &str, shown: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let screen = self.run(&["pane", "read", pane, "--source", "recent"]);
            let screen = String::from_utf8_lossy(&screen.stdout);
            if shown(&screen) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "`{what}` never showed in {pane}\n--- {pane} ---\n{screen}"
            );
            std::thread::sleep(STEP);
        }
    }

    fn panes(&self) -> Vec<Value> {
        self.cli(&["pane", "list"])["panes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The pane of the tab labelled `label`, once there is one.
    fn pane_in_tab(&self, label: &str) -> String {
        settle(&format!("a tab labelled {label}"), || {
            let tabs = self.cli(&["tab", "list"]);
            let tab = tabs["tabs"]
                .as_array()?
                .iter()
                .find(|tab| tab["label"] == label)?["tab_id"]
                .clone();
            self.panes()
                .into_iter()
                .find(|pane| pane["tab_id"] == tab)
                .and_then(|pane| pane["pane_id"].as_str().map(str::to_string))
        })
    }
}

impl Drop for Herdr {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

/// Nothing of the caller's environment reaches Herdr but a `PATH` to find a shell on.
fn isolated(mut command: Command, home: &Path) -> Command {
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .stdin(Stdio::null());
    command
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn settle<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        std::thread::sleep(STEP);
    }
}

#[test]
fn herdr_links_the_manifest_without_a_warning() {
    let herdr = Herdr::start("link");
    let listed = String::from_utf8_lossy(&herdr.run(&["plugin", "list"]).stdout).to_string();
    assert!(
        listed.contains("herdr-stack (Dev stack) enabled"),
        "{listed}"
    );
    assert!(!listed.contains("warning"), "{listed}");
}

#[test]
fn a_log_pane_herdr_opens_follows_the_log_it_is_handed() {
    let herdr = Herdr::start("tail");
    let log = herdr.home.join("unit.log");
    std::fs::write(&log, "first line\n").expect("log");
    herdr.workspace(&herdr.home);

    let opened = herdr.cli(&[
        "plugin",
        "pane",
        "open",
        "--plugin",
        "herdr-stack",
        "--entrypoint",
        "tail",
        "--env",
        &format!("HERDR_STACK_LOG={}", log.display()),
    ]);
    let pane = opened["plugin_pane"]["pane"]["pane_id"]
        .as_str()
        .expect("a pane")
        .to_string();
    herdr.wait_for(&pane, "first line");

    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .and_then(|mut log| log.write_all(b"second line\n"))
        .expect("append");
    herdr.wait_for(&pane, "second line");
}

#[test]
fn the_popup_opens_a_running_units_log_in_a_tab_named_for_it() {
    let herdr = Herdr::start("log-tab");
    let project = herdr.project(
        "proj",
        Some("[local.ticker]\ncmd = [\"sh\", \"-c\", \"echo ticker-up; exec sleep 600\"]\n"),
    );
    let started = herdr.stack(&["start", "--project", &project.to_string_lossy(), "ticker"]);
    assert!(started.status.success(), "{}", said(&started));

    let root = herdr.workspace(&project);
    herdr.popup(&root);
    herdr.wait_until_up(&root, "ticker");
    herdr.cli(&["pane", "send-keys", &root, "O"]);

    let pane = herdr.pane_in_tab("ticker");
    herdr.wait_for(&pane, "ticker-up");
}

#[test]
fn the_popup_attaches_a_pane_that_types_at_a_tty_unit() {
    let herdr = Herdr::start("attach");
    let project = herdr.project(
        "proj",
        Some(
            "[local.repl]\ncmd = [\"sh\", \"-c\", \"while read line; do echo \\\"got $line\\\"; done\"]\ntty = true\n",
        ),
    );
    let started = herdr.stack(&["start", "--project", &project.to_string_lossy(), "repl"]);
    assert!(started.status.success(), "{}", said(&started));

    let root = herdr.workspace(&project);
    herdr.popup(&root);
    herdr.wait_until_up(&root, "repl");
    herdr.cli(&["pane", "send-keys", &root, "O"]);

    let pane = herdr.pane_in_tab("repl");
    herdr.wait_for(&pane, "attached");
    herdr.cli(&["pane", "send-text", &pane, "ping\n"]);
    herdr.wait_for(&pane, "got ping");
}

#[test]
fn an_empty_project_is_handed_to_the_configured_agent_in_a_split() {
    let herdr = Herdr::start("agent");
    let config = herdr
        .home
        .join(".config/herdr/plugins/config/herdr-stack/config.toml");
    std::fs::create_dir_all(config.parent().expect("dir")).expect("config dir");
    std::fs::write(&config, "[agent]\ncommand = [\"echo\", \"agent-got\"]\n").expect("config");
    let empty = herdr.project("empty", None);

    let root = herdr.workspace(&empty);
    herdr.popup(&root);
    herdr.wait_for(&root, "No manifest here");
    herdr.cli(&["pane", "send-keys", &root, "g"]);

    let split = settle("a split beside the popup", || {
        herdr
            .panes()
            .into_iter()
            .find(|pane| pane["pane_id"] != root.as_str())
            .and_then(|pane| pane["pane_id"].as_str().map(str::to_string))
    });
    herdr.wait_for(
        &split,
        &format!("agent-got /herdr-stack-manifest {}", empty.display()),
    );
}
