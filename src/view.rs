//! What the popup is looking at: the project the key resolved to, plus the sibling repos its manifest
//! names — one collapsed row each, unfolded with `→` into that repo's own units.
//!
//! An included repo is a project in its own right. Its rows come from its own manifest — its own
//! commands, its own compose services, its own `hidden` and `one_shot` — and every verb, log and state
//! key carries that repo's own path, so a unit started from the including view is the same unit
//! from the repo's own view, and stopping it from either is one act on one thing.
//!
//! **One level.** An include inside an included manifest is never followed. That is what keeps
//! billing's 15 services from unfolding uninvited, and it is a floor rather than an oversight.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Instant;

use crate::manifest::{Include, Project};
use crate::project::MANIFEST_NAME;
use crate::rows::{self, COLLAPSED_GLYPH, EXPANDED_GLYPH, Row, contract_home};
use crate::unit::Status;

pub const NOT_A_REPO: &str = "→ unfolds an included repo";

/// Which manifest a row came from. Statuses are read per owner and only for what is on screen, so a
/// collapsed repo costs no `compose ps` and no records of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    Focused,
    Included(usize),
}

/// What the daemon last said, per manifest, and when it said it. A read costs a `compose ps`, so the
/// popup takes few of them and grows the uptimes itself in between: a clock that only moves when the
/// daemon is asked reads as a frozen one.
#[derive(Debug, Default)]
pub struct Statuses {
    heard: BTreeMap<Owner, Heard>,
}

#[derive(Debug)]
struct Heard {
    at: Instant,
    units: BTreeMap<String, Status>,
}

impl Statuses {
    pub fn new() -> Statuses {
        Statuses::default()
    }

    pub fn insert(&mut self, owner: Owner, units: BTreeMap<String, Status>) {
        self.heard.insert(
            owner,
            Heard {
                at: Instant::now(),
                units,
            },
        );
    }

    /// One manifest's statuses with every uptime carried forward to now. Only a unit the daemon timed
    /// has one to carry: a stopped or crashed unit shows how it ended, which does not age.
    fn aged(&self, owner: Owner) -> BTreeMap<String, Status> {
        let Some(heard) = self.heard.get(&owner) else {
            return BTreeMap::new();
        };
        let since = heard.at.elapsed();
        heard
            .units
            .iter()
            .map(|(unit, status)| {
                let mut status = status.clone();
                status.uptime = status.uptime.map(|uptime| uptime + since);
                (unit.clone(), status)
            })
            .collect()
    }
}

/// One `[includes.*]` entry, resolved: either the repo's own manifest or the reason there is nothing
/// to unfold. Both render a row — an include that cannot be read says so where it stands rather than
/// vanishing or taking the view down with it.
#[derive(Debug)]
pub struct Included {
    pub name: String,
    pub path: PathBuf,
    pub project: Result<Project, String>,
}

#[derive(Debug)]
pub struct View {
    focused: Project,
    included: Vec<Included>,
    expanded: BTreeSet<usize>,
}

impl View {
    /// Reads every included manifest once, at open. They are file reads and nothing else: no docker
    /// is asked anything until a repo is unfolded.
    pub fn of(focused: Project) -> View {
        let mut included: Vec<Included> = Vec::new();
        for include in &focused.includes {
            let project = read(include, &focused, &included);
            included.push(Included {
                name: include.name.clone(),
                path: include.path.clone(),
                project,
            });
        }
        View {
            focused,
            included,
            expanded: BTreeSet::new(),
        }
    }

    pub fn focused(&self) -> &Project {
        &self.focused
    }

    pub fn has_repos(&self) -> bool {
        !self.included.is_empty()
    }

    /// The manifest a row belongs to. `None` for a repo row whose manifest could not be read: there is
    /// no project to act on, which is also why no verb reaches such a row.
    pub fn project(&self, owner: Owner) -> Option<&Project> {
        match owner {
            Owner::Focused => Some(&self.focused),
            Owner::Included(index) => self.included.get(index)?.project.as_ref().ok(),
        }
    }

    /// Every include this view read, unfolded or not: a caller with no screen — the CLI — covers the
    /// whole of what the focused manifest names rather than what a cursor has reached.
    pub fn included(&self) -> &[Included] {
        &self.included
    }

    pub fn on_screen(&self) -> Vec<(Owner, &Project)> {
        let mut projects = vec![(Owner::Focused, &self.focused)];
        projects.extend(self.expanded.iter().filter_map(|index| {
            let owner = Owner::Included(*index);
            Some((owner, self.project(owner)?))
        }));
        projects
    }

    /// What the manifests on screen could not make sense of, the focused project's first and an
    /// unfolded repo's under its own name. Every one of these leaves the rest of the project usable, so
    /// the footer is the only place they are ever said.
    pub fn complaints(&self) -> Vec<String> {
        self.on_screen()
            .into_iter()
            .flat_map(|(owner, project)| {
                let repo = match owner {
                    Owner::Focused => None,
                    Owner::Included(index) => self
                        .included
                        .get(index)
                        .map(|included| included.name.clone()),
                };
                project
                    .problems
                    .iter()
                    .map(move |problem| match &repo {
                        None => problem.clone(),
                        Some(name) => format!("{name}: {problem}"),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn rows(&self, statuses: &Statuses) -> Vec<Row> {
        let mut rows = rows::unit_rows(
            &self.focused,
            &statuses.aged(Owner::Focused),
            Owner::Focused,
            false,
        );
        for (index, included) in self.included.iter().enumerate() {
            let owner = Owner::Included(index);
            let expanded = self.expanded.contains(&index);
            rows.push(repo_row(included, expanded, owner));
            if let (true, Ok(project)) = (expanded, &included.project) {
                rows.extend(rows::unit_rows(project, &statuses.aged(owner), owner, true));
            }
        }
        rows
    }

    /// `→` on the row under the cursor. `Err` is the reason nothing moved — a row that is not a repo,
    /// or a repo with no manifest to show.
    pub fn unfold(&mut self, row: &Row) -> Result<(), String> {
        let Owner::Included(index) = row.owner else {
            return Err(NOT_A_REPO.to_string());
        };
        if !row.repo {
            return Err(NOT_A_REPO.to_string());
        }
        match self.included.get(index).map(|included| &included.project) {
            None => Err(NOT_A_REPO.to_string()),
            Some(Err(why)) => Err(why.clone()),
            Some(Ok(_)) => {
                self.expanded.insert(index);
                Ok(())
            }
        }
    }

    /// `←` folds the repo a row belongs to, whether that row is the repo's own or one unfolded out of
    /// it.
    pub fn fold(&mut self, row: &Row) -> Result<(), String> {
        let Owner::Included(index) = row.owner else {
            return Err(NOT_A_REPO.to_string());
        };
        self.expanded.remove(&index);
        Ok(())
    }
}

fn repo_row(included: &Included, expanded: bool, owner: Owner) -> Row {
    Row {
        glyph: match expanded {
            true => EXPANDED_GLYPH,
            false => COLLAPSED_GLYPH,
        },
        name: included.name.clone(),
        // No kind and no state: a repo is not a unit, and the note is what is inside it.
        kind: "",
        state: String::new(),
        timing: String::new(),
        note: match &included.project {
            Ok(project) => summary(project),
            Err(why) => why.clone(),
        },
        owner,
        indent: false,
        repo: true,
    }
}

/// What a collapsed row says is inside, counted from the included manifest itself: the rows unfolding
/// it would produce, so `hidden` services are no more counted than they are shown.
fn summary(project: &Project) -> String {
    let mut inside = Vec::new();
    if !project.docker.is_empty() {
        inside.push(counted(project.docker.len(), "service", "services"));
    }
    if !project.local.is_empty() {
        inside.push(counted(project.local.len(), "process", "processes"));
    }
    let mut note = match inside.is_empty() {
        true => "nothing to run, from its own config".to_string(),
        false => format!("{}, from its own config", inside.join(" + ")),
    };
    // One level only, and a manifest that names repos of its own is where that shows.
    if !project.includes.is_empty() {
        note.push_str(&format!(
            "; its own {} not followed",
            counted(project.includes.len(), "include is", "includes are")
        ));
    }
    note
}

fn counted(count: usize, one: &str, many: &str) -> String {
    match count {
        1 => format!("1 {one}"),
        count => format!("{count} {many}"),
    }
}

/// The included repo's own manifest, or one line saying why there is none. Every one of these is a case
/// that happens: a path that was never there, a repo that has no manifest yet, a manifest mid-edit, and
/// a copied `[includes]` block naming the repo it sits in.
fn read(include: &Include, focused: &Project, earlier: &[Included]) -> Result<Project, String> {
    if include.path == focused.root {
        return Err("this project itself, so there is nothing to unfold".to_string());
    }
    if let Some(first) = earlier.iter().find(|already| already.path == include.path) {
        return Err(format!("the same repo as `{}` above", first.name));
    }
    let where_it_should_be = contract_home(&include.path);
    if !include.path.is_dir() {
        return Err(match include.path.exists() {
            true => format!("not a directory: {where_it_should_be}"),
            false => format!("no repo at {where_it_should_be}"),
        });
    }
    let manifest = include.path.join(MANIFEST_NAME);
    if !manifest.is_file() {
        return Err(format!("no {MANIFEST_NAME} in {where_it_should_be}"));
    }
    Project::load(&manifest).map_err(|error| format!("broken {MANIFEST_NAME}: {}", error.terse()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::time::Duration;

    use crate::unit::{self, Exit, State};

    #[test]
    fn an_uptime_grows_between_reads_and_an_exit_stays_what_it_was() {
        let mut running = Status::of(State::Up);
        running.uptime = Some(Duration::from_secs(41));
        let mut crashed = Status::of(State::Dead);
        crashed.exit = Some(Exit::Code(1));

        let mut statuses = Statuses::new();
        statuses.insert(
            Owner::Focused,
            BTreeMap::from([
                (unit::key(unit::LOCAL, "rails"), running),
                (unit::key(unit::LOCAL, "vite"), crashed),
            ]),
        );
        std::thread::sleep(Duration::from_millis(60));

        let aged = statuses.aged(Owner::Focused);
        assert!(
            aged["local-rails"].uptime.expect("an uptime") > Duration::from_secs(41),
            "the uptime stood still between reads"
        );
        assert_eq!(aged["local-vite"].uptime, None);
        assert_eq!(aged["local-vite"].timing(), "exit 1");
    }

    struct Repos(PathBuf);

    impl Repos {
        fn new(name: &str) -> Repos {
            let root = PathBuf::from("/tmp").join(format!("hd-view-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("a place for repos");
            Repos(root)
        }

        fn repo(&self, name: &str, manifest: &str) -> PathBuf {
            let dir = self.0.join(name);
            std::fs::create_dir_all(&dir).expect("a repo");
            std::fs::write(dir.join(MANIFEST_NAME), manifest).expect("a manifest");
            dir
        }

        fn bare(&self, name: &str) -> PathBuf {
            let dir = self.0.join(name);
            std::fs::create_dir_all(&dir).expect("a repo");
            dir
        }

        fn view(&self, name: &str, manifest: &str) -> View {
            let dir = self.repo(name, manifest);
            View::of(Project::load(dir.join(MANIFEST_NAME)).expect("the manifest parses"))
        }
    }

    impl Drop for Repos {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn note_of<'a>(rows: &'a [Row], name: &str) -> &'a str {
        rows.iter()
            .find(|row| row.name == name)
            .map(|row| row.note.as_str())
            .unwrap_or_else(|| panic!("no row named {name} in {rows:?}"))
    }

    #[test]
    fn a_collapsed_row_counts_what_is_inside_without_being_a_unit_itself() {
        let repos = Repos::new("collapsed");
        let included = repos.repo(
            "accounts",
            "[local.rails]\ncmd = [\"rails\", \"s\"]\n[local.vite]\ncmd = [\"bin/vite\"]\n\
             [docker]\nnames = [\"db\", \"redis\", \"minio\"]\nhidden = [\"minio\"]\n",
        );
        let view = repos.view(
            "storefront",
            &format!(
                "[local.rails]\ncmd = [\"rails\", \"s\"]\n[includes.accounts]\npath = \"{}\"\n",
                included.display()
            ),
        );
        let rows = view.rows(&Statuses::new());
        assert_eq!(rows.len(), 2, "{rows:?}");
        let repo = &rows[1];
        assert_eq!(repo.name, "accounts");
        assert_eq!(repo.glyph, COLLAPSED_GLYPH);
        assert!(repo.repo);
        assert_eq!(
            (repo.kind, repo.state.as_str(), repo.timing.as_str()),
            ("", "", "")
        );
        assert_eq!(repo.note, "2 services + 2 processes, from its own config");
    }

    #[test]
    fn unfolding_reads_the_included_repos_own_manifest_and_folding_it_puts_it_away_again() {
        let repos = Repos::new("unfold");
        let included = repos.repo(
            "accounts",
            "[local.sidekiq]\ncmd = [\"sidekiq\"]\n[docker]\nnames = [\"db\", \"web\"]\n\
             one_shot = [\"seed\"]\nhidden = [\"web\", \"seed\"]\n[docker.notes]\ndb = \"its own note\"\n",
        );
        let mut view = repos.view(
            "storefront",
            &format!(
                "[docker]\nnames = [\"db\"]\n[includes.accounts]\npath = \"{}\"\n",
                included.display()
            ),
        );
        let rows = view.rows(&Statuses::new());
        view.unfold(&rows[1]).expect("the repo unfolds");

        let rows = view.rows(&Statuses::new());
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["db", "accounts", "db", "sidekiq"],
        );
        assert_eq!(rows[1].glyph, EXPANDED_GLYPH);
        // The unfolded rows are the included repo's own, hidden services and all: `web` and `seed`
        // never appear, and the note is the one that repo's manifest carries.
        assert_eq!(rows[2].owner, Owner::Included(0));
        assert_eq!(rows[2].note, "its own note");
        assert!(rows[2].indent && rows[3].indent);
        assert_eq!(rows[0].owner, Owner::Focused);
        assert!(!rows[0].indent);

        view.fold(&rows[1]).expect("the repo folds again");
        assert_eq!(view.rows(&Statuses::new()).len(), 2);

        view.unfold(&rows[1]).expect("the repo unfolds");
        view.fold(&rows[3])
            .expect("a unit folds the repo it came out of");
        assert_eq!(view.rows(&Statuses::new()).len(), 2);
        assert_eq!(view.fold(&rows[0]).unwrap_err(), NOT_A_REPO);
    }

    #[test]
    fn an_include_inside_an_included_manifest_is_never_followed_and_the_row_says_so() {
        let repos = Repos::new("one-level");
        let deepest = repos.repo("billing", "[local.rails]\ncmd = [\"rails\", \"s\"]\n");
        let middle = repos.repo(
            "accounts",
            &format!(
                "[local.sidekiq]\ncmd = [\"sidekiq\"]\n[includes.billing]\npath = \"{}\"\n",
                deepest.display()
            ),
        );
        let mut view = repos.view(
            "storefront",
            &format!(
                "[includes.accounts]\npath = \"{}\"\n",
                middle.display()
            ),
        );
        let rows = view.rows(&Statuses::new());
        assert_eq!(
            rows[0].note,
            "1 process, from its own config; its own 1 include is not followed"
        );
        view.unfold(&rows[0]).expect("the repo unfolds");

        let rows = view.rows(&Statuses::new());
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["accounts", "sidekiq"],
            "billing is one level too deep to be here",
        );
    }

    #[test]
    fn a_verbs_project_is_the_one_that_owns_the_row_rather_than_the_one_being_looked_at() {
        let repos = Repos::new("owner");
        let included = repos.repo("accounts", "[local.rails]\ncmd = [\"rails\", \"s\"]\n");
        let mut view = repos.view(
            "storefront",
            &format!(
                "[local.rails]\ncmd = [\"rails\", \"s\"]\n[includes.accounts]\npath = \"{}\"\n",
                included.display()
            ),
        );
        let rows = view.rows(&Statuses::new());
        view.unfold(&rows[1]).expect("the repo unfolds");
        let rows = view.rows(&Statuses::new());

        assert_eq!(
            view.project(rows[0].owner).map(|p| &p.root),
            Some(&view.focused().root)
        );
        assert_eq!(
            view.project(rows[2].owner).map(|p| p.root.clone()),
            Some(included)
        );
        assert_eq!(rows[0].name, rows[2].name, "the same unit name in both");
    }

    #[test]
    fn only_what_is_on_screen_is_asked_about() {
        let repos = Repos::new("on-screen");
        let included = repos.repo("accounts", "[docker]\nnames = [\"db\"]\n");
        let mut view = repos.view(
            "storefront",
            &format!(
                "[includes.accounts]\npath = \"{}\"\n",
                included.display()
            ),
        );
        assert_eq!(
            view.on_screen().len(),
            1,
            "a collapsed repo is not read from"
        );
        let rows = view.rows(&Statuses::new());
        view.unfold(&rows[0]).expect("the repo unfolds");
        assert_eq!(view.on_screen().len(), 2);
    }

    #[test]
    fn the_unfolded_rows_wear_the_statuses_of_their_own_project() {
        let repos = Repos::new("statuses");
        let included = repos.repo("accounts", "[local.rails]\ncmd = [\"rails\", \"s\"]\n");
        let mut view = repos.view(
            "storefront",
            &format!(
                "[local.rails]\ncmd = [\"rails\", \"s\"]\n[includes.accounts]\npath = \"{}\"\n",
                included.display()
            ),
        );
        let rows = view.rows(&Statuses::new());
        view.unfold(&rows[1]).expect("the repo unfolds");

        let mut statuses = Statuses::new();
        statuses.insert(
            Owner::Included(0),
            BTreeMap::from([(unit::key(unit::LOCAL, "rails"), Status::of(State::Up))]),
        );
        let rows = view.rows(&statuses);
        assert_eq!(
            rows[0].state, "down",
            "the focused project's own row is untouched"
        );
        assert_eq!(rows[2].state, "up");
    }

    #[test]
    fn every_include_that_cannot_be_read_says_what_is_wrong_where_it_stands() {
        let repos = Repos::new("degraded");
        let storefront = repos.0.join("storefront");
        let bare = repos.bare("no_manifest");
        let broken = repos.repo("broken", "[local.rails\n");
        let twice = repos.repo("twice", "[local.rails]\ncmd = [\"rails\"]\n");
        let mut view = repos.view(
            "storefront",
            &format!(
                "[includes.missing]\npath = \"{}/nowhere\"\n\
                 [includes.no_manifest]\npath = \"{}\"\n\
                 [includes.broken]\npath = \"{}\"\n\
                 [includes.myself]\npath = \"{}\"\n\
                 [includes.twice]\npath = \"{}\"\n\
                 [includes.again]\npath = \"{}\"\n",
                repos.0.display(),
                bare.display(),
                broken.display(),
                storefront.display(),
                twice.display(),
                twice.display(),
            ),
        );
        let rows = view.rows(&Statuses::new());
        assert_eq!(rows.len(), 6, "{rows:?}");
        assert!(
            rows.iter()
                .all(|row| row.repo && row.glyph == COLLAPSED_GLYPH)
        );

        assert_eq!(
            note_of(&rows, "missing"),
            format!("no repo at {}/nowhere", repos.0.display())
        );
        assert_eq!(
            note_of(&rows, "no_manifest"),
            format!("no {MANIFEST_NAME} in {}", bare.display())
        );
        let broken_note = note_of(&rows, "broken");
        assert!(
            broken_note.starts_with("broken .herdr-stack.toml: TOML parse error"),
            "{broken_note}"
        );
        assert_eq!(broken_note.lines().count(), 1, "{broken_note}");
        assert_eq!(
            note_of(&rows, "myself"),
            "this project itself, so there is nothing to unfold"
        );
        assert_eq!(note_of(&rows, "twice"), "1 process, from its own config");
        assert_eq!(note_of(&rows, "again"), "the same repo as `twice` above");

        // Not one of the six unfolds, and none of them takes the view down with it.
        for row in &rows {
            let unfolded = view.unfold(row);
            if row.name == "twice" {
                assert!(unfolded.is_ok());
                continue;
            }
            assert_eq!(
                unfolded.as_ref().err().map(String::as_str),
                Some(row.note.as_str())
            );
        }
    }

    #[test]
    fn a_relative_or_doubled_back_include_path_is_the_same_repo_as_its_plain_spelling() {
        let repos = Repos::new("spelling");
        let included = repos.repo("accounts", "[local.rails]\ncmd = [\"rails\"]\n");
        let view = repos.view(
            "storefront",
            "[includes.plain]\npath = \"../accounts\"\n\
             [includes.doubled]\npath = \"../storefront/../accounts\"\n",
        );
        let rows = view.rows(&Statuses::new());
        assert_eq!(
            view.project(rows[0].owner).map(|p| p.root.clone()),
            Some(included)
        );
        assert_eq!(note_of(&rows, "doubled"), "the same repo as `plain` above");
    }

    /// A manifest is rendered anyway and says what it could not make sense of. An included repo's
    /// complaints are its own, and are only worth saying once it is unfolded.
    #[test]
    fn a_manifests_own_complaints_are_the_focused_ones_plus_an_unfolded_repos_under_its_name() {
        let repos = Repos::new("complaints");
        let included = repos.repo(
            "accounts",
            "[local.rails]\ncwd = \"/tmp\"\n[docker]\nnames = [\"db\"]\n",
        );
        let mut view = repos.view(
            "storefront",
            &format!(
                "[docker]\nnames = [\"db\"]\none_shot = [\"ghost\"]\n\
                 [includes.accounts]\npath = \"{}\"\n[includes.nameless]\nwrong = 1\n",
                included.display()
            ),
        );
        let folded = view.complaints();
        assert_eq!(folded.len(), 2, "{folded:?}");
        assert!(folded[0].contains("`ghost`"), "{folded:?}");
        assert!(folded[1].contains("[includes.nameless]"), "{folded:?}");

        let rows = view.rows(&Statuses::new());
        view.unfold(&rows[1]).expect("the repo unfolds");
        let unfolded = view.complaints();
        assert_eq!(unfolded.len(), 3, "{unfolded:?}");
        assert!(
            unfolded[2].starts_with("accounts: [local.rails]"),
            "{unfolded:?}"
        );
    }

    #[test]
    fn a_repo_row_is_the_only_thing_that_unfolds() {
        let repos = Repos::new("unfold");
        let mut view = repos.view("storefront", "[local.rails]\ncmd = [\"rails\"]\n");
        let rows = view.rows(&Statuses::new());
        assert_eq!(view.unfold(&rows[0]).unwrap_err(), NOT_A_REPO);
    }

    #[test]
    fn a_repo_with_nothing_in_its_manifest_says_that_rather_than_counting_to_zero() {
        let repos = Repos::new("empty");
        let included = repos.repo("accounts", "[env]\n");
        let view = repos.view(
            "storefront",
            &format!(
                "[includes.accounts]\npath = \"{}\"\n",
                included.display()
            ),
        );
        assert_eq!(
            view.rows(&Statuses::new())[0].note,
            "nothing to run, from its own config"
        );
    }

    #[test]
    fn the_real_storefront_manifest_stays_three_collapsed_rows_whatever_its_repos_hold() {
        let repos = Repos::new("storefront");
        let billing = repos.repo(
            "billing",
            &std::fs::read_to_string(example("billing")).expect("the example manifest"),
        );
        let view = repos.view(
            "storefront",
            &format!(
                "[docker]\nnames = [\"db\"]\n\
                 [includes.accounts]\npath = \"{0}/accounts\"\n\
                 [includes.billing]\npath = \"{1}\"\n\
                 [includes.backoffice]\npath = \"{0}/backoffice\"\n",
                repos.0.display(),
                billing.display(),
            ),
        );
        let rows = view.rows(&Statuses::new());
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert_eq!(
            note_of(&rows, "billing"),
            "9 services + 3 processes, from its own config",
            "the widest repo on the machine, folded into one line",
        );
    }

    fn example(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/manifests")
            .join(format!("{name}.herdr-stack.toml"))
    }
}
