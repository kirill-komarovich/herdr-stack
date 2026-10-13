//! State and log storage: one directory per project, one TOML record and one log per unit.
//!
//! Every write goes down as `.tmp` and is then `rename()`d, because `popup.close` can kill the TUI —
//! and the daemon with the last client — with no warning, so nothing may be buffered for exit.
//!
//! A record is a cache of what the daemon knew when it last wrote one. Its `state` field says whether
//! a unit was stopped on purpose or died on its own; whether it is *running* is never read from here,
//! only from the daemon's `wait()`. A docker unit's file is a `Cache` rather than a `Record`, and is
//! never read to decide liveness either: `compose ps` is asked on every read.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use toml_edit::{Array, DocumentMut, Item, Value as TomlValue, value};

use crate::unit::{Exit, State};

const PROJECTS: &str = "projects";
const UNITS: &str = "units";
const LOGS: &str = "logs";
const PROJECT_FILE: &str = "project.toml";

/// The symlink dropped in the project root so `tail -f .herdr-stack-logs/local-vite.log` works from
/// where you already are. The trailing `*` in `~/.gitignore_global` is what keeps it untracked.
pub const LOG_LINK: &str = ".herdr-stack-logs";

/// `<basename>-<first 8 hex of sha256(absolute path)>`: readable, and two checkouts of one repo never
/// collide.
pub fn project_key(path: &Path) -> String {
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());
    format!("{name}-{:.8}", hex(&digest))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A project as its own record spells it: the path is what the cleanup rule checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub path: PathBuf,
    pub name: String,
}

impl Identity {
    pub fn key(&self) -> String {
        project_key(&self.path)
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn at(root: impl Into<PathBuf>) -> Store {
        Store { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path arithmetic only: a slot for a project the store may never have seen.
    pub fn slot(&self, identity: &Identity) -> Slot {
        Slot {
            dir: self.root.join(PROJECTS).join(identity.key()),
        }
    }

    pub fn open(&self, identity: &Identity) -> std::io::Result<Slot> {
        let slot = self.slot(identity);
        std::fs::create_dir_all(slot.dir.join(UNITS))?;
        std::fs::create_dir_all(slot.dir.join(LOGS))?;
        if slot.identity().as_ref() != Some(identity) {
            let mut doc = DocumentMut::new();
            doc["path"] = value(identity.path.to_string_lossy().as_ref());
            doc["name"] = value(&identity.name);
            replace(&slot.dir.join(PROJECT_FILE), doc.to_string().as_bytes())?;
        }
        slot.link_logs(&identity.path);
        Ok(slot)
    }

    pub fn slot_at(&self, key: &str) -> Slot {
        Slot {
            dir: self.root.join(PROJECTS).join(key),
        }
    }

    /// Every project the store has a directory for, in key order.
    pub fn slots(&self) -> Vec<Slot> {
        let mut slots: Vec<Slot> = match std::fs::read_dir(self.root.join(PROJECTS)) {
            Err(_) => return Vec::new(),
            Ok(entries) => entries
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .map(|entry| Slot { dir: entry.path() })
                .collect(),
        };
        slots.sort_by(|a, b| a.dir.cmp(&b.dir));
        slots
    }

    /// The one cleanup rule: a project directory whose recorded path is gone. No TTL, and a project
    /// whose `project.toml` cannot be read is left alone rather than guessed about.
    pub fn drop_vanished(&self) -> Vec<PathBuf> {
        let mut dropped = Vec::new();
        for slot in self.slots() {
            let Some(identity) = slot.identity() else {
                continue;
            };
            if !identity.path.exists() && std::fs::remove_dir_all(slot.dir()).is_ok() {
                dropped.push(identity.path);
            }
        }
        dropped
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    dir: PathBuf,
}

impl Slot {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn key(&self) -> String {
        self.dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn identity(&self) -> Option<Identity> {
        let text = std::fs::read_to_string(self.dir.join(PROJECT_FILE)).ok()?;
        let doc = text.parse::<DocumentMut>().ok()?;
        let path = doc.get("path")?.as_str()?.to_string();
        Some(Identity {
            name: doc
                .get("name")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string(),
            path: PathBuf::from(path),
        })
    }

    pub fn record_path(&self, unit: &str) -> PathBuf {
        self.dir.join(UNITS).join(format!("{unit}.toml"))
    }

    pub fn record(&self, unit: &str) -> Option<Record> {
        Record::read(&std::fs::read_to_string(self.record_path(unit)).ok()?)
    }

    pub fn write(&self, unit: &str, record: &Record) -> std::io::Result<()> {
        replace(&self.record_path(unit), record.to_toml().as_bytes())
    }

    pub fn records(&self) -> BTreeMap<String, Record> {
        let mut records = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(self.dir.join(UNITS)) else {
            return records;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "toml")
            {
                let Some(name) = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                else {
                    continue;
                };
                if let Some(record) = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| Record::read(&text))
                {
                    records.insert(name, record);
                }
            }
        }
        records
    }

    pub fn cache(&self, unit: &str) -> Option<Cache> {
        Cache::read(&std::fs::read_to_string(self.record_path(unit)).ok()?)
    }

    pub fn write_cache(&self, unit: &str, cache: &Cache) -> std::io::Result<()> {
        replace(&self.record_path(unit), cache.to_toml().as_bytes())
    }

    pub fn log_path(&self, unit: &str) -> PathBuf {
        self.dir.join(LOGS).join(format!("{unit}.log"))
    }

    fn previous_log_path(&self, unit: &str) -> PathBuf {
        self.dir.join(LOGS).join(format!("{unit}.log.1"))
    }

    /// A fresh log for a spawn, the last one kept as `.log.1`. Rotation is only ever possible here:
    /// a running unit holds the log fd itself, so renaming under it would leave it writing to the
    /// old inode.
    pub fn open_log(&self, unit: &str) -> std::io::Result<File> {
        let log = self.log_path(unit);
        std::fs::create_dir_all(self.dir.join(LOGS))?;
        if log.exists() {
            std::fs::rename(&log, self.previous_log_path(unit))?;
        }
        File::create(&log)
    }

    /// Best effort: a project whose root is gone, or read-only, is not a reason to refuse a spawn.
    fn link_logs(&self, project_root: &Path) {
        let link = project_root.join(LOG_LINK);
        let target = self.dir.join(LOGS);
        if std::fs::read_link(&link).is_ok_and(|current| current == target) {
            return;
        }
        if !project_root.is_dir() {
            return;
        }
        let _ = std::fs::remove_file(&link);
        let _ = std::os::unix::fs::symlink(&target, &link);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub state: State,
    pub pid: Option<u32>,
    pub started_at: Option<SystemTime>,
    /// `ps -o lstart=` as it read at spawn. Its one role is telling a leftover from a reused pid.
    pub ps_start: Option<String>,
    pub cmd: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub tty: bool,
    pub exit: Option<Exit>,
    /// When the unit's daemon let go of it while it was still running, which is the whole of what the
    /// next daemon needs to know to put the stack back. An explicit stop and a crash both clear it:
    /// neither is something to restore.
    pub restore: Option<SystemTime>,
}

impl Record {
    pub fn stopped() -> Record {
        Record {
            state: State::Down,
            pid: None,
            started_at: None,
            ps_start: None,
            cmd: Vec::new(),
            cwd: PathBuf::new(),
            env: BTreeMap::new(),
            tty: false,
            exit: None,
            restore: None,
        }
    }

    pub fn uptime(&self) -> Option<Duration> {
        self.started_at
            .and_then(|started| SystemTime::now().duration_since(started).ok())
    }

    pub fn to_toml(&self) -> String {
        let mut doc = DocumentMut::new();
        doc["state"] = value(self.state.label());
        if let Some(pid) = self.pid {
            doc["pid"] = value(pid as i64);
        }
        if let Some(started_at) = self.started_at {
            doc["started_at"] = value(epoch(started_at));
        }
        if let Some(ps_start) = &self.ps_start {
            doc["ps_start"] = value(ps_start);
        }
        if !self.cmd.is_empty() {
            let mut cmd = Array::new();
            for word in &self.cmd {
                cmd.push(word.as_str());
            }
            doc["cmd"] = Item::Value(TomlValue::Array(cmd));
        }
        if !self.cwd.as_os_str().is_empty() {
            doc["cwd"] = value(self.cwd.to_string_lossy().as_ref());
        }
        if !self.env.is_empty() {
            let mut env = toml_edit::InlineTable::new();
            for (key, set_to) in &self.env {
                env.insert(key, TomlValue::from(set_to.as_str()));
            }
            doc["env"] = Item::Value(TomlValue::InlineTable(env));
        }
        if self.tty {
            doc["tty"] = value(true);
        }
        if let Some(restore) = self.restore {
            doc["restore"] = value(epoch(restore));
        }
        match self.exit {
            Some(Exit::Code(code)) => doc["exit_code"] = value(code as i64),
            Some(Exit::Signal(signal)) => doc["signal"] = value(signal as i64),
            None => {}
        }
        doc.to_string()
    }

    pub fn read(text: &str) -> Option<Record> {
        let doc = text.parse::<DocumentMut>().ok()?;
        let exit = match (doc.get("exit_code"), doc.get("signal")) {
            (Some(code), _) => code.as_integer().map(|code| Exit::Code(code as i32)),
            (None, Some(signal)) => signal
                .as_integer()
                .map(|signal| Exit::Signal(signal as i32)),
            (None, None) => None,
        };
        Some(Record {
            state: doc
                .get("state")
                .and_then(Item::as_str)
                .and_then(State::read)
                .unwrap_or(State::Unknown),
            pid: doc
                .get("pid")
                .and_then(Item::as_integer)
                .map(|pid| pid as u32),
            started_at: doc
                .get("started_at")
                .and_then(Item::as_float)
                .map(|seconds| UNIX_EPOCH + Duration::from_secs_f64(seconds)),
            ps_start: doc
                .get("ps_start")
                .and_then(Item::as_str)
                .map(str::to_string),
            cmd: doc
                .get("cmd")
                .and_then(Item::as_array)
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|word| word.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            cwd: doc
                .get("cwd")
                .and_then(Item::as_str)
                .map(PathBuf::from)
                .unwrap_or_default(),
            env: doc
                .get("env")
                .and_then(Item::as_table_like)
                .map(|table| {
                    table
                        .iter()
                        .filter_map(|(key, set_to)| {
                            Some((key.to_string(), set_to.as_str()?.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            tty: doc.get("tty").and_then(Item::as_bool).unwrap_or_default(),
            exit,
            restore: doc
                .get("restore")
                .and_then(Item::as_float)
                .map(|seconds| UNIX_EPOCH + Duration::from_secs_f64(seconds)),
        })
    }
}

/// The docker record: the timestamped `compose ps` result last seen, and nothing else. It paints a
/// row fast and gives the row something to show — marked stale, never pretended live — when docker
/// cannot be reached. It is never consulted to decide whether a service is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cache {
    /// Docker's own `State` and `Health` words, kept verbatim so one mapping reads both a live
    /// observation and a remembered one.
    pub state: String,
    pub health: String,
    pub exit_code: i32,
    pub seen_at: SystemTime,
    /// The uptime as it read at `seen_at`, which is why a stale row can say how long a service had
    /// been up without claiming it still is.
    pub uptime: Option<Duration>,
}

impl Cache {
    pub fn age(&self) -> Duration {
        self.seen_at.elapsed().unwrap_or_default()
    }

    pub fn to_toml(&self) -> String {
        let mut doc = DocumentMut::new();
        doc["state"] = value(&self.state);
        doc["health"] = value(&self.health);
        doc["exit_code"] = value(self.exit_code as i64);
        doc["seen_at"] = value(epoch(self.seen_at));
        if let Some(uptime) = self.uptime {
            doc["uptime"] = value(uptime.as_secs_f64());
        }
        doc.to_string()
    }

    pub fn read(text: &str) -> Option<Cache> {
        let doc = text.parse::<DocumentMut>().ok()?;
        Some(Cache {
            state: doc
                .get("state")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string(),
            health: doc
                .get("health")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string(),
            exit_code: doc
                .get("exit_code")
                .and_then(Item::as_integer)
                .unwrap_or_default() as i32,
            seen_at: UNIX_EPOCH
                + Duration::from_secs_f64(doc.get("seen_at").and_then(Item::as_float)?),
            uptime: doc
                .get("uptime")
                .and_then(Item::as_float)
                .map(Duration::from_secs_f64),
        })
    }
}

fn epoch(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs_f64())
        .unwrap_or_default()
}

/// `.tmp` then `rename()` — atomic on APFS, so a reader never sees half a record.
fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::unit;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let root = std::env::temp_dir().join(format!("herdr-stack-store-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("scratch");
            Scratch(root)
        }

        fn store(&self) -> Store {
            Store::at(self.0.join("state"))
        }

        fn project(&self, name: &str) -> Identity {
            let path = self.0.join(name);
            std::fs::create_dir_all(&path).expect("project root");
            Identity {
                path,
                name: name.to_string(),
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn running(pid: u32) -> Record {
        Record {
            state: State::Up,
            pid: Some(pid),
            started_at: Some(SystemTime::now()),
            ps_start: Some("Mon Aug 18 17:32:05 2026".into()),
            cmd: vec!["bin/vite".into(), "dev".into()],
            cwd: PathBuf::from("/repos/storefront"),
            env: BTreeMap::from([("VITE_RUBY_HOST".to_string(), "127.0.0.1".to_string())]),
            tty: false,
            exit: None,
            restore: None,
        }
    }

    #[test]
    fn two_checkouts_of_one_repo_get_readable_keys_that_do_not_collide() {
        let one = project_key(Path::new("/repos/storefront"));
        let two = project_key(Path::new("/repos/storefront-wt2/storefront"));
        assert!(one.starts_with("storefront-"), "{one}");
        assert_ne!(one, two);
        assert_eq!(one.len(), "storefront-".len() + 8);
        assert_eq!(project_key(Path::new("/repos/storefront")), one);
    }

    #[test]
    fn opening_a_project_writes_its_identity_and_links_its_logs_into_the_project_root() {
        let scratch = Scratch::new("open");
        let store = scratch.store();
        let storefront = scratch.project("storefront");
        let slot = store.open(&storefront).expect("slot");

        assert_eq!(slot.identity(), Some(storefront.clone()));
        assert_eq!(slot.key(), storefront.key());
        assert_eq!(
            std::fs::read_link(storefront.path.join(LOG_LINK)).expect("symlink"),
            slot.dir().join(LOGS)
        );

        // Opening again is the common case — every verb does it — and must not churn the link.
        let again = store.open(&storefront).expect("slot again");
        assert_eq!(again, slot);
    }

    #[test]
    fn a_record_survives_a_round_trip_through_its_file_and_leaves_no_temporary_behind() {
        let scratch = Scratch::new("record");
        let store = scratch.store();
        let slot = store.open(&scratch.project("storefront")).expect("slot");
        let key = unit::key(unit::LOCAL, "vite");

        slot.write(&key, &running(4242)).expect("write");
        let read = slot.record(&key).expect("record");
        assert_eq!(read.pid, Some(4242));
        assert_eq!(read.state, State::Up);
        assert_eq!(read.cmd, vec!["bin/vite".to_string(), "dev".to_string()]);
        assert_eq!(read.ps_start.as_deref(), Some("Mon Aug 18 17:32:05 2026"));
        assert!(read.uptime().is_some());
        assert!(
            !slot.record_path(&key).with_extension("tmp").exists(),
            "a temporary file was left in the units directory"
        );
        assert_eq!(slot.records().keys().collect::<Vec<_>>(), vec![&key]);
    }

    /// Everything a re-spawn needs, which is the whole `Spec` and not just the command.
    #[test]
    fn what_a_unit_would_have_to_be_started_again_with_survives_the_round_trip() {
        let scratch = Scratch::new("respawn");
        let store = scratch.store();
        let slot = store.open(&scratch.project("storefront")).expect("slot");
        let key = unit::key(unit::LOCAL, "console");

        let left_at = SystemTime::now() - Duration::from_secs(90);
        let mut record = running(4242);
        record.tty = true;
        record.restore = Some(left_at);
        slot.write(&key, &record).expect("write");

        let read = slot.record(&key).expect("record");
        assert_eq!(read.cwd, PathBuf::from("/repos/storefront"));
        assert_eq!(
            read.env.get("VITE_RUBY_HOST").map(String::as_str),
            Some("127.0.0.1")
        );
        assert!(read.tty);
        let marked = read.restore.expect("a restore mark");
        assert!(
            marked.duration_since(left_at).unwrap_or_default() < Duration::from_millis(1),
            "the mark drifted: {marked:?} against {left_at:?}"
        );
    }

    /// A record written before any of this existed still reads, and asks for nothing.
    #[test]
    fn a_record_from_an_older_build_reads_without_a_terminal_or_a_restore_mark() {
        let older =
            "state = \"up\"\npid = 4242\ncmd = [\"bin/vite\"]\ncwd = \"/repos/storefront\"\n";
        let read = Record::read(older).expect("an older record reads");
        assert_eq!(read.state, State::Up);
        assert!(read.env.is_empty());
        assert!(!read.tty);
        assert_eq!(read.restore, None);
    }

    #[test]
    fn an_exit_status_outlives_the_state_it_was_recorded_with() {
        let scratch = Scratch::new("exit");
        let store = scratch.store();
        let slot = store.open(&scratch.project("storefront")).expect("slot");
        let key = unit::key(unit::LOCAL, "vite");

        let mut crashed = running(4242);
        crashed.state = State::Dead;
        crashed.exit = Some(Exit::Code(7));
        crashed.pid = None;
        slot.write(&key, &crashed).expect("write");
        assert_eq!(slot.record(&key).expect("record").exit, Some(Exit::Code(7)));

        let mut signalled = crashed.clone();
        signalled.exit = Some(Exit::Signal(9));
        slot.write(&key, &signalled).expect("rewrite");
        assert_eq!(
            slot.record(&key).expect("record").exit,
            Some(Exit::Signal(9))
        );
    }

    #[test]
    fn a_spawn_takes_a_fresh_log_and_keeps_exactly_one_previous_generation() {
        let scratch = Scratch::new("logs");
        let store = scratch.store();
        let slot = store.open(&scratch.project("storefront")).expect("slot");
        let key = unit::key(unit::LOCAL, "vite");

        for generation in ["first", "second", "third"] {
            let mut log = slot.open_log(&key).expect("log");
            writeln!(log, "{generation}").expect("write");
        }

        assert_eq!(
            std::fs::read_to_string(slot.log_path(&key)).expect("log"),
            "third\n"
        );
        assert_eq!(
            std::fs::read_to_string(slot.previous_log_path(&key)).expect("previous log"),
            "second\n"
        );
        let generations = std::fs::read_dir(slot.dir().join(LOGS))
            .expect("logs")
            .count();
        assert_eq!(generations, 2, "more than one generation was kept");
    }

    #[test]
    fn a_project_whose_directory_is_gone_is_dropped_and_the_rest_are_left_alone() {
        let scratch = Scratch::new("cleanup");
        let store = scratch.store();
        let kept = scratch.project("storefront");
        let vanished = scratch.project("gone");
        store.open(&kept).expect("kept");
        let vanished_slot = store.open(&vanished).expect("vanished");
        std::fs::remove_dir_all(&vanished.path).expect("remove the project");

        assert_eq!(store.drop_vanished(), vec![vanished.path.clone()]);
        assert!(!vanished_slot.dir().exists());
        assert!(store.slot(&kept).dir().exists());
        assert_eq!(store.slots().len(), 1);
        assert!(store.drop_vanished().is_empty());
    }

    #[test]
    fn a_project_directory_without_a_readable_identity_is_never_dropped() {
        let scratch = Scratch::new("unreadable");
        let store = scratch.store();
        let orphan = store.root().join(PROJECTS).join("mystery-00000000");
        std::fs::create_dir_all(&orphan).expect("orphan");

        assert!(store.drop_vanished().is_empty());
        assert!(orphan.exists());
    }
}
