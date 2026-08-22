//! The footer at the widths a popup really gets. A window is routinely narrower than the 180 columns
//! the other pty tests ask for, and 90% of a narrow window is where a sentence-length notice used to be
//! cut off mid-word — so everything here is drawn at 80 and 100 columns.
//!
//! No daemon is started and no docker is touched: the socket §8 spells out is answered by this file, out
//! of a `HOME` of its own under `/tmp`, so the only process any test here signals is the popup it
//! spawned. The pid the fake daemon claims is never signalled by anything — it is a string in a banner.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use herdr_dev::daemon::PROTOCOL;
use herdr_dev::peek::NO_REPO_LOG;
use herdr_dev::view::NOT_A_REPO;

mod support;

use support::{Pty, answer_snapshots};

const DOWN: &str = "j";
const TAB: &str = "\t";
const NARROW: u16 = 80;
const MIDDLING: u16 = 100;
const VERSION: &str = "9.9.9";
const PID: u32 = 424242;

/// A `HOME` of its own with something answering both sockets, and a popup looking at one throwaway repo.
struct Stage {
    root: PathBuf,
    home: PathBuf,
    pty: Option<Pty>,
}

impl Stage {
    fn set(name: &str) -> Stage {
        let root = support::staging(&format!("hd-foot-{name}"));
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

    fn repo(&self, name: &str, manifest: &str) -> PathBuf {
        let dir = self.root.join(format!("hdf{}{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a repo");
        std::fs::write(dir.join(".herdr-dev.toml"), manifest).expect("a manifest");
        dir
    }

    /// The popup at a width of this test's choosing, talking to a daemon that only says what it is told
    /// to. `protocol` is what decides skew, so a wrong one stages the longest banner the footer has.
    fn look_at(&mut self, repo: &Path, cols: u16, protocol: u64) {
        answer_snapshots(
            self.home.join(".config/herdr/herdr.sock"),
            repo.to_path_buf(),
        );
        fake_daemon(
            self.home
                .join(".local/state/herdr/plugins/herdr-dev/daemon.sock"),
            protocol,
        );
        self.pty = Some(Pty::of(&self.home, repo, cols));
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

/// A handshake and a status read, and nothing else: the reply carries every field either answer needs,
/// because the client matches no ids and reads only what it asked for.
fn fake_daemon(socket: PathBuf, protocol: u64) {
    let listener = UnixListener::bind(&socket).expect("a daemon socket of our own");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let reading = stream.try_clone().expect("clone");
            let mut writer = &stream;
            for line in BufReader::new(reading).lines() {
                let Ok(line) = line else { break };
                let asked: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                let reply = serde_json::json!({
                    "id": asked.get("id").cloned().unwrap_or_default(),
                    "result": {
                        "version": VERSION, "protocol": protocol, "pid": PID, "units": {},
                    },
                });
                if writeln!(writer, "{reply}").is_err() {
                    break;
                }
            }
        }
    });
}

/// The line a notice was drawn on, without the mark it wears, or nothing if no line holds it whole.
fn notice_line(screen: &str, needle: &str) -> Option<String> {
    screen
        .lines()
        .find(|line| line.contains(needle))
        .map(|line| line.trim().trim_start_matches("! ").trim_end().to_string())
}

fn keys_line(screen: &str) -> String {
    screen
        .lines()
        .find(|line| line.contains("q quit"))
        .unwrap_or_else(|| panic!("the keys are nowhere on screen:\n{screen}"))
        .trim_end()
        .to_string()
}

fn heading(screen: &str) -> String {
    screen.lines().next().expect("a heading").to_string()
}

/// Every notice on screen, read back as one sentence per notice: a wrapped notice is only readable if
/// the pieces join up to what it said.
fn read_back(screen: &str) -> Vec<String> {
    let mut said: Vec<String> = Vec::new();
    for line in screen.lines() {
        let trimmed = line.trim_end();
        match trimmed.strip_prefix("! ") {
            Some(first) => said.push(first.to_string()),
            None => {
                if let (Some(last), true) = (said.last_mut(), trimmed.starts_with("  ")) {
                    last.push(' ');
                    last.push_str(trimmed.trim());
                }
            }
        }
    }
    said
}

#[test]
fn a_notice_too_long_for_the_keys_line_is_read_in_full_at_eighty_columns() {
    let mut stage = Stage::set("narrow");
    let inner = stage.repo("accounts", "[local.rails]\ncmd = [\"rails\", \"s\"]\n");
    let outer = stage.repo(
        "storefront",
        &format!(
            "[local.idle]\ncmd = [\"sleep\", \"300\"]\n[includes.accounts]\npath = \"{}\"\n",
            inner.display()
        ),
    );
    stage.look_at(&outer, NARROW, PROTOCOL);
    stage.pty().wait_for("accounts");

    // `L` on the repo row: the longest refusal the rows have, and one that never fitted beside the keys.
    stage.pty().press(DOWN);
    stage.pty().press("L");
    let screen = stage.pty().wait_for(NO_REPO_LOG);

    assert_eq!(
        notice_line(&screen, NO_REPO_LOG).as_deref(),
        Some(NO_REPO_LOG),
        "the notice is not on a line of its own:\n{screen}"
    );
    // The keys and the daemon are both still there to be found.
    let keys = keys_line(&screen);
    assert!(keys.contains("L log") && keys.contains("↹ repo"), "{keys}");
    assert!(keys.chars().count() <= NARROW as usize, "{keys}");
    assert!(
        heading(&screen).contains(&format!("daemon {VERSION}")),
        "{screen}"
    );
    assert!(heading(&screen).contains(&PID.to_string()), "{screen}");
}

#[test]
fn a_notice_is_read_in_full_at_a_hundred_columns_too() {
    let mut stage = Stage::set("middling");
    let repo = stage.repo("storefront", "[docker]\nnames = [\"db\"]\n");
    stage.look_at(&repo, MIDDLING, PROTOCOL);
    stage.pty().wait_for("db");

    stage.pty().press(TAB);
    let screen = stage.pty().wait_for(NOT_A_REPO);
    assert_eq!(
        notice_line(&screen, NOT_A_REPO).as_deref(),
        Some(NOT_A_REPO),
        "{screen}"
    );
    assert!(keys_line(&screen).contains("O overlay"), "{screen}");
    assert!(
        heading(&screen).contains(&format!("daemon {VERSION}")),
        "{screen}"
    );
}

/// The banner is long by design — it names the process to kill — and it is the one thing that must not
/// be the casualty of a narrow popup.
#[test]
fn the_daemon_skew_banner_survives_eighty_columns_whole() {
    let mut stage = Stage::set("skew");
    let repo = stage.repo("storefront", "[local.idle]\ncmd = [\"sleep\", \"300\"]\n");
    stage.look_at(&repo, NARROW, PROTOCOL + 90);
    let screen = stage.pty().wait_for("kill");

    let banner = format!(
        "daemon {VERSION} pid {PID} speaks protocol {}, this build speaks {PROTOCOL} \
         — kill {PID} to clear it",
        PROTOCOL + 90
    );
    assert!(banner.chars().count() > NARROW as usize, "{banner}");
    assert_eq!(read_back(&screen), vec![banner], "{screen}");
    assert!(heading(&screen).contains("daemon skewed"), "{screen}");
    assert!(keys_line(&screen).contains("q quit"), "{screen}");
}

/// The row list pays for a notice only while one is up: with nothing to say the footer is the one line
/// it has always been, and the last row of a full list is still drawn.
#[test]
fn the_row_list_keeps_its_last_row_until_a_notice_needs_one() {
    let mut stage = Stage::set("no-notice");
    let units: String = (1..=22)
        .map(|nth| format!("[local.u{nth:02}]\ncmd = [\"sleep\", \"300\"]\n"))
        .collect();
    let repo = stage.repo("storefront", &units);
    stage.look_at(&repo, NARROW, PROTOCOL);

    let full = stage.pty().wait_for("u22");
    assert!(full.lines().count() >= 24);
    assert!(
        read_back(&full).is_empty(),
        "nothing was asked for, so nothing is said:\n{full}"
    );

    stage.pty().press(TAB);
    let screen = stage.pty().wait_for(NOT_A_REPO);
    assert!(
        !screen.contains("u22"),
        "the notice took no room from the list:\n{screen}"
    );
    assert_eq!(
        notice_line(&screen, NOT_A_REPO).as_deref(),
        Some(NOT_A_REPO),
        "{screen}"
    );
}

/// §5: a manifest is rendered anyway and says what it could not make sense of — the focused project's
/// complaints from the start, and an included repo's once it is unfolded.
#[test]
fn a_manifests_own_complaints_are_said_in_the_footer() {
    let mut stage = Stage::set("problems");
    let inner = stage.repo(
        "accounts",
        "[local.sidekiq]\ncmd = [\"sidekiq\"]\n[local.rails]\ncwd = \"/tmp\"\n",
    );
    let outer = stage.repo(
        "storefront",
        &format!(
            "[docker]\nnames = [\"db\"]\none_shot = [\"ghost\"]\n\
             [includes.accounts]\npath = \"{}\"\n",
            inner.display()
        ),
    );
    stage.look_at(&outer, NARROW, PROTOCOL);
    let screen = stage.pty().wait_for("ghost");
    assert_eq!(
        read_back(&screen),
        vec![
            "`docker.one_shot` names `ghost`, which is in neither `names` nor `hidden`".to_string()
        ],
        "{screen}"
    );

    stage.pty().press(DOWN);
    stage.pty().press(TAB);
    let unfolded = stage.pty().wait_for("accounts: ");
    let said = read_back(&unfolded);
    assert_eq!(said.len(), 2, "{unfolded}");
    assert!(
        said[1].starts_with("accounts: [local.rails]") && said[1].contains("cmd"),
        "{said:?}"
    );
}
