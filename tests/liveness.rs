//! The popup between keystrokes, and while the daemon is taking its time.
//!
//! The daemon here answers the handshake and the first status read and then goes quiet without
//! hanging up. That is the shape of every slow answer — a `compose ps` on a loaded machine, an
//! `up -d --wait` inside a healthcheck's start period — held still, so a test can press keys during
//! one and see what the rows do with nobody pressing anything at all.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use herdr_dev::daemon::PROTOCOL;

mod support;

use support::{Pty, answer_snapshots};

const COLS: u16 = 120;
const MANIFEST: &str = "[local.vite]\ncmd = [\"sleep\", \"30\"]\n";
/// What the one status read ever answered says, and what the rows have to grow from.
const UPTIME: u64 = 41_000;

struct Stage {
    root: PathBuf,
    home: PathBuf,
    pty: Option<Pty>,
}

impl Stage {
    fn set(name: &str) -> Stage {
        let root = support::staging(&format!("hd-live-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".config/herdr")).expect("a herdr config dir");
        std::fs::create_dir_all(home.join(".local/state/herdr/plugins/herdr-dev"))
            .expect("a state root");
        Stage {
            root,
            home,
            pty: None,
        }
    }

    fn repo(&self) -> PathBuf {
        let dir = self.root.join(format!("hdl{}repo", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a repo");
        std::fs::write(dir.join(".herdr-dev.toml"), MANIFEST).expect("a manifest");
        dir
    }

    fn look_at(&mut self, repo: &Path) -> Arc<AtomicUsize> {
        answer_snapshots(
            self.home.join(".config/herdr/herdr.sock"),
            repo.to_path_buf(),
        );
        let reads = stalling_daemon(
            self.home
                .join(".local/state/herdr/plugins/herdr-dev/daemon.sock"),
        );
        self.pty = Some(Pty::of(&self.home, repo, COLS));
        reads
    }

    fn pty(&mut self) -> &mut Pty {
        self.pty.as_mut().expect("a popup")
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if let Some(pty) = self.pty.as_mut() {
            pty.close();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Answers a handshake always and the first status read once, and then says nothing to anything while
/// keeping every connection open. The count is of status reads asked for, so a test can tell a popup
/// that stopped asking from one that is quietly asking over and over.
fn stalling_daemon(socket: PathBuf) -> Arc<AtomicUsize> {
    let listener = UnixListener::bind(&socket).expect("a daemon socket of our own");
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&reads);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let reading = stream.try_clone().expect("clone");
            let counted = Arc::clone(&counted);
            std::thread::spawn(move || {
                let mut writer = &stream;
                for line in BufReader::new(reading).lines() {
                    let Ok(line) = line else { break };
                    let asked: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                    let id = asked.get("id").cloned().unwrap_or_default();
                    let reply = match asked.get("method").and_then(|method| method.as_str()) {
                        Some("handshake") => serde_json::json!({"id": id, "result": {
                            "version": "9.9.9", "protocol": PROTOCOL, "pid": 424_242,
                        }}),
                        Some("status") if counted.fetch_add(1, Ordering::SeqCst) == 0 => {
                            serde_json::json!({"id": id, "result": {"units": {
                                "local-vite": {"state": "up", "uptime_ms": UPTIME},
                            }}})
                        }
                        // Everything else is taken and never answered.
                        _ => continue,
                    };
                    if writeln!(writer, "{reply}").is_err() {
                        break;
                    }
                }
            });
        }
    });
    reads
}

fn row_of(screen: &str, name: &str) -> String {
    screen
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some(name))
        .unwrap_or_else(|| panic!("no row for {name} on screen:\n{screen}"))
        .to_string()
}

#[test]
fn an_uptime_climbs_with_nobody_pressing_anything() {
    let mut stage = Stage::set("uptime");
    let repo = stage.repo();
    stage.look_at(&repo);

    // 41s is what the daemon said; every second after it is the popup's own doing, and no key is
    // pressed between this line and the next.
    assert!(stage.pty().wait_for("41s").contains("vite"));
    let before = stage.pty().cpu();
    let climbed = stage.pty().wait_for("44s");
    assert!(row_of(&climbed, "vite").contains("up"), "{climbed}");
    // A clock is worth nothing that a laptop's fan can hear: an unchanged frame writes nothing, so
    // the wakeups the seconds cost are all this is.
    let spent = stage.pty().cpu() - before;
    assert!(
        spent < 0.5,
        "three seconds of rows cost {spent}s of processor time"
    );
    assert!(stage.pty().alive(), "the popup died");
}

#[test]
fn the_keyboard_never_waits_on_a_verb_the_daemon_is_still_thinking_about() {
    let mut stage = Stage::set("verb");
    let repo = stage.repo();
    let reads = stage.look_at(&repo);
    stage.pty().wait_for("41s");

    // The start is taken and never answered, which is a `up -d --wait` sitting inside a start period.
    stage.pty().press("s");
    stage.pty().wait_for("starting vite");

    // Everything the keyboard does while that start is outstanding still happens: a peek opens over
    // the rows, closes again, and the rows are still counting.
    stage.pty().press("L");
    stage.pty().wait_for("esc rows");
    stage.pty().press("\u{1b}");
    stage.pty().wait_for("s start");
    stage.pty().wait_for("46s");

    // The beat has come round several times by now, and exactly one read is out: the one that was
    // answered and the one that never was. A read nobody replied to must not become a queue of them.
    assert_eq!(reads.load(Ordering::SeqCst), 2);

    stage.pty().press("q");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while stage.pty().alive() {
        assert!(
            std::time::Instant::now() < deadline,
            "the popup would not quit while a verb was outstanding"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
