//! Included repos as a user meets them: the real binary drawn into a pty of this test's own making,
//! `↹` typed into it, and the screen read back.
//!
//! Every repo here is a throwaway under `/tmp` and `HOME` is one of its own, so the state root, the
//! daemon socket and the herdr socket the popup dials all land beside them — the last answered by this
//! file rather than by Herdr. The only processes signalled are the popup this test spawned and the
//! daemon that popup started, and every test ends with both gone.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use herdr_dev::client::{Endpoint, Link, Target};
use herdr_dev::local;
use herdr_dev::manifest::Project;
use herdr_dev::store::{LOG_LINK, project_key};
use herdr_dev::unit::{self, State, Status};

mod support;

use support::{PATIENCE, Pty, STEP, answer_snapshots};

/// Wider than a window ever makes a popup: what a narrow one does to the footer is `footer.rs`'s.
const COLS: u16 = 180;

const TAB: &str = "\t";
const DOWN: &str = "j";
const UP: &str = "k";
const ESC: &str = "\u{1b}";

const EXAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/manifests");

fn example(name: &str) -> String {
    std::fs::read_to_string(PathBuf::from(EXAMPLES).join(format!("{name}.herdr-dev.toml")))
        .unwrap_or_else(|error| panic!("{name}: {error}"))
}

/// A `HOME` of its own, a handful of repos in it, and a popup looking at one of them.
struct Stage {
    root: PathBuf,
    home: PathBuf,
    pty: Option<Pty>,
}

impl Stage {
    fn set(name: &str) -> Stage {
        let root = support::staging(&format!("hd-inc-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".local/bin")).expect("a home");
        std::fs::create_dir_all(home.join(".config/herdr")).expect("a herdr config dir");
        // A unit spawns through mise at a spelled-out path under HOME, and this HOME is not the real one.
        std::os::unix::fs::symlink(local::mise_path(), home.join(".local/bin/mise")).expect("mise");
        Stage {
            root,
            home,
            pty: None,
        }
    }

    /// Where a repo would be, before it is one: an include may name a directory that never appears.
    fn at(&self, name: &str) -> PathBuf {
        // Compose takes its project name from the directory basename and addresses a project by name,
        // so every directory here is unmistakably this process's.
        self.root.join(format!("hdi{}{name}", std::process::id()))
    }

    fn bare(&self, name: &str) -> PathBuf {
        let dir = self.at(name);
        std::fs::create_dir_all(&dir).expect("a repo");
        dir
    }

    fn repo(&self, name: &str, manifest: &str) -> PathBuf {
        let dir = self.bare(name);
        std::fs::write(dir.join(".herdr-dev.toml"), manifest).expect("a manifest");
        dir
    }

    /// The popup, looking at `repo` the way a keypress in a pane of it would leave it.
    fn look_at(&mut self, repo: &Path) {
        answer_snapshots(
            self.home.join(".config/herdr/herdr.sock"),
            repo.to_path_buf(),
        );
        self.pty = Some(Pty::of(&self.home, repo, COLS));
    }

    fn pty(&mut self) -> &mut Pty {
        self.pty.as_mut().expect("a popup")
    }

    fn state(&self) -> PathBuf {
        self.home.join(".local/state/herdr/plugins/herdr-dev")
    }

    /// What an included repo's own popup would ask the daemon: the same socket, its own project.
    fn link(&self) -> Link {
        Endpoint::at(self.state())
            .connect_within(PATIENCE)
            .expect("a link to the daemon the popup started")
    }

    fn project(&self, repo: &Path) -> Project {
        Project::load(repo.join(".herdr-dev.toml")).expect("the manifest parses")
    }
}

/// The popup goes first — it owns nothing — then the daemon it started, whose exit takes every unit
/// with it.
impl Drop for Stage {
    fn drop(&mut self) {
        if let Some(pty) = self.pty.as_mut() {
            pty.close();
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

/// Whether the row `name` sits on is one of an unfolded repo's: every other row has its name one
/// space after the glyph, and only a stepped-in one has two.
fn stepped_in(line: &str, name: &str) -> bool {
    line.contains(&format!("  {name}"))
}

/// The row a name sits on, as it is drawn: the state columns are only readable one row at a time.
fn row_of(screen: &str, name: &str) -> String {
    screen
        .lines()
        .find(|line| line.contains(name))
        .unwrap_or_else(|| panic!("no row for {name} on screen:\n{screen}"))
        .to_string()
}

/// Waits for the row `name` sits on to carry `needle`, so one row reading `up` is never mistaken for
/// another's.
fn wait_row(pty: &mut Pty, name: &str, needle: &str) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = pty.screen();
        if screen
            .lines()
            .any(|line| line.contains(name) && line.contains(needle))
        {
            return row_of(&screen, name);
        }
        assert!(
            Instant::now() < deadline,
            "no row for {name} ever read {needle:?}; the screen is\n{screen}"
        );
        std::thread::sleep(STEP);
    }
}

#[test]
fn a_unit_started_from_the_including_view_is_the_same_unit_from_the_included_repos_own() {
    let mut stage = Stage::set("same-unit");
    let inner = stage.repo(
        "accounts",
        "[local.ticker]\ncmd = [\"sh\", \"-c\", \"while :; do echo tick; sleep 0.2; done\"]\n",
    );
    let outer = stage.repo(
        "storefront",
        &format!(
            "[local.idle]\ncmd = [\"sleep\", \"300\"]\n[includes.accounts]\npath = \"{}\"\n",
            inner.display()
        ),
    );
    stage.look_at(&outer);

    let folded = stage.pty().wait_for("accounts");
    assert!(
        !folded.contains("ticker"),
        "the included repo unfolded uninvited:\n{folded}"
    );

    // `↹` on the repo row, then the cursor onto the unit it revealed.
    stage.pty().press(DOWN);
    stage.pty().press(TAB);
    let unfolded = stage.pty().wait_for("ticker");
    assert!(
        stepped_in(&row_of(&unfolded, "ticker"), "ticker"),
        "an unfolded row is not stepped in:\n{unfolded}"
    );
    assert!(row_of(&unfolded, "ticker").contains("local"));

    stage.pty().press(DOWN);
    stage.pty().press("s");
    wait_row(stage.pty(), "ticker", "up");

    // What the included repo's own popup would ask, on the same socket, with its own project: the unit
    // the including view started is up, and it is that project's unit rather than the other's.
    let mut link = stage.link();
    let inner_project = stage.project(&inner);
    let outer_project = stage.project(&outer);
    let ticker = unit::key(unit::LOCAL, "ticker");
    let seen = link.status(&inner_project).expect("a status read");
    assert_eq!(
        seen.get(&ticker).map(|status: &Status| status.state),
        Some(State::Up),
        "the included repo's own view does not see the unit: {seen:?}",
    );
    // The other side of the same fact: the unit belongs to the repo that declared it, so asking the
    // including project about that name says nothing at all.
    let claimed = link.status(&outer_project).expect("a status read");
    assert_eq!(
        claimed.get(&ticker),
        None,
        "the including project answered for someone else's unit: {claimed:?}",
    );

    // The record and the log are the included repo's, keyed by its own path.
    let logs = |repo: &Path| repo.join(LOG_LINK).join("local-ticker.log");
    assert!(logs(&inner).exists(), "no log under the included repo");
    assert!(
        !logs(&outer).exists(),
        "the including project kept a log of its own for someone else's unit"
    );
    let units = stage
        .state()
        .join("projects")
        .join(project_key(&inner))
        .join("units/local-ticker.toml");
    assert!(units.is_file(), "{} is not there", units.display());

    // The peek reaches the same file from the including view.
    stage.pty().press("L");
    let peeked = stage.pty().wait_for("f follow");
    let heading = peeked.lines().next().unwrap_or_default();
    assert!(
        heading.contains(&project_key(&inner)),
        "the peek is not reading the included repo's own log:\n{peeked}"
    );
    stage.pty().press(ESC);
    stage.pty().wait_for("s start");

    // Stopping from the included repo's own view is the same act: the including view's row goes down.
    assert_eq!(
        link.stop(
            &inner_project,
            &Target::of(&inner_project, unit::LOCAL, "ticker").expect("unit")
        ),
        Ok(None)
    );
    stage.pty().press(UP);
    let stopped = wait_row(stage.pty(), "ticker", "down");
    assert!(!stopped.contains("up"), "{stopped}");
    assert!(stage.pty().alive(), "the popup died");
}

#[test]
fn six_included_repos_stay_six_rows_and_each_one_that_cannot_be_read_says_why() {
    let mut stage = Stage::set("degraded");
    let accounts = stage.repo("accounts", &example("accounts"));
    let billing = stage.repo("billing", &example("billing"));
    let bare = stage.bare("backoffice");
    let broken = stage.repo("broken", "[local.rails\n");
    let storefront = stage.at("storefront");
    let storefront = stage.repo(
        "storefront",
        &format!(
            "[includes.accounts]\npath = \"{}\"\n\
             [includes.billing]\npath = \"{}\"\n\
             [includes.backoffice]\npath = \"{}\"\n\
             [includes.broken]\npath = \"{}\"\n\
             [includes.myself]\npath = \"{}\"\n\
             [includes.missing]\npath = \"{}\"\n",
            accounts.display(),
            billing.display(),
            bare.display(),
            broken.display(),
            storefront.display(),
            stage.at("never_cloned").display(),
        ),
    );
    stage.look_at(&storefront);

    let screen = stage.pty().wait_for("backoffice");
    assert!(
        row_of(&screen, "billing").contains("9 services + 3 processes, from its own config"),
        "{screen}"
    );
    assert!(
        row_of(&screen, "accounts").contains("5 services + 3 processes"),
        "{screen}"
    );
    assert!(
        row_of(&screen, "backoffice").contains("no .herdr-dev.toml in"),
        "{screen}"
    );
    assert!(
        row_of(&screen, "broken").contains("broken .herdr-dev.toml: TOML parse error"),
        "{screen}"
    );
    assert!(
        row_of(&screen, "myself").contains("this project itself"),
        "{screen}"
    );
    assert!(
        row_of(&screen, "missing").contains("no repo at"),
        "{screen}"
    );
    assert!(screen.contains("↹ repo"), "{screen}");
    for uninvited in ["dragonfly", "redis-sentinel-1", "sidekiq", "mongo"] {
        assert!(
            !screen.contains(uninvited),
            "{uninvited} unfolded uninvited:\n{screen}"
        );
    }

    // A repo row is not a unit: no verb means anything on one.
    stage.pty().press("s");
    let refused = stage.pty().wait_for("act on unit rows");
    assert!(!refused.contains("sidekiq"), "{refused}");

    // Neither does `↹` on any of the four that cannot be read.
    for _ in 0..2 {
        stage.pty().press(DOWN);
    }
    for _ in 0..4 {
        stage.pty().press(TAB);
        stage.pty().press(DOWN);
    }
    let still_folded = stage.pty().screen();
    assert_eq!(
        still_folded
            .lines()
            .filter(|line| line.contains('▸') || line.contains('▾'))
            .count(),
        6,
        "{still_folded}"
    );
    assert!(!still_folded.contains("sidekiq"), "{still_folded}");

    // And the one that can be read unfolds into its own units, one level deep.
    for _ in 0..6 {
        stage.pty().press(UP);
    }
    stage.pty().press(TAB);
    let unfolded = stage.pty().wait_for("sidekiq");
    for own in [
        "rails",
        "sidekiq",
        "vite",
        "db",
        "redis",
        "memcached",
        "mongo",
    ] {
        assert!(
            stepped_in(&row_of(&unfolded, own), own),
            "{own} is not stepped in:\n{unfolded}"
        );
    }
    assert!(
        !unfolded.contains("dragonfly"),
        "the wrong repo unfolded:\n{unfolded}"
    );
    assert!(stage.pty().alive(), "the popup died");
}

#[test]
fn storefronts_own_manifest_stays_legible_when_none_of_the_repos_it_names_holds_one() {
    let mut stage = Stage::set("storefront");
    let storefront = stage.repo("storefront", &example("storefront"));
    stage.look_at(&storefront);

    let screen = stage.pty().wait_for("backoffice");
    // `~` is the throwaway HOME, and nothing under it was ever cloned — the state of every one of
    // these repos on this machine today.
    for named in ["accounts", "billing", "backoffice"] {
        let row = row_of(&screen, named);
        assert!(row.contains("▸"), "{row}");
        assert!(
            row.contains(&format!("no repo at ~/projects/acme/{named}")),
            "{row}"
        );
    }
    assert!(row_of(&screen, " db ").contains("docker"), "{screen}");
    assert_eq!(
        screen
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        // The heading, six rows, the footer: nothing about a repo that could not be read is hidden,
        // and nothing about one is unfolded either.
        8,
        "{screen}"
    );
    assert!(stage.pty().alive(), "the popup died");
}
