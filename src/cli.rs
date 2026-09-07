//! The agent-facing surface: the same stack the popup drives, as JSON on stdout.
//!
//! An agent working in a repo has a shell and nothing else. It cannot read a TUI, so everything the
//! rows say — what the deps are, which are up, what ports they answer on, where their logs are — is
//! spelled out once here and printed whole. One command, no follow-up round trips.
//!
//! Two rules make it safe to call from a script:
//!
//! - **`status` never starts anything.** It dials the daemon and takes silence for an answer, because
//!   a daemon starting up restores what the last one was running — a read must not resurrect a stack.
//!   A verb starts a daemon exactly as the popup does.
//! - **Nothing is inferred from a cwd the caller did not name.** The project is the manifest at or
//!   above `--project`, defaulting to the working directory, and never Herdr's focused pane: an agent
//!   is not looking at anything.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::client::{Endpoint, Link, Target};
use crate::manifest::Project;
use crate::project::{self, MANIFEST_NAME};
use crate::store::{Identity, Store};
use crate::unit::{self, State, Status};
use crate::view::View;
use crate::{docker, ports, state};

/// Long enough to be sure nothing is listening, short enough that a stopped stack answers at once:
/// dialling a socket that exists either connects or fails immediately.
const NO_WAIT: Duration = Duration::ZERO;

/// What a caller named before the verb: where the manifest is, and — for tests and for driving a
/// scratch daemon by hand — which state root to reach it through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ask {
    pub project: Option<PathBuf>,
    pub state: Option<PathBuf>,
}

impl Ask {
    fn endpoint(&self) -> Endpoint {
        match &self.state {
            Some(root) => Endpoint::at(root),
            None => Endpoint::spelled_out(),
        }
    }

    fn store(&self) -> Store {
        Store::at(self.state.clone().unwrap_or_else(state::root))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Start,
    Stop,
    Restart,
}

impl Act {
    pub fn label(self) -> &'static str {
        match self {
            Act::Start => "start",
            Act::Stop => "stop",
            Act::Restart => "restart",
        }
    }
}

/// What every unit of the project is and how it is doing. Printed even when the daemon is down and
/// docker is unreachable: a manifest is still an answer to "what are this project's deps".
pub fn status(ask: &Ask) -> Result<Value, String> {
    let view = View::of(load(ask.project.clone())?);
    let mut problems = view.complaints();
    let mut link = dial(ask, &mut problems);

    let mut units = Vec::new();
    for (repo, project) in listed(&view, &mut problems) {
        let statuses = read(ask, project, link.as_mut(), &mut problems);
        units.extend(described(ask, project, repo, &statuses));
    }
    attach_local_ports(&mut units, &mut problems);

    Ok(json!({
        "project": named(view.focused()),
        "daemon": link.map(|link| json!({"version": link.peer().version, "pid": link.peer().pid})),
        "units": units,
        "problems": problems,
    }))
}

/// One verb against one or more units, each answered separately: a selector nothing matched is a
/// failure of that selector alone, and the units that did match still run. The `bool` is whether every
/// one of them took, which is what a caller's exit status is for — the document is worth printing
/// either way.
pub fn control(act: Act, ask: &Ask, selectors: &[String]) -> Result<(Value, bool), String> {
    if selectors.is_empty() {
        return Err(format!("{}: name at least one unit", act.label()));
    }
    let view = View::of(load(ask.project.clone())?);
    let mut problems = view.complaints();
    let listed = listed(&view, &mut problems);
    let mut link = ask.endpoint().open()?;
    if link.skewed() {
        return Err(link.footer());
    }

    let mut done = Vec::new();
    let mut refused = false;
    for selector in selectors {
        let outcome = match pick(&listed, selector) {
            Err(complaint) => Err(complaint),
            Ok(found) => act_on(&mut link, act, found.project, found.kind, &found.name),
        };
        refused |= outcome.is_err();
        done.push(match outcome {
            Ok(note) => json!({"unit": selector, "ok": true, "note": note}),
            Err(complaint) => json!({"unit": selector, "ok": false, "error": complaint}),
        });
    }

    Ok((
        json!({"action": act.label(), "results": done, "problems": problems}),
        !refused,
    ))
}

fn act_on(
    link: &mut Link,
    act: Act,
    project: &Project,
    kind: &str,
    name: &str,
) -> Result<Option<String>, String> {
    let target = Target::of(project, kind, name)?;
    match act {
        Act::Start => link.start(project, &target),
        Act::Stop => link.stop(project, &target),
        Act::Restart => link.restart(project, &target),
    }
}

fn load(root: Option<PathBuf>) -> Result<Project, String> {
    let start = match root {
        Some(dir) => dir,
        None => {
            std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?
        }
    };
    let manifest = project::nearest_manifest(&start)
        .ok_or_else(|| format!("no {MANIFEST_NAME} at or above {}", start.display()))?;
    Project::load(&manifest).map_err(|error| error.to_string())
}

/// A daemon that is not running is not a problem — it is the stack being down — but one that answers
/// in a protocol we cannot read is, and is then treated as absent rather than half-trusted.
fn dial(ask: &Ask, problems: &mut Vec<String>) -> Option<Link> {
    let link = ask.endpoint().connect_within(NO_WAIT).ok()?;
    match link.skewed() {
        true => {
            problems.push(link.footer());
            None
        }
        false => Some(link),
    }
}

/// The focused manifest, then every repo it includes under its own name. An include that could not be
/// read says so once and contributes no units.
fn listed<'a>(view: &'a View, problems: &mut Vec<String>) -> Vec<(Option<&'a str>, &'a Project)> {
    let mut listed = vec![(None, view.focused())];
    for included in view.included() {
        match &included.project {
            Ok(project) => listed.push((Some(included.name.as_str()), project)),
            Err(complaint) => problems.push(format!("{}: {complaint}", included.name)),
        }
    }
    listed
}

/// The daemon's answer when one is listening, and docker's own when none is: a compose service is
/// running or not regardless of who asks, while a local unit without its parent alive is down by
/// definition.
fn read(
    ask: &Ask,
    project: &Project,
    link: Option<&mut Link>,
    problems: &mut Vec<String>,
) -> BTreeMap<String, Status> {
    match link {
        Some(link) => match link.status(project) {
            Ok(statuses) => statuses,
            Err(complaint) => {
                problems.push(complaint);
                BTreeMap::new()
            }
        },
        None => {
            let services: Vec<docker::Service> = project
                .docker
                .iter()
                .map(|service| docker::Service {
                    name: service.name.clone(),
                    one_shot: service.one_shot,
                })
                .collect();
            docker::statuses(&ask.store(), &identity(project), &services)
        }
    }
}

fn described(
    ask: &Ask,
    project: &Project,
    repo: Option<&str>,
    statuses: &BTreeMap<String, Status>,
) -> Vec<Value> {
    let slot = ask.store().slot(&identity(project));
    let mut units = Vec::new();

    for service in &project.docker {
        let key = unit::key(unit::DOCKER, &service.name);
        let mut value = spelled(
            statuses.get(&key),
            &key,
            unit::DOCKER,
            &service.name,
            repo,
            project,
        );
        insert(&mut value, "one_shot", json!(service.one_shot));
        if let Some(note) = &service.note {
            // The manifest's standing hint, which the daemon's own note about the state would
            // otherwise overwrite: both are worth having, so they keep separate keys.
            insert(&mut value, "manifest_note", json!(note));
        }
        units.push(value);
    }

    for local in &project.local {
        let key = unit::key(unit::LOCAL, &local.name);
        let mut value = spelled(
            statuses.get(&key),
            &key,
            unit::LOCAL,
            &local.name,
            repo,
            project,
        );
        insert(&mut value, "cmd", json!(local.cmd));
        insert(&mut value, "cwd", json!(local.cwd));
        insert(&mut value, "tty", json!(local.tty));
        insert(&mut value, "log", json!(slot.log_path(&key)));
        // The pid is what a port reading is joined on later, and what makes the log a live one.
        if let Some(pid) = slot.record(&key).and_then(|record| record.pid) {
            insert(&mut value, "pid", json!(pid));
        }
        if let Some(problem) = &local.problem {
            insert(&mut value, "error", json!(problem));
        }
        units.push(value);
    }
    units
}

/// A unit nothing has ever run is `down` rather than absent: a manifest entry is a dep whether or not
/// the daemon has heard of it.
fn spelled(
    status: Option<&Status>,
    key: &str,
    kind: &str,
    name: &str,
    repo: Option<&str>,
    project: &Project,
) -> Value {
    let mut value = status
        .cloned()
        .unwrap_or_else(|| Status::of(State::Down))
        .to_value();
    // Always spelled out, empty or not: a caller reading ports has one shape to handle rather than
    // an absent key meaning the same as an empty list.
    if value.get("ports").is_none() {
        insert(&mut value, "ports", json!([]));
    }
    insert(&mut value, "unit", json!(key));
    insert(&mut value, "kind", json!(kind));
    insert(&mut value, "name", json!(name));
    insert(&mut value, "project", json!(project.root));
    if let Some(repo) = repo {
        insert(&mut value, "repo", json!(repo));
    }
    value
}

fn insert(value: &mut Value, key: &str, what: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert(key.to_string(), what);
    }
}

/// Ports are read once for the whole machine and only when something is up, because `lsof` costs more
/// than every other part of a status put together.
fn attach_local_ports(units: &mut [Value], problems: &mut Vec<String>) {
    let wanted: Vec<usize> = units
        .iter()
        .enumerate()
        .filter(|(_, unit)| unit.get("kind").and_then(Value::as_str) == Some(unit::LOCAL))
        .filter(|(_, unit)| unit.get("pid").is_some())
        .map(|(at, _)| at)
        .collect();
    if wanted.is_empty() {
        return;
    }
    let listeners = match ports::listening() {
        Ok(listeners) => listeners,
        Err(complaint) => {
            problems.push(complaint);
            return;
        }
    };
    for at in wanted {
        let Some(pid) = units[at].get("pid").and_then(Value::as_u64) else {
            continue;
        };
        let found: &BTreeSet<u16> = match listeners.get(&(pid as u32)) {
            Some(found) => found,
            None => continue,
        };
        insert(&mut units[at], "ports", json!(found));
    }
}

fn named(project: &Project) -> Value {
    json!({
        "name": project.name,
        "path": project.root,
        "manifest": project.manifest,
    })
}

fn identity(project: &Project) -> Identity {
    Identity {
        path: project.root.clone(),
        name: project.name.clone(),
    }
}

/// What a selector matched, and the manifest it has to be run against.
#[derive(Debug)]
struct Found<'a> {
    project: &'a Project,
    kind: &'static str,
    name: String,
}

/// `rails`, `local-rails` and `accounts:rails` all reach a unit; the qualified spellings exist for
/// when the bare name is ambiguous, which is the one case a caller cannot resolve on its own.
fn pick<'a>(
    listed: &[(Option<&'a str>, &'a Project)],
    selector: &str,
) -> Result<Found<'a>, String> {
    let (repo, unit) = match selector.split_once(':') {
        Some((repo, unit)) => (Some(repo), unit),
        None => (None, selector),
    };
    let found: Vec<Found<'a>> = listed
        .iter()
        .filter(|(name, _)| repo.is_none() || *name == repo)
        .flat_map(|(_, project)| {
            candidates(project).into_iter().map(|(kind, name)| Found {
                project,
                kind,
                name,
            })
        })
        .filter(|found| names(found, unit))
        .collect();

    match found.len() {
        1 => Ok(found.into_iter().next().expect("one match")),
        0 => Err(format!("no unit `{selector}` in this project")),
        _ => Err(format!(
            "`{selector}` is ambiguous: {}",
            found
                .iter()
                .map(|found| qualified(listed, found))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn candidates(project: &Project) -> Vec<(&'static str, String)> {
    let docker = project
        .docker
        .iter()
        .map(|service| (unit::DOCKER, service.name.clone()));
    let local = project
        .local
        .iter()
        .map(|local| (unit::LOCAL, local.name.clone()));
    docker.chain(local).collect()
}

fn names(found: &Found, unit: &str) -> bool {
    found.name == unit || unit::key(found.kind, &found.name) == unit
}

fn qualified(listed: &[(Option<&str>, &Project)], found: &Found) -> String {
    let key = unit::key(found.kind, &found.name);
    match listed
        .iter()
        .find(|(_, project)| std::ptr::eq(*project, found.project))
        .and_then(|(repo, _)| *repo)
    {
        Some(repo) => format!("{repo}:{key}"),
        None => key,
    }
}

/// Both modes print one JSON document and nothing else, so a caller may pipe stdout straight into a
/// parser and read stderr as the only prose.
pub fn print(said: &Value) {
    println!("{}", serde_json::to_string_pretty(said).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;

    fn project(text: &str) -> Project {
        Project::parse(text, Path::new("/repos/storefront/.herdr-dev.toml")).expect("manifest")
    }

    #[test]
    fn a_unit_the_daemon_never_heard_of_is_down_rather_than_missing() {
        let project = project("[local.rails]\ncmd = [\"rails\", \"s\"]\n");
        let units = described(&Ask::default(), &project, None, &BTreeMap::new());
        assert_eq!(units.len(), 1);
        assert_eq!(units[0]["state"], json!("down"));
        assert_eq!(units[0]["unit"], json!("local-rails"));
        assert_eq!(units[0]["cmd"], json!(["rails", "s"]));
    }

    #[test]
    fn a_unit_listening_on_nothing_still_carries_an_empty_port_list() {
        let project = project("[local.rails]\ncmd = [\"rails\"]\n");
        let units = described(&Ask::default(), &project, None, &BTreeMap::new());
        assert_eq!(units[0]["ports"], json!([]));
    }

    #[test]
    fn a_docker_units_ports_are_whatever_the_status_carried() {
        let project = project("[docker]\nnames = [\"db\"]\n");
        let mut status = Status::of(State::Up);
        status.ports = vec![5432];
        let statuses = BTreeMap::from([(unit::key(unit::DOCKER, "db"), status)]);
        let units = described(&Ask::default(), &project, None, &statuses);
        assert_eq!(units[0]["ports"], json!([5432]));
        assert_eq!(units[0]["one_shot"], json!(false));
    }

    #[test]
    fn an_included_repos_units_wear_its_name() {
        let project = project("[local.rails]\ncmd = [\"rails\"]\n");
        let units = described(
            &Ask::default(),
            &project,
            Some("accounts"),
            &BTreeMap::new(),
        );
        assert_eq!(units[0]["repo"], json!("accounts"));
    }

    #[test]
    fn a_selector_reaches_a_unit_by_name_by_key_or_by_repo() {
        let focused = project("[local.rails]\ncmd = [\"rails\"]\n[docker]\nnames = [\"db\"]\n");
        let included = Project::parse(
            "[local.rails]\ncmd = [\"rails\"]\n",
            Path::new("/repos/accounts/.herdr-dev.toml"),
        )
        .expect("manifest");
        let listed = vec![(None, &focused), (Some("accounts"), &included)];

        assert_eq!(pick(&listed, "db").expect("db").kind, unit::DOCKER);
        assert_eq!(
            pick(&listed, "accounts:rails")
                .expect("rails")
                .project
                .root,
            PathBuf::from("/repos/accounts")
        );
        assert_eq!(
            pick(&listed, "accounts:local-rails")
                .expect("rails")
                .name,
            "rails"
        );
    }

    #[test]
    fn a_name_two_manifests_share_is_refused_with_both_spellings() {
        let focused = project("[local.rails]\ncmd = [\"rails\"]\n");
        let included = Project::parse(
            "[local.rails]\ncmd = [\"rails\"]\n",
            Path::new("/repos/accounts/.herdr-dev.toml"),
        )
        .expect("manifest");
        let listed = vec![(None, &focused), (Some("accounts"), &included)];

        let complaint = pick(&listed, "rails").unwrap_err();
        assert!(complaint.contains("local-rails"), "{complaint}");
        assert!(
            complaint.contains("accounts:local-rails"),
            "{complaint}"
        );
    }

    #[test]
    fn a_local_and_a_docker_unit_of_one_name_are_told_apart_by_the_key() {
        let focused = project("[local.web]\ncmd = [\"rails\"]\n[docker]\nnames = [\"web\"]\n");
        let listed = vec![(None, &focused)];
        assert!(pick(&listed, "web").is_err());
        assert_eq!(pick(&listed, "local-web").expect("local").kind, unit::LOCAL);
        assert_eq!(
            pick(&listed, "docker-web").expect("docker").kind,
            unit::DOCKER
        );
    }

    #[test]
    fn a_selector_nothing_matches_names_itself() {
        let focused = project("[local.rails]\ncmd = [\"rails\"]\n");
        let complaint = pick(&[(None, &focused)], "sidekiq").unwrap_err();
        assert!(complaint.contains("sidekiq"), "{complaint}");
    }

    #[test]
    fn the_project_is_the_nearest_manifest_above_the_directory_named() {
        let root = std::env::temp_dir().join(format!("herdr-dev-cli-{}", std::process::id()));
        let deep = root.join("app/models");
        fs::create_dir_all(&deep).expect("dirs");
        fs::write(
            root.join(MANIFEST_NAME),
            "[local.rails]\ncmd = [\"rails\"]\n",
        )
        .expect("write");

        let found = load(Some(deep)).expect("project");
        assert_eq!(found.local.len(), 1);
        assert!(load(Some(std::env::temp_dir().join("nowhere"))).is_err());

        fs::remove_dir_all(&root).expect("cleanup");
    }
}
