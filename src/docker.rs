//! The docker half of the unit state model: what `compose ps --all` reports, and the three commands
//! that change it.
//!
//! Nothing here is owned. A compose service outlives the daemon, so every read goes to docker and no
//! record is ever consulted to decide whether a service is running — the records hold a display cache
//! only, for painting rows when docker cannot be reached at all.
//!
//! `--all` is not optional: measured, the default output omits exited containers entirely, so a
//! finished one-shot and a crash would both look like a service that was never created. `State`,
//! `Health` and `ExitCode` are the fields read. The prose `Status` is never parsed.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::store::{Cache, Identity, Store};
use crate::supervisor::Verdict;
use crate::unit::{self, Exit, State, Status};

/// Spelled out for the same reason mise's path is: a popup inherits no `PATH` worth trusting, and
/// this is the symlink Docker Desktop installs.
pub const DOCKER: &str = "/usr/local/bin/docker";

/// How long `--wait` may hold a start. Measured: a service whose healthcheck is still inside its
/// `start_period` held `up -d --wait` for 2m05s, which would wedge the daemon and outlast the
/// client's patience. A start is allowed to return with the service not yet up, so the wait is
/// bounded and `compose ps` says how it went.
const WAIT: Duration = Duration::from_secs(10);

const RUNNING: &str = "running";
const EXITED: &str = "exited";
const HEALTHY: &str = "healthy";
const STARTING: &str = "starting";
const UNHEALTHY: &str = "unhealthy";

const LOG_TAIL_LINES: &str = "500";

/// As long a reason as a note column can carry before it is more noise than help.
const REASON_WIDTH: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    pub one_shot: bool,
}

/// One container as `docker compose ps --all --format json` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub service: String,
    /// The container name, which is what `docker inspect` takes.
    pub name: String,
    pub state: String,
    pub health: String,
    pub exit_code: i32,
    /// Host ports, from `Publishers`. A container is reachable at these and at nothing else, which is
    /// why the target port inside it is dropped.
    pub ports: Vec<u16>,
}

impl Container {
    fn read(value: &Value) -> Option<Container> {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let service = text("Service");
        if service.is_empty() {
            return None;
        }
        Some(Container {
            service,
            name: text("Name"),
            state: text("State"),
            health: text("Health"),
            exit_code: value
                .get("ExitCode")
                .and_then(Value::as_i64)
                .unwrap_or_default() as i32,
            ports: published(value),
        })
    }

    fn published(&self) -> Vec<u16> {
        match self.running() {
            true => self.ports.clone(),
            // An exited container's publishers are still reported, and nothing answers on them.
            false => Vec::new(),
        }
    }

    fn running(&self) -> bool {
        self.state == RUNNING
    }
}

/// A publisher with `PublishedPort` 0 is a port compose knows about but has not bound to the host.
fn published(value: &Value) -> Vec<u16> {
    let mut ports: Vec<u16> = value
        .get("Publishers")
        .and_then(Value::as_array)
        .map(|publishers| {
            publishers
                .iter()
                .filter_map(|publisher| publisher.get("PublishedPort")?.as_u64())
                .filter(|port| *port != 0)
                .map(|port| port as u16)
                .collect()
        })
        .unwrap_or_default();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// Compose emits one object per line, and emitted a single array in older versions; a project with
/// nothing created at all emits neither.
pub fn parse(text: &str) -> Vec<Container> {
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) {
        return items.iter().filter_map(Container::read).collect();
    }
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| Container::read(&value))
        .collect()
}

/// Default file discovery only. Never `-f`, because the repos hold compose files
/// — `-services.yml`, `-ci.yml`, `.jmeter.yml` — that are no part of the dev flow.
fn compose(root: &Path) -> Command {
    let mut command = Command::new(DOCKER);
    command
        .arg("compose")
        .current_dir(root)
        .stdin(Stdio::null());
    command
}

pub fn ps_command(root: &Path) -> Command {
    let mut command = compose(root);
    command.args(["ps", "--all", "--format", "json"]);
    command
}

pub fn up_command(root: &Path, service: &Service) -> Command {
    let mut command = compose(root);
    command.args(["up", "-d"]);
    // An exit-0 service in the waited set makes `--wait` return exit 1 *and* abandon the wait early, so a one-shot is excluded rather than forgiven.
    if !service.one_shot {
        command
            .arg("--wait")
            .arg("--wait-timeout")
            .arg(WAIT.as_secs().to_string());
    }
    command.arg(&service.name);
    command
}

/// One service, never `down`: `down` is all-or-nothing and removes the network.
pub fn stop_command(root: &Path, service: &Service) -> Command {
    let mut command = compose(root);
    command.arg("stop").arg(&service.name);
    command
}

/// Streamed rather than polled, because a poll would refork the CLI several times a second and
/// re-read lines it has already shown. Colour is off at the source, and
/// `--tail` keeps a container that has been up for a week from arriving as a wall of history.
pub fn logs_command(root: &Path, service: &str) -> Command {
    let mut command = compose(root);
    command.args([
        "logs",
        "--no-color",
        "--no-log-prefix",
        "--follow",
        "--tail",
        LOG_TAIL_LINES,
        service,
    ]);
    command
}

pub fn inspect_command(names: &[&str]) -> Command {
    let mut command = Command::new(DOCKER);
    command
        .args(["inspect", "--format", "{{.Name}} {{.State.StartedAt}}"])
        .args(names)
        .stdin(Stdio::null());
    command
}

pub fn start(root: &Path, service: &Service) -> Result<Verdict, String> {
    run(up_command(root, service), &service.name)
}

pub fn stop(root: &Path, service: &Service) -> Result<Verdict, String> {
    run(stop_command(root, service), &service.name)
}

/// The same command as start, so there is one code path and a changed compose file takes effect. `compose restart` was rejected for silently reusing a stale container.
pub fn restart(root: &Path, service: &Service) -> Result<Verdict, String> {
    start(root, service)
}

/// Docker is read once for the whole list.
pub fn statuses(
    store: &Store,
    project: &Identity,
    services: &[Service],
) -> BTreeMap<String, Status> {
    if services.is_empty() {
        return BTreeMap::new();
    }
    // A project whose rows are docker-only still needs its identity on disk, or the cleanup rule
    // has nothing to read and the directory outlives the repo.
    let slot = store.open(project).unwrap_or_else(|_| store.slot(project));

    let observed = match ps(&project.path) {
        Err(reason) => {
            return services
                .iter()
                .map(|service| {
                    let key = unit::key(unit::DOCKER, &service.name);
                    let status = stale(slot.cache(&key).as_ref(), service.one_shot, &reason);
                    (key, status)
                })
                .collect();
        }
        Ok(containers) => containers,
    };

    let running: Vec<&str> = observed
        .iter()
        .filter(|container| container.running())
        .map(|container| container.name.as_str())
        .collect();
    let started = started_at(&running);

    services
        .iter()
        .map(|service| {
            let container = observed
                .iter()
                .find(|container| container.service == service.name);
            let mut status = observed_status(container, service.one_shot);
            status.uptime = container
                .filter(|container| container.running())
                .and_then(|container| started.get(&container.name).copied())
                .and_then(|started| SystemTime::now().duration_since(started).ok());
            let key = unit::key(unit::DOCKER, &service.name);
            let _ = slot.write_cache(&key, &cached(container.cloned(), status.uptime));
            (key, status)
        })
        .collect()
}

/// No observation yields `dead`: `compose stop` exits **137**, indistinguishable
/// from a kill or an OOM, so "you stopped it" and "it fell over" are one state.
pub fn observed_status(container: Option<&Container>, one_shot: bool) -> Status {
    let Some(container) = container else {
        return Status::of(State::Down);
    };
    let mut status = Status::of(State::Down);
    status.ports = container.published();
    match (container.state.as_str(), container.health.as_str()) {
        (RUNNING, UNHEALTHY) => {
            status.state = State::Up;
            status.note = Some(UNHEALTHY.to_string());
        }
        (RUNNING, STARTING) => status.state = State::Starting,
        (RUNNING, HEALTHY | "") => status.state = State::Up,
        // A healthcheck word we have not met is not a reason to guess at liveness: it is running.
        (RUNNING, other) => {
            status.state = State::Up;
            status.note = Some(other.to_string());
        }
        (EXITED, _) => {
            status.state = if one_shot { State::Done } else { State::Down };
            status.exit = Some(Exit::Code(container.exit_code));
        }
        // `created`, `paused`, `restarting`, `removing` and docker's own `dead`: not running, and the
        // docker word goes in the note rather than into a state column that has no room for it.
        (other, _) => status.note = Some(other.to_string()),
    }
    status
}

/// Docker could not be asked, so the row shows what was last seen and says how old it is. Never the
/// cached state in the state column: that would pretend a reading we do not have.
fn stale(cache: Option<&Cache>, one_shot: bool, reason: &str) -> Status {
    let mut status = Status::of(State::Unknown);
    // A service that was absent when last read has no reading to be stale about, and the last read is
    // recorded as exactly that, so the reason is all there is left to say.
    let cache = cache.filter(|cache| !cache.state.is_empty());
    status.note = Some(match cache {
        None => clip(reason),
        Some(cache) => {
            let last = observed_status(Some(&remembered(cache)), one_shot);
            let timing = match (cache.uptime, last.timing().as_str()) {
                (Some(uptime), _) => format!(" {}", unit::elapsed(uptime)),
                (None, "") => String::new(),
                (None, timing) => format!(" {timing}"),
            };
            format!(
                "stale: {}{timing}, seen {} ago",
                last.state.label(),
                unit::elapsed(cache.age())
            )
        }
    });
    status
}

/// The cache read back as the observation it was written from, so one mapping serves both.
fn remembered(cache: &Cache) -> Container {
    Container {
        service: String::new(),
        name: String::new(),
        state: cache.state.clone(),
        health: cache.health.clone(),
        exit_code: cache.exit_code,
        // The cache is a display fallback for a docker that stopped answering; a port nobody has
        // just confirmed is not worth remembering.
        ports: Vec::new(),
    }
}

fn cached(container: Option<Container>, uptime: Option<Duration>) -> Cache {
    let container = container.unwrap_or(Container {
        service: String::new(),
        name: String::new(),
        state: String::new(),
        health: String::new(),
        exit_code: 0,
        ports: Vec::new(),
    });
    Cache {
        state: container.state,
        health: container.health,
        exit_code: container.exit_code,
        seen_at: SystemTime::now(),
        uptime,
    }
}

/// `Err` is "docker could not tell us": an unreachable daemon, or a project root compose knows
/// nothing about. Both are `unknown` rather than a state invented from an absence.
pub fn ps(root: &Path) -> Result<Vec<Container>, String> {
    let output = ps_command(root)
        .output()
        .map_err(|error| format!("{DOCKER}: {error}"))?;
    if !output.status.success() {
        return Err(complaint(&output).unwrap_or_else(|| "docker is not answering".to_string()));
    }
    Ok(parse(&String::from_utf8_lossy(&output.stdout)))
}

/// Uptime from the container's start time. Never `RunningFor`, which measures from *creation* —
/// a db reporting "2 weeks ago" while up 24 hours.
fn started_at(names: &[&str]) -> BTreeMap<String, SystemTime> {
    if names.is_empty() {
        return BTreeMap::new();
    }
    let Ok(output) = inspect_command(names).output() else {
        return BTreeMap::new();
    };
    // A name that has gone since `compose ps` read makes inspect fail as a whole while still
    // reporting every other container, so the exit status is not consulted.
    parse_started(&String::from_utf8_lossy(&output.stdout))
}

fn parse_started(text: &str) -> BTreeMap<String, SystemTime> {
    text.lines()
        .filter_map(|line| {
            let (name, stamp) = line.trim().split_once(' ')?;
            Some((
                name.trim_start_matches('/').to_string(),
                instant(stamp.trim())?,
            ))
        })
        .collect()
}

/// `docker inspect` reports RFC 3339 in UTC — `2026-08-18T21:17:44.678150491Z`. The fraction is
/// dropped because the uptime column is whole seconds; anything not in this shape yields no uptime
/// rather than a wrong one.
fn instant(text: &str) -> Option<SystemTime> {
    let (date, clock) = text.strip_suffix('Z')?.split_once('T')?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    let mut clock = clock
        .split_once('.')
        .map_or(clock, |(whole, _)| whole)
        .split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(seconds).ok()?))
}

/// Hinnant's `days_from_civil`: the calendar without a calendar crate.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted = (month + 9) % 12;
    let day_of_year = (153 * shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn run(mut command: Command, service: &str) -> Result<Verdict, String> {
    let output = command
        .output()
        .map_err(|error| format!("{DOCKER}: {error}"))?;
    // `--wait`'s exit code is never the source of truth — an already-unhealthy service that is
    // plainly running exits 1 — so a refusal is a note on the row, and `compose ps` decides the state.
    if output.status.success() {
        return Ok(Verdict::done());
    }
    Ok(Verdict::note(
        complaint(&output).unwrap_or_else(|| format!("docker refused {service}")),
    ))
}

/// Compose narrates its progress on stderr and puts what went wrong last.
fn complaint(output: &std::process::Output) -> Option<String> {
    [&output.stderr, &output.stdout]
        .into_iter()
        .filter_map(|stream| {
            String::from_utf8_lossy(stream)
                .lines()
                .rfind(|line| !line.trim().is_empty())
                .map(|line| clip(line.trim()))
        })
        .find(|line| !line.is_empty())
}

fn clip(reason: &str) -> String {
    match reason.char_indices().nth(REASON_WIDTH) {
        None => reason.to_string(),
        Some((cut, _)) => format!("{}…", &reason[..cut]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded from a throwaway compose project, one line per awkward case; the whole `Labels` and
    /// `Status` prose is kept exactly as compose wrote it, including the fields we refuse to read.
    const RECORDED: &str = include_str!("../tests/fixtures/compose-ps-all.json");

    fn service(name: &str, one_shot: bool) -> Service {
        Service {
            name: name.to_string(),
            one_shot,
        }
    }

    fn observed(service: &str, one_shot: bool) -> Status {
        let containers = parse(RECORDED);
        let found = containers
            .iter()
            .find(|container| container.service == service);
        observed_status(found, one_shot)
    }

    fn argv(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn every_recorded_container_is_read_by_state_health_and_code_and_never_by_the_prose() {
        let containers = parse(RECORDED);
        let read: Vec<(&str, &str, &str, i32)> = containers
            .iter()
            .map(|container| {
                (
                    container.service.as_str(),
                    container.state.as_str(),
                    container.health.as_str(),
                    container.exit_code,
                )
            })
            .collect();
        assert_eq!(
            read,
            vec![
                ("boom", "exited", "", 7),
                ("checked", "running", "healthy", 0),
                ("never", "created", "", 0),
                ("oneshot", "exited", "", 0),
                ("plain", "running", "", 0),
                ("published", "running", "", 0),
                ("sick", "running", "unhealthy", 0),
                ("slow", "running", "starting", 0),
            ]
        );
        assert!(
            RECORDED.contains("\"Status\":\"Up About a minute (health: starting)\""),
            "the fixture must still carry the prose this code refuses to parse"
        );
    }

    #[test]
    fn a_published_port_is_read_once_however_many_addresses_it_is_bound_on() {
        let status = observed("published", false);
        assert_eq!(status.ports, vec![54999]);
        assert!(observed("plain", false).ports.is_empty());
    }

    #[test]
    fn a_service_that_is_not_running_publishes_nothing_whatever_compose_still_reports() {
        let stopped = Container {
            service: "published".into(),
            name: "probe-published-1".into(),
            state: EXITED.into(),
            health: String::new(),
            exit_code: 0,
            ports: vec![54999],
        };
        assert!(observed_status(Some(&stopped), false).ports.is_empty());
    }

    #[test]
    fn a_running_service_is_up_whether_it_has_a_healthcheck_or_passes_one() {
        for name in ["plain", "checked"] {
            let status = observed(name, false);
            assert_eq!(status.state, State::Up, "{name}");
            assert_eq!(status.note, None, "{name}");
        }
    }

    #[test]
    fn a_running_service_failing_its_healthcheck_is_up_with_unhealthy_in_the_note() {
        let status = observed("sick", false);
        assert_eq!(status.state, State::Up);
        assert_eq!(status.note.as_deref(), Some("unhealthy"));
    }

    #[test]
    fn a_healthcheck_still_inside_its_start_period_is_starting() {
        assert_eq!(observed("slow", false).state, State::Starting);
    }

    #[test]
    fn an_exited_one_shot_is_done_and_the_same_container_undeclared_is_down() {
        let done = observed("oneshot", true);
        assert_eq!(done.state, State::Done);
        assert_eq!(done.timing(), "exit 0");

        let down = observed("oneshot", false);
        assert_eq!(down.state, State::Down);
        assert_eq!(down.timing(), "exit 0");
    }

    #[test]
    fn an_exited_service_shows_the_code_it_exited_with() {
        let status = observed("boom", false);
        assert_eq!(status.state, State::Down);
        assert_eq!(status.timing(), "exit 7");
    }

    #[test]
    fn a_service_absent_from_the_output_is_down_with_nothing_to_show() {
        let status = observed("ghost", false);
        assert_eq!(status.state, State::Down);
        assert_eq!(status.timing(), "");
        assert_eq!(status.note, None);
    }

    #[test]
    fn a_created_container_is_down_and_says_which_docker_word_it_is_in() {
        let status = observed("never", false);
        assert_eq!(status.state, State::Down);
        assert_eq!(status.note.as_deref(), Some("created"));
    }

    #[test]
    fn a_service_we_stopped_reads_as_down_and_never_as_dead() {
        // Recorded: `compose stop` leaves exit 137, which no state may read as a crash.
        let stopped = include_str!("../tests/fixtures/compose-ps-stopped.json");
        let containers = parse(stopped);
        let status = observed_status(containers.first(), false);
        assert_eq!(status.state, State::Down);
        assert_eq!(status.timing(), "exit 137");
        for one_shot in [true, false] {
            for container in parse(RECORDED).iter().chain(containers.iter()) {
                assert_ne!(
                    observed_status(Some(container), one_shot).state,
                    State::Dead,
                    "{container:?}"
                );
            }
        }
    }

    #[test]
    fn a_project_with_nothing_created_yields_no_containers_rather_than_a_complaint() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n").is_empty());
        assert!(parse("not json at all").is_empty());
    }

    #[test]
    fn a_line_that_is_not_a_container_is_skipped_rather_than_costing_the_rows_around_it() {
        // Compose narrates on stderr — an override file still declaring `version` earns a warning
        // there on every read — and only stdout is parsed, but a stray line must still be survivable.
        let noisy = format!(
            "time=\"2026-08-18T23:39:38+02:00\" level=warning msg=\"obsolete\"\n{RECORDED}"
        );
        assert_eq!(parse(&noisy), parse(RECORDED));
        assert!(!parse(RECORDED).is_empty());
    }

    #[test]
    fn the_array_shape_older_compose_emitted_reads_the_same_as_one_object_per_line() {
        let lines = "{\"Service\":\"db\",\"State\":\"running\",\"Health\":\"healthy\",\"Name\":\"p-db-1\",\"ExitCode\":0}\n\
                     {\"Service\":\"web\",\"State\":\"exited\",\"Health\":\"\",\"Name\":\"p-web-1\",\"ExitCode\":1}";
        let array = format!("[{}]", lines.replace('\n', ","));
        assert_eq!(parse(lines), parse(&array));
        assert_eq!(parse(lines).len(), 2);
    }

    #[test]
    fn a_start_waits_for_readiness_and_a_one_shot_start_never_does() {
        let root = Path::new("/repos/storefront");
        let waited = argv(&up_command(root, &service("db", false)));
        assert_eq!(
            waited,
            [
                "compose",
                "up",
                "-d",
                "--wait",
                "--wait-timeout",
                "10",
                "db"
            ]
        );
        assert_eq!(
            argv(&up_command(root, &service("minio_init", true))),
            ["compose", "up", "-d", "minio_init"]
        );
    }

    #[test]
    fn a_restart_is_the_same_command_as_a_start_so_a_changed_compose_file_takes_effect() {
        let root = Path::new("/repos/storefront");
        let service = service("db", false);
        // restart() *is* start(), so the argv is the same by construction; this is the guard against
        // that quietly becoming `compose restart`, which reuses a stale container.
        assert_eq!(
            argv(&up_command(root, &service)),
            argv(&up_command(root, &service))
        );
        assert!(!argv(&up_command(root, &service)).contains(&"restart".to_string()));
    }

    #[test]
    fn a_stop_names_one_service_and_never_brings_the_project_down() {
        let argv = argv(&stop_command(
            Path::new("/repos/storefront"),
            &service("db", false),
        ));
        assert_eq!(argv, ["compose", "stop", "db"]);
        assert!(!argv.contains(&"down".to_string()));
    }

    #[test]
    fn a_log_peek_streams_one_service_without_colour() {
        let argv = argv(&logs_command(Path::new("/repos/storefront"), "db"));
        assert_eq!(
            argv,
            [
                "compose",
                "logs",
                "--no-color",
                "--no-log-prefix",
                "--follow",
                "--tail",
                LOG_TAIL_LINES,
                "db"
            ]
        );
    }

    #[test]
    fn every_command_runs_from_the_project_root_with_no_compose_file_named() {
        let root = Path::new("/repos/storefront");
        let service = service("db", false);
        for command in [
            ps_command(root),
            up_command(root, &service),
            stop_command(root, &service),
            logs_command(root, &service.name),
        ] {
            assert_eq!(command.get_current_dir(), Some(root));
            assert_eq!(command.get_program(), DOCKER);
            let argv = argv(&command);
            assert!(
                !argv.iter().any(|arg| arg == "-f" || arg == "--file"),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn a_read_asks_for_stopped_containers_too_because_a_finished_one_shot_is_one() {
        assert!(argv(&ps_command(Path::new("/repos/storefront"))).contains(&"--all".to_string()));
    }

    #[test]
    fn uptime_comes_from_the_start_time_docker_inspect_reports() {
        let recorded = "/herdrstack05probe-plain-1 2026-08-18T21:17:44.678150491Z\n\
                        /herdrstack05probe-checked-1 2026-08-18T21:17:44.644433581Z\n";
        let started = parse_started(recorded);
        assert_eq!(started.len(), 2);
        let plain = started["herdrstack05probe-plain-1"];
        assert_eq!(
            plain.duration_since(UNIX_EPOCH).expect("after the epoch"),
            Duration::from_secs(1_787_087_864)
        );
        assert!(started["herdrstack05probe-checked-1"] <= plain);
        assert!(
            argv(&inspect_command(&["p-db-1"]))
                .contains(&"{{.Name}} {{.State.StartedAt}}".to_string())
        );
    }

    #[test]
    fn a_start_time_in_any_other_shape_yields_no_uptime_rather_than_a_wrong_one() {
        assert!(instant("2026-08-18T21:17:44Z").is_some());
        assert!(instant("0001-01-01T00:00:00Z").is_none());
        assert!(instant("2026-08-18 21:17:44 +0200 CEST").is_none());
        assert!(instant("").is_none());
        assert!(parse_started("2 weeks ago").is_empty());
    }

    #[test]
    fn a_docker_that_cannot_be_reached_shows_unknown_and_says_why() {
        let reason = "failed to connect to the docker API at unix:///var/run/docker.sock";
        let status = stale(None, false, reason);
        assert_eq!(status.state, State::Unknown);
        assert!(
            status
                .note
                .as_deref()
                .is_some_and(|note| note.starts_with("failed to connect")),
            "{status:?}"
        );
        assert_eq!(status.uptime, None);
    }

    #[test]
    fn a_cached_row_is_shown_marked_stale_rather_than_pretended_live() {
        let cache = Cache {
            state: RUNNING.to_string(),
            health: HEALTHY.to_string(),
            exit_code: 0,
            seen_at: SystemTime::now() - Duration::from_secs(150),
            uptime: Some(Duration::from_secs(2460)),
        };
        let status = stale(Some(&cache), false, "docker is not answering");
        assert_eq!(status.state, State::Unknown);
        assert_eq!(status.uptime, None, "a cached uptime must not tick on");
        assert_eq!(status.timing(), "");
        assert_eq!(
            status.note.as_deref(),
            Some("stale: up 41m00s, seen 2m30s ago")
        );
    }

    #[test]
    fn a_cached_one_shot_stays_done_while_docker_is_away() {
        let cache = Cache {
            state: EXITED.to_string(),
            health: String::new(),
            exit_code: 0,
            seen_at: SystemTime::now() - Duration::from_secs(30),
            uptime: None,
        };
        let note = stale(Some(&cache), true, "docker is not answering")
            .note
            .expect("a stale note");
        assert!(
            note.starts_with("stale: done exit 0, seen 30s ago"),
            "{note}"
        );
    }

    #[test]
    fn a_service_absent_when_last_read_has_nothing_to_be_stale_about() {
        let cache = Cache {
            state: String::new(),
            health: String::new(),
            exit_code: 0,
            seen_at: SystemTime::now(),
            uptime: None,
        };
        let note = stale(Some(&cache), false, "docker is not answering")
            .note
            .expect("a note");
        assert_eq!(note, "docker is not answering");
    }

    #[test]
    fn a_reason_longer_than_the_note_column_is_clipped_rather_than_left_to_run() {
        let long = "x".repeat(200);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), REASON_WIDTH + 1);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip("short"), "short");
    }
}
