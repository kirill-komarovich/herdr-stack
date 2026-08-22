//! One row per unit, six columns wide: state glyph, name, kind, state label, uptime-or-exit, note.
//!
//! The last three columns are the daemon's answer, keyed by unit key. A unit the daemon has never
//! heard of is `down` if it is ours to run and blank if it is docker's, whose liveness is read live
//! from compose rather than from anything here.

use std::collections::BTreeMap;
use std::path::Path;

use crate::manifest::Project;
use crate::unit::{self, State, Status};
use crate::view::Owner;

pub const UNKNOWN_GLYPH: &str = "·";
pub const COLLAPSED_GLYPH: &str = "▸";
pub const EXPANDED_GLYPH: &str = "▾";

pub const EMPTY_HEADLINE: &str = "No manifest here";
pub const EMPTY_BODY: &str = "Nothing above the focused pane holds a .herdr-dev.toml.";
pub const EMPTY_HINT: &str = "Press g to split a pane, start an agent and have it write one.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub glyph: &'static str,
    pub name: String,
    pub kind: &'static str,
    pub state: String,
    pub timing: String,
    pub note: String,
    /// The manifest this row came from, so a verb, a log and a state key all reach the project that
    /// owns the unit rather than the one being looked at.
    pub owner: Owner,
    /// Set on the units of an expanded repo: they belong to something one level in.
    pub indent: bool,
    /// A collapsed repo stands for a whole manifest of its own, so no verb reaches it.
    pub repo: bool,
}

/// The rows one manifest contributes: its own services and its own processes, and never its includes —
/// a repo row is the including view's business, and an include inside an included manifest is not
/// followed at all.
pub fn unit_rows(
    project: &Project,
    statuses: &BTreeMap<String, Status>,
    owner: Owner,
    indent: bool,
) -> Vec<Row> {
    let docker = project.docker.iter().map(|service| {
        let status = statuses.get(&unit::key(unit::DOCKER, &service.name));
        let mut row = match status {
            Some(status) => paint(&service.name, unit::DOCKER, status, owner, indent),
            None => Row {
                glyph: UNKNOWN_GLYPH,
                name: service.name.clone(),
                kind: unit::DOCKER,
                state: String::new(),
                timing: String::new(),
                note: String::new(),
                owner,
                indent,
                repo: false,
            },
        };
        // What the state itself has to say wins: `unhealthy` on a service that is up all the same, or
        // how old a row is once docker stopped answering, outranks a standing hint from the manifest.
        row.note = status
            .and_then(|status| status.note.clone())
            .or_else(|| service.note.clone())
            .unwrap_or_default();
        row
    });
    let local = project.local.iter().map(|unit| {
        let status = statuses
            .get(&unit::key(unit::LOCAL, &unit.name))
            .cloned()
            .unwrap_or_else(|| Status::of(State::Down));
        let mut row = paint(&unit.name, unit::LOCAL, &status, owner, indent);
        // A unit that cannot be run at all says so; a claim only matters for one that can.
        row.note = match (&unit.problem, &status.held) {
            (Some(problem), _) => problem.clone(),
            (None, Some(claim)) => claim.label(),
            (None, None) => String::new(),
        };
        row
    });
    docker.chain(local).collect()
}

fn paint(name: &str, kind: &'static str, status: &Status, owner: Owner, indent: bool) -> Row {
    Row {
        glyph: status.state.glyph(),
        name: name.to_string(),
        kind,
        state: status.state.label().to_string(),
        timing: status.timing(),
        note: String::new(),
        owner,
        indent,
        repo: false,
    }
}

pub fn title(project: &Project) -> String {
    contract_home(&project.root)
}

pub fn contract_home(path: &Path) -> String {
    let home = std::env::var_os("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Ok(rest) if !home.is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::unit::{Claim, Exit};

    fn project(text: &str) -> Project {
        Project::parse(text, Path::new("/repos/storefront/.herdr-dev.toml")).expect("manifest")
    }

    fn nothing() -> BTreeMap<String, Status> {
        BTreeMap::new()
    }

    fn rows(project: &Project, statuses: &BTreeMap<String, Status>) -> Vec<Row> {
        unit_rows(project, statuses, Owner::Focused, false)
    }

    #[test]
    fn every_unit_gets_a_row_with_all_six_columns_filled_or_blank() {
        let rows = rows(
            &project(
                "[local.rails]\ncmd = [\"rails\", \"s\"]\n\
                 [docker]\nnames = [\"db\"]\n[docker.notes]\ndb = \"needs migrate\"\n\
                 [includes.accounts]\npath = \"/repos/accounts\"\n",
            ),
            &nothing(),
        );
        assert_eq!(rows.len(), 2, "an include is not a unit of this manifest");
        assert_eq!(
            (
                rows[0].name.as_str(),
                rows[0].kind,
                rows[0].note.as_str(),
                rows[0].glyph
            ),
            ("db", "docker", "needs migrate", UNKNOWN_GLYPH)
        );
        assert_eq!((rows[1].name.as_str(), rows[1].kind), ("rails", "local"));
        assert!(rows.iter().all(|row| !row.repo && !row.indent));
    }

    #[test]
    fn a_local_unit_the_daemon_has_never_run_is_down_while_a_docker_row_stays_blank() {
        let rows = rows(
            &project("[local.rails]\ncmd = [\"rails\", \"s\"]\n[docker]\nnames = [\"db\"]\n"),
            &nothing(),
        );
        assert_eq!((rows[0].state.as_str(), rows[0].timing.as_str()), ("", ""));
        assert_eq!(rows[1].state, "down");
        assert_eq!(rows[1].timing, "");
    }

    #[test]
    fn a_running_unit_shows_its_uptime_and_a_dead_one_what_it_exited_with() {
        let statuses = BTreeMap::from([
            (
                unit::key(unit::LOCAL, "rails"),
                Status {
                    state: State::Up,
                    uptime: Some(Duration::from_secs(724)),
                    exit: None,
                    held: None,
                    note: None,
                },
            ),
            (
                unit::key(unit::LOCAL, "vite"),
                Status {
                    state: State::Dead,
                    uptime: None,
                    exit: Some(Exit::Code(1)),
                    held: None,
                    note: None,
                },
            ),
        ]);
        let rows = rows(
            &project(
                "[local.rails]\ncmd = [\"rails\", \"s\"]\n[local.vite]\ncmd = [\"bin/vite\"]\n",
            ),
            &statuses,
        );
        assert_eq!(
            (
                rows[0].state.as_str(),
                rows[0].timing.as_str(),
                rows[0].glyph
            ),
            ("up", "12m04s", State::Up.glyph())
        );
        assert_eq!(
            (
                rows[1].state.as_str(),
                rows[1].timing.as_str(),
                rows[1].glyph
            ),
            ("dead", "exit 1", State::Dead.glyph())
        );
    }

    #[test]
    fn a_unit_another_project_holds_says_so_in_the_note_column() {
        let statuses = BTreeMap::from([(
            unit::key(unit::LOCAL, "vite"),
            Status {
                state: State::Down,
                uptime: None,
                exit: None,
                held: Some(Claim {
                    project: "storefront-wt2".into(),
                    pid: 51234,
                }),
                note: None,
            },
        )]);
        let rows = rows(&project("[local.vite]\ncmd = [\"bin/vite\"]\n"), &statuses);
        assert_eq!(rows[0].note, "held by storefront-wt2, pid 51234");
    }

    #[test]
    fn a_docker_row_wears_what_compose_said_and_its_own_note_outranks_the_manifests() {
        let statuses = BTreeMap::from([
            (
                unit::key(unit::DOCKER, "db"),
                Status {
                    state: State::Up,
                    uptime: Some(Duration::from_secs(2460)),
                    exit: None,
                    held: None,
                    note: Some("unhealthy".into()),
                },
            ),
            (
                unit::key(unit::DOCKER, "minio_init"),
                Status {
                    state: State::Done,
                    uptime: None,
                    exit: Some(Exit::Code(0)),
                    held: None,
                    note: None,
                },
            ),
        ]);
        let rows = rows(
            &project(
                "[docker]\nnames = [\"db\", \"minio_init\"]\none_shot = [\"minio_init\"]\n                 [docker.notes]\ndb = \"needs migrate\"\nminio_init = \"seeds the bucket\"\n",
            ),
            &statuses,
        );
        assert_eq!(
            (
                rows[0].state.as_str(),
                rows[0].timing.as_str(),
                rows[0].note.as_str()
            ),
            ("up", "41m00s", "unhealthy")
        );
        assert_eq!(
            (
                rows[1].state.as_str(),
                rows[1].timing.as_str(),
                rows[1].note.as_str()
            ),
            ("done", "exit 0", "seeds the bucket")
        );
    }

    #[test]
    fn a_docker_row_nobody_could_read_shows_unknown_and_how_old_the_last_reading_is() {
        let statuses = BTreeMap::from([(
            unit::key(unit::DOCKER, "db"),
            Status {
                state: State::Unknown,
                uptime: None,
                exit: None,
                held: None,
                note: Some("stale: up 41m00s, seen 2m30s ago".into()),
            },
        )]);
        let rows = rows(
            &project("[docker]\nnames = [\"db\"]\n[docker.notes]\ndb = \"needs migrate\"\n"),
            &statuses,
        );
        assert_eq!(rows[0].state, "unknown");
        assert_eq!(rows[0].glyph, State::Unknown.glyph());
        assert_eq!(
            rows[0].timing, "",
            "a stale row never pretends to an uptime"
        );
        assert_eq!(rows[0].note, "stale: up 41m00s, seen 2m30s ago");
    }

    #[test]
    fn a_broken_unit_carries_its_complaint_in_the_note_column() {
        let rows = rows(&project("[local.rails]\ncwd = \"/tmp\"\n"), &nothing());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].note.contains("cmd"), "{:?}", rows[0].note);
    }

    #[test]
    fn a_hidden_service_has_no_row() {
        let rows = rows(
            &project("[docker]\nnames = [\"db\"]\nhidden = [\"caddy\"]\n"),
            &nothing(),
        );
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn the_title_is_the_project_root_with_home_contracted() {
        let home = std::env::var("HOME").expect("HOME");
        let manifest = format!("{home}/projects/acme/storefront/.herdr-dev.toml");
        let project = Project::parse("", Path::new(&manifest)).expect("manifest");
        assert_eq!(title(&project), "~/projects/acme/storefront");
    }
}
