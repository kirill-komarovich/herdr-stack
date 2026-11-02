//! Against a real Herdr 2 core, `hc`: the popup run as one of its terminals, finding its project, and
//! opening what `O` and `g` open as terminals of that core.
//!
//! Each test starts a core of its own: its own `HC_SESSION`, its state and config under `/tmp`, and a
//! `HOME` there too, so neither the user's core nor their herdr-stack daemon is ever reached. Every hc
//! client command starts a core when none answers, which is why every one of them gets that
//! environment and nothing else.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use herdr_stack::client::Endpoint;
use serde_json::{Value, json};

const PATIENCE: Duration = Duration::from_secs(20);
const STEP: Duration = Duration::from_millis(100);
const SPACE: &str = "spTest";
const TAB: &str = "tbTest";

fn stack_exe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_herdr-stack"))
}

/// Resolved once, here, so no shim on the caller's `PATH` runs under the core's private
/// `XDG_CONFIG_HOME`.
fn hc_exe() -> PathBuf {
    if let Some(exe) = std::env::var_os("HC_BIN") {
        return PathBuf::from(exe);
    }
    let home = std::env::var_os("HOME").expect("HOME");
    let installed = Path::new(&home).join(".local/bin/hc");
    assert!(
        installed.is_file(),
        "no hc at {}: set HC_BIN",
        installed.display()
    );
    installed
}

struct Core {
    root: PathBuf,
    session: String,
    hc: PathBuf,
}

impl Core {
    fn start(name: &str) -> Core {
        let root = PathBuf::from(format!("/tmp/hs-hct-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "state", "config"] {
            std::fs::create_dir_all(root.join(dir)).expect("core dirs");
        }
        // A terminal's cwd is the one the OS resolves, `/private/tmp` on macOS, and the daemon keys a
        // project by its path, so the CLI has to be handed the same spelling.
        let root = root.canonicalize().expect("core root");
        let core = Core {
            root,
            session: format!("hs-test-{name}"),
            hc: hc_exe(),
        };
        let started = core.hc(&["server", "start"]);
        assert!(
            started.status.success(),
            "hc server start: {}",
            said(&started)
        );
        settle("the core's socket", || core.socket().exists().then_some(()));
        core
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn socket(&self) -> PathBuf {
        let lock = self
            .root
            .join("state/herdr-core")
            .join(&self.session)
            .join("core.lock");
        PathBuf::from(std::fs::read_to_string(lock).unwrap_or_default().trim())
    }

    fn isolated(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("HOME", self.home())
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("SHELL", "/bin/sh")
            .env("LANG", "en_US.UTF-8")
            .env("HC_SESSION", &self.session)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .current_dir(self.home())
            .stdin(Stdio::null());
        command
    }

    fn hc(&self, args: &[&str]) -> Output {
        self.isolated(&self.hc).args(args).output().expect("hc")
    }

    fn stack(&self, args: &[&str]) -> Output {
        self.isolated(stack_exe())
            .args(args)
            .output()
            .expect("herdr-stack")
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        let hc = herdr_stack::hc::Hc {
            socket: self.socket(),
            terminal: String::new(),
        };
        hc.request(method, params)
            .unwrap_or_else(|error| panic!("{method}: {error}"))
    }

    fn project(&self, name: &str, manifest: Option<&str>) -> PathBuf {
        let dir = self.home().join(name);
        std::fs::create_dir_all(&dir).expect("project");
        if let Some(manifest) = manifest {
            std::fs::write(dir.join(".herdr-stack.toml"), manifest).expect("manifest");
        }
        dir
    }

    /// The popup, in a terminal a window would show in its test space and tab.
    fn popup(&self, cwd: &Path) -> String {
        let ran = self.rpc(
            "terminal.run",
            json!({
                "argv": [stack_exe()],
                "cwd": cwd,
                "labels": {"herdr.space": SPACE, "herdr.tab": TAB},
                "env": {"TERM": "xterm-256color"},
            }),
        );
        ran["address"].as_str().expect("an address").to_string()
    }

    fn keys(&self, terminal: &str, keys: &[&str]) {
        self.rpc("terminal.keys", json!({"terminal": terminal, "keys": keys}));
    }

    fn screen(&self, terminal: &str) -> String {
        let read = self.hc(&["read", terminal]);
        String::from_utf8_lossy(&read.stdout).into_owned()
    }

    fn wait_for(&self, terminal: &str, text: &str) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let screen = self.screen(terminal);
            if screen.replace('\n', "").contains(text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "`{text}` never showed in {terminal}\n--- {terminal} ---\n{screen}"
            );
            std::thread::sleep(STEP);
        }
    }

    fn wait_until_up(&self, terminal: &str, unit: &str) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let screen = self.screen(terminal);
            let up = screen.lines().any(|line| {
                let words: Vec<&str> = line.split_whitespace().collect();
                words.windows(3).any(|row| row == [unit, "local", "up"])
            });
            if up {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{unit} never came up in {terminal}\n{screen}"
            );
            std::thread::sleep(STEP);
        }
    }

    fn terminal_labelled(&self, key: &str, value: &str) -> Value {
        settle(&format!("a terminal labelled {key}={value}"), || {
            let listed = self.rpc("terminal.list", json!({"labels": {key: value}}));
            listed["terminals"].as_array()?.first().cloned()
        })
    }

    fn daemon_pid(&self) -> Option<u32> {
        let root = self.home().join(".local/state/herdr/plugins/herdr-stack");
        Endpoint::at(root)
            .connect_within(Duration::from_millis(300))
            .ok()
            .map(|link| link.peer().pid)
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid() {
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            let _ = settle_within(Duration::from_secs(10), || {
                (unsafe { libc::kill(pid as i32, 0) } != 0).then_some(())
            });
        }
        let _ = self.hc(&["server", "stop"]);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn settle<T>(what: &str, probe: impl FnMut() -> Option<T>) -> T {
    settle_within(PATIENCE, probe).unwrap_or_else(|| panic!("gave up waiting for {what}"))
}

fn settle_within<T>(patience: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + patience;
    loop {
        if let Some(found) = probe() {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(STEP);
    }
}

const TICKER: &str = "[local.ticker]\ncmd = [\"sh\", \"-c\", \"echo ticker-up; exec sleep 600\"]\n";

#[test]
fn the_popup_opens_a_running_units_log_in_a_new_tab_of_its_space() {
    let core = Core::start("log");
    let project = core.project("proj", Some(TICKER));
    let started = core.stack(&["start", "--project", &project.to_string_lossy(), "ticker"]);
    assert!(started.status.success(), "{}", said(&started));

    let popup = core.popup(&project);
    core.wait_until_up(&popup, "ticker");
    core.keys(&popup, &["shift+o"]);

    let log = core.terminal_labelled("herdr-stack.unit", "ticker");
    let address = log["address"].as_str().expect("an address");
    assert_eq!(log["labels"]["herdr.space"], SPACE);
    assert!(log["labels"].get("herdr.tab").is_none(), "{log}");
    assert_eq!(log["labels"]["herdr-stack.view"], "tail");
    assert_eq!(log["parent"], Value::Null);
    core.wait_for(address, "ticker-up");
    settle("the log's title", || {
        (core.rpc("terminal.get", json!({"terminal": address}))["title"] == "ticker").then_some(())
    });
}

#[test]
fn the_popup_finds_its_project_where_the_rest_of_its_space_sits() {
    let core = Core::start("space");
    let project = core.project("proj", Some(TICKER));
    let elsewhere = core.project("elsewhere", None);
    core.rpc(
        "terminal.run",
        json!({"argv": ["sleep", "600"], "cwd": project, "labels": {"herdr.space": SPACE}}),
    );

    let popup = core.popup(&elsewhere);
    core.wait_for(&popup, "ticker");
}

#[test]
fn the_popup_attaches_a_terminal_that_types_at_a_tty_unit() {
    let core = Core::start("attach");
    let project = core.project(
        "proj",
        Some(
            "[local.repl]\ncmd = [\"sh\", \"-c\", \"while read line; do echo \\\"got $line\\\"; done\"]\ntty = true\n",
        ),
    );
    let started = core.stack(&["start", "--project", &project.to_string_lossy(), "repl"]);
    assert!(started.status.success(), "{}", said(&started));

    let popup = core.popup(&project);
    core.wait_until_up(&popup, "repl");
    core.keys(&popup, &["shift+o"]);

    let attached = core.terminal_labelled("herdr-stack.unit", "repl");
    let address = attached["address"].as_str().expect("an address");
    assert_eq!(attached["labels"]["herdr-stack.view"], "attach");
    core.wait_for(address, "attached");
    core.keys(address, &["p", "i", "n", "g", "enter"]);
    core.wait_for(address, "got ping");
}

#[test]
fn units_the_popup_started_outlive_its_terminal() {
    let core = Core::start("outlive");
    let project = core.project("proj", Some(TICKER));

    let popup = core.popup(&project);
    core.wait_for(&popup, "ticker");
    core.keys(&popup, &["s"]);
    core.wait_until_up(&popup, "ticker");
    let daemon = core.daemon_pid().expect("a daemon");
    core.rpc("terminal.kill", json!({"terminal": popup}));

    let status = core.stack(&["status", "--project", &project.to_string_lossy()]);
    let status: Value = serde_json::from_slice(&status.stdout).expect("status json");
    let ticker = status["units"]
        .as_array()
        .and_then(|units| units.iter().find(|unit| unit["name"] == "ticker"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(ticker["state"], "up", "{status}");
    assert_eq!(core.daemon_pid(), Some(daemon));
}

#[test]
fn an_empty_project_is_handed_to_the_configured_agent_beside_the_popup() {
    let core = Core::start("agent");
    let config = core
        .home()
        .join(".config/herdr/plugins/config/herdr-stack/config.toml");
    std::fs::create_dir_all(config.parent().expect("dir")).expect("config dir");
    std::fs::write(&config, "[agent]\ncommand = [\"echo\", \"agent-got\"]\n").expect("config");
    let empty = core.project("empty", None);

    let popup = core.popup(&empty);
    core.wait_for(&popup, "No manifest here");
    core.keys(&popup, &["g"]);

    let agent = settle("a terminal beside the popup", || {
        let listed = core.rpc("terminal.list", json!({"labels": {"herdr.tab": TAB}}));
        listed["terminals"]
            .as_array()?
            .iter()
            .find(|terminal| terminal["address"] != popup.as_str())
            .cloned()
    });
    assert_eq!(agent["labels"]["herdr.space"], SPACE);
    let address = agent["address"].as_str().expect("an address");
    core.wait_for(
        address,
        &format!("agent-got /herdr-stack-manifest {}", empty.display()),
    );
}
