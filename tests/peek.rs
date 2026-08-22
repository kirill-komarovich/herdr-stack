//! The log peek as a user meets it: the real binary, drawn into a pty of this test's own making, keys
//! written into it and the screen read back with the escape sequences taken out.
//!
//! Nothing here can touch the state root §8 spells out, because `HOME` is a throwaway directory under
//! the temporary one: both the state root and the herdr control socket the popup dials are derived from
//! it, and the socket is answered by this file rather than by Herdr. The only processes signalled are
//! the popup this test spawned and the daemon that popup started, and every test ends with both gone.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use herdr_dev::client::{Endpoint, Link, Target};
use herdr_dev::docker::DOCKER;
use herdr_dev::local;
use herdr_dev::manifest::Project;
use herdr_dev::peek::Source;
use herdr_dev::tail::RESTARTED;
use herdr_dev::unit;

mod support;

use support::{PATIENCE, Pty, STEP, answer_snapshots};

/// Wider than a window ever makes a popup: what a narrow one does to the footer is `footer.rs`'s.
const COLS: u16 = 180;

fn staging(name: &str) -> PathBuf {
    support::staging(&format!("hd-peek-{name}"))
}

/// Everything one popup needs to exist: a HOME of its own, a project, and something answering
/// `session.snapshot` so the popup can resolve which project the key was about.
struct Stage {
    root: PathBuf,
    project: Project,
    pty: Pty,
    composed: bool,
}

impl Stage {
    fn set(name: &str, manifest: &str) -> Stage {
        let root = staging(name);
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".local/bin")).expect("a home");
        std::fs::create_dir_all(home.join(".config/herdr")).expect("a herdr config dir");
        // §6 spawns through mise at a spelled-out path under HOME, and this HOME is not the real one.
        std::os::unix::fs::symlink(local::mise_path(), home.join(".local/bin/mise")).expect("mise");
        // Docker Desktop installs its cli-plugins under HOME, so `docker compose` is not a subcommand
        // any more once HOME moves. Only the plugins are borrowed, never the real docker config.
        let plugins =
            PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".docker/cli-plugins");
        if plugins.is_dir() {
            std::fs::create_dir_all(home.join(".docker")).expect("a docker dir");
            std::os::unix::fs::symlink(plugins, home.join(".docker/cli-plugins")).expect("plugins");
        }

        // Compose takes a project name from the directory basename and addresses a project by *name*: a
        // directory called `storefront` here would let a compose verb reach the real storefront.
        let dir = root.join(format!("hdproj{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a project");
        let path = dir.join(".herdr-dev.toml");
        std::fs::write(&path, manifest).expect("manifest");
        let project = Project::load(&path).expect("the manifest parses");

        answer_snapshots(home.join(".config/herdr/herdr.sock"), dir);
        let pty = Pty::of(&home, &project.root, COLS);
        Stage {
            root,
            project,
            pty,
            composed: false,
        }
    }

    fn state(&self) -> PathBuf {
        self.root.join("home/.local/state/herdr/plugins/herdr-dev")
    }

    fn link(&self) -> Link {
        Endpoint::at(self.state())
            .connect_within(PATIENCE)
            .expect("a link to the daemon the popup started")
    }

    fn unit(&self, name: &str) -> Target<'_> {
        Target::of(&self.project, unit::LOCAL, name).expect("the manifest declares the unit")
    }

    /// A compose project of the popup's own, found by §11's default file discovery.
    fn compose(&mut self, service: &str) {
        std::fs::write(
            self.project.root.join("docker-compose.yml"),
            format!(
                "services:\n  {service}:\n    image: alpine:latest\n    \
                 command: [\"sh\", \"-c\", \"n=0; while :; do n=$$((n+1)); echo tick $$n; sleep 0.2; done\"]\n",
            ),
        )
        .expect("a compose file");
        self.composed = true;
        let up = Command::new(DOCKER)
            .args(["compose", "up", "-d", service])
            .current_dir(&self.project.root)
            .output()
            .expect("docker compose up runs");
        assert!(up.status.success(), "{up:?}");
    }

    /// What the counter unit prints, so one generation can be told from the next.
    fn generation(&self, mark: &str) {
        std::fs::write(self.root.join("generation"), mark).expect("generation");
    }

    fn generation_file(&self) -> PathBuf {
        self.root.join("generation")
    }
}

/// The popup goes first — it owns nothing — then the daemon it started, whose exit takes every unit
/// with it (§7).
impl Drop for Stage {
    fn drop(&mut self) {
        self.pty.close();
        if self.composed {
            let _ = Command::new(DOCKER)
                .args(["compose", "down", "-v", "--remove-orphans"])
                .current_dir(&self.project.root)
                .output();
        }
        if let Ok(link) = Endpoint::at(self.state()).connect_within(Duration::from_secs(2)) {
            let daemon = link.peer().pid;
            drop(link);
            assert_eq!(unsafe { libc::kill(daemon as i32, libc::SIGTERM) }, 0);
            let deadline = Instant::now() + PATIENCE;
            while local::alive(daemon) && Instant::now() < deadline {
                std::thread::sleep(STEP);
            }
            assert!(!local::alive(daemon), "the daemon outlived the test");
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A unit whose every line names the generation it belongs to, so a restart is visible in its output
/// rather than inferred from it.
fn counter_manifest(generation: &Path) -> String {
    format!(
        "[local.counter]\ncmd = [\"sh\", \"-c\", \"gen=$(cat {}); n=0; while :; do n=$((n+1)); echo \\\"gen$gen line $n\\\"; sleep 0.2; done\"]\n",
        generation.display()
    )
}

#[test]
fn a_peek_restarted_under_it_shows_the_fresh_generation_instead_of_the_end_of_the_old_log() {
    let generation = staging("restart").join("generation");
    let mut stage = Stage::set("restart", &counter_manifest(&generation));
    assert_eq!(stage.generation_file(), generation);
    stage.generation("1");

    stage.pty.wait_for("counter");
    stage.pty.press("s");
    stage.pty.wait_for("up");

    stage.pty.press("L");
    stage.pty.wait_for("gen1 line 3");
    assert!(stage.pty.wait_for("following").contains("f follow"));

    // The restart goes through the daemon while the peek is open, which is exactly the case §12 says a
    // peek must survive: the log is rotated aside and created anew under the follower.
    stage.generation("2");
    let mut link = stage.link();
    link.restart(&stage.project, &stage.unit("counter"))
        .expect("restart");

    // The screen itself, not the history of what was ever drawn into the pty: the previous generation
    // is gone from it and the fresh one is on it.
    let screen = stage.pty.wait_for("gen2 line 2");
    assert!(
        screen.contains(RESTARTED),
        "the peek never announced the fresh generation: {screen:?}"
    );
    assert!(
        !screen.contains("gen1 line"),
        "the peek is still showing the old generation: {screen:?}"
    );
    let counted = |screen: &str| {
        screen
            .split("gen2 line ")
            .skip(1)
            .filter_map(|rest| rest.split_whitespace().next()?.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
    };
    let first = counted(&screen);
    let later = counted(&stage.pty.wait_for(&format!("gen2 line {}", first + 3)));
    assert!(later > first, "the peek stopped following: {first} {later}");

    // The popup never died and the rows come back.
    assert!(stage.pty.alive(), "the popup died with the peek");
    stage.pty.press("\u{1b}");
    let rows = stage.pty.wait_for("s start");
    assert!(rows.contains("counter"), "{rows:?}");
    assert!(!rows.contains("gen2 line"), "{rows:?}");

    assert_eq!(link.stop(&stage.project, &stage.unit("counter")), Ok(None));
}

#[test]
fn a_repo_row_says_it_has_no_single_log_rather_than_peeking_nothing() {
    let mut stage = Stage::set(
        "repo-row",
        "[local.idle]\ncmd = [\"sleep\", \"300\"]\n\
         [includes.accounts]\npath = \"/repos/accounts\"\n",
    );
    stage.pty.wait_for("accounts");

    stage.pty.press("j");
    stage.pty.press("L");
    let screen = stage.pty.wait_for("no single log");
    assert!(screen.contains("s start"), "the rows went away: {screen:?}");
    assert!(stage.pty.alive(), "the popup died on a row with no log");
}

#[test]
fn following_needs_no_keypress_and_an_idle_peek_burns_no_processor_time() {
    let mut stage = Stage::set(
        "idle",
        "[local.quiet]\ncmd = [\"sh\", \"-c\", \"echo one quiet line; sleep 300\"]\n",
    );
    stage.pty.wait_for("quiet");
    stage.pty.press("s");
    stage.pty.wait_for("up");
    stage.pty.press("L");
    // Nothing was pressed between the peek opening and this line arriving in it.
    stage.pty.wait_for("one quiet line");

    let before = stage.pty.cpu();
    let idled = Duration::from_secs(3);
    std::thread::sleep(idled);
    let spent = stage.pty.cpu() - before;
    assert!(
        spent < 0.5,
        "an idle peek spent {spent}s of processor time in {idled:?}"
    );

    stage.pty.press("k");
    assert!(stage.pty.wait_for("paused").contains("paused 1/1"));
    stage.pty.press("f");
    stage.pty.wait_for("following");

    let mut link = stage.link();
    assert_eq!(link.stop(&stage.project, &stage.unit("quiet")), Ok(None));
}

/// A compose project of this test's own making, taken down again whether the test passes or panics.
/// The project name is the directory basename, made unmistakably this process's: compose addresses a
/// project by name, so a directory called `storefront` here would let `down` reach the real storefront.
struct Composed(PathBuf);

impl Composed {
    fn up(service: &str) -> Composed {
        let dir = PathBuf::from("/tmp").join(format!("hdpeek{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a compose project");
        std::fs::write(
            dir.join("docker-compose.yml"),
            format!(
                "name: {}\nservices:\n  {service}:\n    image: alpine:latest\n    \
                 command: [\"sh\", \"-c\", \"n=0; while :; do n=$$((n+1)); echo tick $$n; sleep 0.2; done\"]\n",
                dir.file_name().expect("a basename").to_string_lossy(),
            ),
        )
        .expect("a compose file");
        let composed = Composed(dir);
        assert!(
            Command::new(DOCKER)
                .args(["image", "inspect", "alpine:latest"])
                .output()
                .is_ok_and(|output| output.status.success()),
            "alpine:latest must already be in the local image store; nothing here pulls"
        );
        let up = Command::new(DOCKER)
            .args(["compose", "up", "-d", service])
            .current_dir(&composed.0)
            .output()
            .expect("docker compose up runs");
        assert!(up.status.success(), "{up:?}");
        composed
    }
}

impl Drop for Composed {
    fn drop(&mut self) {
        let _ = Command::new(DOCKER)
            .args(["compose", "down", "-v", "--remove-orphans"])
            .current_dir(&self.0)
            .output();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How many processes are streaming a compose log the way a peek does. Read only — nothing here signals
/// a process it did not spawn.
fn streaming() -> usize {
    let found = Command::new("/usr/bin/pgrep")
        .args(["-f", "compose logs --no-color --no-log-prefix --follow"])
        .output()
        .expect("pgrep runs");
    String::from_utf8_lossy(&found.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

#[test]
#[ignore = "creates and destroys its own compose project; needs docker and alpine:latest"]
fn a_docker_row_peeks_that_services_compose_logs_and_the_stream_ends_with_the_peek() {
    let composed = Composed::up("chatty");
    let streaming_before = streaming();

    let mut peek = Source::Compose {
        service: "chatty".to_string(),
        root: composed.0.clone(),
    }
    .open()
    .expect("compose logs starts");

    let deadline = Instant::now() + PATIENCE;
    let shown = loop {
        peek.pump();
        let shown = peek.view(20, 180);
        if shown.iter().any(|line| line.contains("tick 3")) {
            break shown;
        }
        assert!(Instant::now() < deadline, "the peek showed {shown:?}");
        std::thread::sleep(STEP);
    };
    assert_eq!(peek.trouble(), None, "{shown:?}");
    assert!(
        shown.iter().all(|line| !line.contains('\u{1b}')),
        "colour reached the screen: {shown:?}"
    );
    // The CLI and the compose plugin it execs both match, so the count is a floor rather than a number.
    assert!(
        streaming() > streaming_before,
        "the peek is not streaming compose logs"
    );

    drop(peek);
    let deadline = Instant::now() + PATIENCE;
    while streaming() > streaming_before && Instant::now() < deadline {
        std::thread::sleep(STEP);
    }
    assert_eq!(
        streaming(),
        streaming_before,
        "the compose stream outlived the peek"
    );
}

#[test]
#[ignore = "creates and destroys its own compose project; needs docker and alpine:latest"]
fn a_docker_row_peeks_compose_without_the_popup_going_deaf_to_the_keyboard() {
    let mut stage = Stage::set("docker", "[docker]\nnames = [\"chatty\"]\n");
    stage.pty.wait_for("chatty");
    stage.compose("chatty");

    stage.pty.press("L");
    stage.pty.wait_for("tick 3");
    // A key answered while the stream is running is the event loop still being the popup's own.
    stage.pty.press("k");
    assert!(stage.pty.wait_for("paused").contains("paused"));
    stage.pty.press("f");
    stage.pty.wait_for("following");

    stage.pty.press("\u{1b}");
    let rows = stage.pty.wait_for("s start");
    assert!(rows.contains("chatty"), "{rows:?}");
    assert!(!rows.contains("tick"), "{rows:?}");
    assert!(stage.pty.alive(), "the popup died in the peek");
}
