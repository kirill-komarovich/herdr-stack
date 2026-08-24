//! What the daemon owns: every local unit it has forked, and the records of the ones it has buried.
//!
//! Liveness is the `wait()` of a thread per unit, never a poll and never a record: the daemon is the
//! parent, so it is told when a child dies and learns the status. That is also why adoption is
//! rejected — a unit inherited from a dead daemon has an exit code nobody can ever read.
//!
//! Because macOS orphans children rather than killing them, *daemon alive ⇔ stack alive* is upheld
//! from both ends: `kill_all` on the way out, `kill_leftovers` on the way in.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::local::{self, Spec};
use crate::store::{Identity, Record, Slot, Store};
use crate::unit::{self, Exit, State, Status};

/// §10: `SIGTERM` to the group, `SIGKILL` five seconds later.
pub const GRACE: Duration = Duration::from_secs(5);

/// `SIGKILL` cannot be ignored, so this is scheduling slack rather than a grace period.
const REAP_WAIT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);

/// The answer to a verb: done, or done with something the footer should say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub note: Option<String>,
}

impl Verdict {
    pub fn done() -> Verdict {
        Verdict { note: None }
    }

    pub fn note(note: impl Into<String>) -> Verdict {
        Verdict {
            note: Some(note.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Handle {
    project: String,
    unit: String,
}

/// A unit this daemon forked and has not yet reaped.
#[derive(Debug)]
struct Live {
    pid: u32,
    started_at: SystemTime,
    /// Set before the group is signalled, so the reaping thread knows a `down` from a `dead`.
    stopping: bool,
}

/// How far a halt had to go.
enum Halt {
    NotRunning,
    Stopped,
    Escalated,
}

pub struct Supervisor {
    store: Store,
    grace: Duration,
    live: Mutex<HashMap<Handle, Live>>,
}

impl Supervisor {
    pub fn new(root: &Path, grace: Duration) -> Arc<Supervisor> {
        Arc::new(Supervisor {
            store: Store::at(root),
            grace,
            live: Mutex::new(HashMap::new()),
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Ownership in §7's sense: a daemon with a unit running has something to be the parent of.
    pub fn busy(&self) -> bool {
        !self.live.lock().expect("live units").is_empty()
    }

    pub fn start(self: &Arc<Self>, project: &Identity, spec: &Spec) -> Result<Verdict, String> {
        let handle = self.handle(project, &spec.name);
        if let Some(pid) = self.running(&handle) {
            return Ok(Verdict::note(format!(
                "{} is already running, pid {pid}",
                spec.name
            )));
        }

        let slot = self
            .store
            .open(project)
            .map_err(|error| format!("{}: {error}", self.store.root().display()))?;
        let log = slot
            .open_log(&handle.unit)
            .map_err(|error| format!("{}: {error}", slot.log_path(&handle.unit).display()))?;
        let spawned = local::spawn(spec, log)
            .map_err(|error| format!("cannot start {}: {error}", spec.name))?;

        slot.write(
            &handle.unit,
            &Record {
                state: State::Up,
                pid: Some(spawned.pid),
                started_at: Some(spawned.started_at),
                ps_start: spawned.ps_start.clone(),
                cmd: spec.cmd.clone(),
                cwd: spec.cwd.clone(),
                exit: None,
            },
        )
        .map_err(|error| format!("{}: {error}", slot.record_path(&handle.unit).display()))?;

        let mut child = spawned.child;
        self.live.lock().expect("live units").insert(
            handle.clone(),
            Live {
                pid: spawned.pid,
                started_at: spawned.started_at,
                stopping: false,
            },
        );
        let supervisor = Arc::clone(self);
        std::thread::spawn(move || {
            let status = child.wait();
            supervisor.reaped(&handle, status);
        });
        Ok(Verdict::done())
    }

    pub fn stop(&self, project: &Identity, name: &str) -> Result<Verdict, String> {
        let handle = self.handle(project, name);
        Ok(match self.halt(&handle) {
            // Nothing to signal, and nothing to rewrite: the record still says what the unit exited
            // with, which is the whole point of stopping something that already crashed.
            Halt::NotRunning => Verdict::note(format!("{name} is not running")),
            Halt::Stopped => Verdict::done(),
            Halt::Escalated => Verdict::note(escalation(name, self.grace)),
        })
    }

    pub fn restart(self: &Arc<Self>, project: &Identity, spec: &Spec) -> Result<Verdict, String> {
        let handle = self.handle(project, &spec.name);
        let halted = self.halt(&handle);
        let verdict = self.start(project, spec)?;
        Ok(match (halted, &verdict.note) {
            (Halt::Escalated, None) => Verdict::note(escalation(&spec.name, self.grace)),
            _ => verdict,
        })
    }

    /// Every unit this project has a record or a live entry for. Another project's units are its own
    /// business, even the ones it happens to have named the same.
    pub fn status(&self, project: &Identity) -> BTreeMap<String, Status> {
        let key = project.key();
        let slot = self.store.slot(project);
        // A docker unit's file lives in the same directory and is a display cache, never a claim
        // about a process this daemon could have been the parent of.
        let mut statuses: BTreeMap<String, Status> = slot
            .records()
            .into_iter()
            .filter(|(unit, _)| unit::name_of(unit, unit::LOCAL).is_some())
            .map(|(unit, record)| (unit, remembered(&record)))
            .collect();

        for (handle, live) in self.live.lock().expect("live units").iter() {
            if handle.project == key {
                statuses.insert(
                    handle.unit.clone(),
                    Status {
                        state: State::Up,
                        uptime: live.started_at.elapsed().ok(),
                        exit: None,
                        note: None,
                    },
                );
            }
        }
        statuses
    }

    /// §7, before anything else a daemon does: whatever its predecessor left running dies now. A
    /// record's pid is only ever acted on when `ps` agrees it is still the same process.
    pub fn kill_leftovers(&self) -> Vec<String> {
        let mut killed = Vec::new();
        for slot in self.store.slots() {
            for (unit, record) in slot.records() {
                if unit::name_of(&unit, unit::LOCAL).is_none() {
                    continue;
                }
                let Some(pid) = record.pid else { continue };
                if local::still_running(pid, record.ps_start.as_deref()) {
                    let _ = local::term_group(pid);
                    if !settled(pid, self.grace) {
                        let _ = local::kill_group(pid);
                        settled(pid, REAP_WAIT);
                    }
                    killed.push(format!("{}/{unit}", slot.key()));
                }
                // Whether it was killed or had already gone, the pid is stale and its exit status is
                // unknowable: this daemon was never its parent.
                forget(&slot, &unit, &record);
            }
        }
        killed
    }

    /// Kill-on-exit. The invariant is worth more than the survival: killing the daemon is the
    /// deliberate stop-everything hatch.
    pub fn kill_all(&self) {
        let running: Vec<(Handle, u32)> = {
            let mut live = self.live.lock().expect("live units");
            live.iter_mut()
                .map(|(handle, unit)| {
                    unit.stopping = true;
                    (handle.clone(), unit.pid)
                })
                .collect()
        };
        for (_, pid) in &running {
            let _ = local::term_group(*pid);
        }
        if !self.all_quiet(&running, self.grace) {
            for (handle, pid) in &running {
                if !self.is_quiet(handle, *pid) {
                    let _ = local::kill_group(*pid);
                }
            }
            self.all_quiet(&running, REAP_WAIT);
        }
        // A reaping thread that never got scheduled must not leave a record claiming a pid that is
        // already dead, which the next daemon would then chase.
        for (handle, _) in &running {
            if self.holds(handle) {
                let slot = self.store.slot_at(&handle.project);
                if let Some(record) = slot.record(&handle.unit) {
                    forget(&slot, &handle.unit, &record);
                }
            }
        }
    }

    fn all_quiet(&self, running: &[(Handle, u32)], patience: Duration) -> bool {
        let deadline = std::time::Instant::now() + patience;
        loop {
            if running
                .iter()
                .all(|(handle, pid)| self.is_quiet(handle, *pid))
            {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }

    fn handle(&self, project: &Identity, name: &str) -> Handle {
        Handle {
            project: project.key(),
            unit: unit::key(unit::LOCAL, name),
        }
    }

    fn holds(&self, handle: &Handle) -> bool {
        self.live.lock().expect("live units").contains_key(handle)
    }

    /// A unit name belongs to the project that declared it: two projects that both call something
    /// `rails` each run their own. What a start has to be refused for is this project's own unit
    /// already running, which the live table answers by the whole handle.
    fn running(&self, handle: &Handle) -> Option<u32> {
        let live = self.live.lock().expect("live units");
        Some(live.get(handle)?.pid)
    }

    fn halt(&self, handle: &Handle) -> Halt {
        let Some(pid) = self.mark_stopping(handle) else {
            return Halt::NotRunning;
        };
        let _ = local::term_group(pid);
        if self.quiet(handle, pid, self.grace) {
            return Halt::Stopped;
        }
        let _ = local::kill_group(pid);
        self.quiet(handle, pid, REAP_WAIT);
        Halt::Escalated
    }

    fn mark_stopping(&self, handle: &Handle) -> Option<u32> {
        let mut live = self.live.lock().expect("live units");
        let unit = live.get_mut(handle)?;
        unit.stopping = true;
        Some(unit.pid)
    }

    /// Stopped means two things and needs both: the wrapper reaped — the live entry only goes away
    /// once `wait()` has returned — and nothing left in its process group. A wrapper that dies on
    /// `SIGTERM` while its children ignore it would otherwise pass for a clean stop.
    fn quiet(&self, handle: &Handle, pid: u32, patience: Duration) -> bool {
        let deadline = std::time::Instant::now() + patience;
        loop {
            if self.is_quiet(handle, pid) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }

    fn is_quiet(&self, handle: &Handle, pid: u32) -> bool {
        !self.holds(handle) && local::group_empty(pid)
    }

    /// The record goes down before the live entry goes away, so a caller that waited for the entry to
    /// vanish is looking at a record that already knows how the unit ended.
    fn reaped(&self, handle: &Handle, status: std::io::Result<std::process::ExitStatus>) {
        let stopping = self
            .live
            .lock()
            .expect("live units")
            .get(handle)
            .is_some_and(|unit| unit.stopping);
        let slot = self.store.slot_at(&handle.project);
        let mut record = slot.record(&handle.unit).unwrap_or_else(Record::stopped);
        record.state = if stopping { State::Down } else { State::Dead };
        record.pid = None;
        record.started_at = None;
        record.ps_start = None;
        record.exit = status.ok().map(Exit::of);
        let _ = slot.write(&handle.unit, &record);
        self.live.lock().expect("live units").remove(handle);
    }
}

fn escalation(name: &str, grace: Duration) -> String {
    format!(
        "{name} ignored SIGTERM; killed after {}s",
        grace.as_secs().max(1)
    )
}

/// A record for a unit that is not running: its exit status stays, its pid does not.
fn forget(slot: &Slot, unit: &str, record: &Record) {
    if record.pid.is_none() && record.state != State::Up {
        return;
    }
    let mut record = record.clone();
    record.state = State::Down;
    record.pid = None;
    record.started_at = None;
    record.ps_start = None;
    let _ = slot.write(unit, &record);
}

/// Not running by any authority available to us: the process is gone.
fn remembered(record: &Record) -> Status {
    Status {
        state: match record.state {
            State::Dead => State::Dead,
            // A record claiming `up` outside the live table is a stale cache, never a running unit.
            _ => State::Down,
        },
        uptime: None,
        exit: record.exit,
        note: None,
    }
}

/// A leftover is another daemon's child, so there is no `wait()` to lean on: the group emptying is
/// the only evidence available.
fn settled(pid: u32, patience: Duration) -> bool {
    let deadline = std::time::Instant::now() + patience;
    while !local::group_empty(pid) {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::path::PathBuf;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let root = std::env::temp_dir().join(format!("herdr-dev-supervisor-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("scratch");
            Scratch(root)
        }

        fn project(&self, name: &str) -> Identity {
            let path = self.0.join(name);
            std::fs::create_dir_all(&path).expect("project root");
            Identity {
                path,
                name: name.to_string(),
            }
        }

        fn supervisor(&self, grace: Duration) -> Arc<Supervisor> {
            Supervisor::new(&self.0.join("state"), grace)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn spec(name: &str, cmd: &[&str], cwd: &Path) -> Spec {
        Spec {
            name: name.to_string(),
            cmd: cmd.iter().map(|word| word.to_string()).collect(),
            cwd: cwd.to_path_buf(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn a_record_claiming_up_without_a_live_entry_reads_as_down_rather_than_running() {
        let scratch = Scratch::new("stale");
        let supervisor = scratch.supervisor(GRACE);
        let project = scratch.project("storefront");
        let slot = supervisor.store().open(&project).expect("slot");
        let key = unit::key(unit::LOCAL, "vite");
        slot.write(
            &key,
            &Record {
                state: State::Up,
                pid: Some(1),
                started_at: Some(SystemTime::now()),
                ps_start: Some("Mon Aug 18 17:32:05 2026".into()),
                cmd: vec!["bin/vite".into()],
                cwd: project.path.clone(),
                exit: None,
            },
        )
        .expect("write");

        let status = &supervisor.status(&project)[&key];
        assert_eq!(status.state, State::Down);
        assert_eq!(status.uptime, None);
    }

    #[test]
    fn a_dead_units_exit_survives_in_the_status_it_is_read_back_as() {
        let scratch = Scratch::new("remembered");
        let supervisor = scratch.supervisor(GRACE);
        let project = scratch.project("storefront");
        let slot = supervisor.store().open(&project).expect("slot");
        let key = unit::key(unit::LOCAL, "rails");
        let mut record = Record::stopped();
        record.state = State::Dead;
        record.exit = Some(Exit::Code(7));
        slot.write(&key, &record).expect("write");

        let status = &supervisor.status(&project)[&key];
        assert_eq!(status.state, State::Dead);
        assert_eq!(status.timing(), "exit 7");
    }

    #[test]
    fn stopping_something_that_never_ran_says_so_and_leaves_its_record_alone() {
        let scratch = Scratch::new("absent");
        let supervisor = scratch.supervisor(GRACE);
        let project = scratch.project("storefront");
        let slot = supervisor.store().open(&project).expect("slot");
        let key = unit::key(unit::LOCAL, "rails");
        let mut crashed = Record::stopped();
        crashed.state = State::Dead;
        crashed.exit = Some(Exit::Code(7));
        slot.write(&key, &crashed).expect("write");

        let verdict = supervisor.stop(&project, "rails").expect("stop");
        assert_eq!(verdict.note.as_deref(), Some("rails is not running"));
        assert_eq!(slot.record(&key).expect("record"), crashed);
    }

    #[test]
    fn a_unit_this_project_already_runs_is_refused_rather_than_started_twice() {
        let scratch = Scratch::new("twice");
        let supervisor = scratch.supervisor(Duration::from_millis(200));
        let project = scratch.project("storefront");

        let sleeper = spec("sleeper", &["sleep", "20"], &project.path);
        assert_eq!(
            supervisor.start(&project, &sleeper).expect("start"),
            Verdict::done()
        );
        let key = unit::key(unit::LOCAL, "sleeper");
        let pid = supervisor
            .store()
            .slot(&project)
            .record(&key)
            .expect("record")
            .pid;
        let refusal = supervisor
            .start(&project, &sleeper)
            .expect("second start")
            .note
            .expect("a refusal");
        assert_eq!(
            refusal,
            format!("sleeper is already running, pid {}", pid.expect("a pid"))
        );

        supervisor.kill_all();
        assert!(!supervisor.busy());
    }

    #[test]
    fn two_projects_that_name_a_unit_alike_each_run_their_own() {
        let scratch = Scratch::new("alike");
        let supervisor = scratch.supervisor(Duration::from_millis(200));
        let one = scratch.project("storefront");
        let two = scratch.project("accounts");

        for project in [&one, &two] {
            assert_eq!(
                supervisor
                    .start(project, &spec("rails", &["sleep", "20"], &project.path))
                    .expect("start"),
                Verdict::done(),
            );
        }

        let key = unit::key(unit::LOCAL, "rails");
        assert_eq!(supervisor.status(&one)[&key].state, State::Up);
        assert_eq!(supervisor.status(&two)[&key].state, State::Up);
        let pid = |project| {
            supervisor
                .store()
                .slot(project)
                .record(&key)
                .expect("record")
                .pid
        };
        assert_ne!(pid(&one), pid(&two), "one process answered for both");

        supervisor.kill_all();
        assert!(!supervisor.busy());
    }

    #[test]
    fn an_immediate_failure_is_witnessed_by_its_parent_with_the_code_it_exited_with() {
        let scratch = Scratch::new("boom");
        let supervisor = scratch.supervisor(Duration::from_millis(200));
        let project = scratch.project("storefront");
        let key = unit::key(unit::LOCAL, "boom");

        supervisor
            .start(
                &project,
                &spec("boom", &["sh", "-c", "exit 7"], &project.path),
            )
            .expect("start");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while supervisor.busy() && std::time::Instant::now() < deadline {
            std::thread::sleep(POLL);
        }

        let status = &supervisor.status(&project)[&key];
        assert_eq!(status.state, State::Dead, "{status:?}");
        assert_eq!(status.exit, Some(Exit::Code(7)));
        assert!(
            supervisor.store().slot(&project).log_path(&key).exists(),
            "a unit that died at once still owns a log"
        );
    }

    #[test]
    fn stopping_a_unit_records_down_while_keeping_what_it_exited_with() {
        let scratch = Scratch::new("stop");
        let supervisor = scratch.supervisor(GRACE);
        let project = scratch.project("storefront");
        let key = unit::key(unit::LOCAL, "sleeper");

        supervisor
            .start(&project, &spec("sleeper", &["sleep", "20"], &project.path))
            .expect("start");
        assert!(supervisor.busy());
        assert_eq!(
            supervisor.stop(&project, "sleeper").expect("stop"),
            Verdict::done()
        );
        assert!(!supervisor.busy());

        let record = supervisor
            .store()
            .slot(&project)
            .record(&key)
            .expect("record");
        assert_eq!(record.state, State::Down);
        assert_eq!(record.pid, None);
        assert!(record.exit.is_some(), "a stop is still an exit");
        assert_eq!(supervisor.status(&project)[&key].state, State::Down);
    }

    #[test]
    fn a_restart_leaves_one_previous_generation_of_the_log_and_a_new_pid() {
        let scratch = Scratch::new("restart");
        let supervisor = scratch.supervisor(GRACE);
        let project = scratch.project("storefront");
        let key = unit::key(unit::LOCAL, "chatty");
        let chatty = spec(
            "chatty",
            &["sh", "-c", "echo generation; sleep 20"],
            &project.path,
        );

        supervisor.start(&project, &chatty).expect("start");
        let slot = supervisor.store().slot(&project);
        let first = slot.record(&key).expect("record").pid.expect("a pid");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::fs::read_to_string(slot.log_path(&key))
            .unwrap_or_default()
            .is_empty()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(POLL);
        }

        supervisor.restart(&project, &chatty).expect("restart");
        let second = slot.record(&key).expect("record").pid.expect("a pid");
        assert_ne!(first, second);
        assert_eq!(
            std::fs::read_dir(slot.dir().join("logs"))
                .expect("logs")
                .count(),
            2,
            "a restart keeps exactly one previous generation"
        );

        supervisor.kill_all();
    }
}
