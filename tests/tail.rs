//! `tail` mode as the plugin pane really runs it: the binary, the log path in the environment, and a
//! restart happening under it. Every process started here is killed here.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(5);
const STEP: Duration = Duration::from_millis(50);

struct Tailing {
    child: Child,
    out: PathBuf,
}

impl Tailing {
    fn of(log: &Path, out: PathBuf) -> Tailing {
        let child = Command::new(env!("CARGO_BIN_EXE_herdr-dev"))
            .arg("tail")
            .env("HERDR_DEV_LOG", log)
            .stdout(std::fs::File::create(&out).expect("stdout file"))
            .stderr(Stdio::null())
            .spawn()
            .expect("tail spawns");
        Tailing { child, out }
    }

    fn wait_for(&self, needle: &str) -> String {
        let deadline = Instant::now() + PATIENCE;
        let mut seen = String::new();
        while Instant::now() < deadline {
            seen = std::fs::read_to_string(&self.out).unwrap_or_default();
            if seen.contains(needle) {
                return seen;
            }
            sleep(STEP);
        }
        panic!("tail never showed {needle:?}; it showed {seen:?}");
    }
}

impl Drop for Tailing {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn append(log: &Path, line: &str) {
    let mut file = OpenOptions::new().append(true).open(log).expect("append");
    write!(file, "{line}").expect("append");
}

#[test]
fn the_pane_follows_appended_lines_and_survives_the_log_being_truncated() {
    let dir = std::env::temp_dir().join("herdr-dev-tail-mode");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    let log = dir.join("local-vite.log");
    std::fs::write(&log, "first generation\n").expect("log");

    let tailing = Tailing::of(&log, dir.join("pane.out"));
    tailing.wait_for("first generation");
    append(&log, "still running\n");
    tailing.wait_for("still running");

    // What a restart does to the log: rotate the old generation aside, create the log anew.
    std::fs::rename(&log, dir.join("local-vite.log.1")).expect("rotate");
    std::fs::write(&log, "after the restart\n").expect("fresh log");
    let seen = tailing.wait_for("after the restart");
    assert!(seen.contains("restarted"), "{seen}");

    drop(tailing);
    let _ = std::fs::remove_dir_all(&dir);
}
