//! The env a directory gets from the user's shell, for the commands herdr-stack runs there itself:
//! `docker compose` takes `COMPOSE_PROJECT_NAME` and every `${VAR}` in a compose file from it, and a
//! repo whose mise or direnv config sets those is unreadable without it.
//!
//! A status reads compose for every project on screen, and starting the shell costs more than the read,
//! so each process keeps what it read per directory. It is read again once a config file there
//! changes, or after `FRESH_FOR`, which is how a change outside the directory still arrives. Never
//! written down: an env carries the secrets a repo's local config holds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::shell::Shell;

const FRESH_FOR: Duration = Duration::from_secs(30);

/// What mise and direnv read in a directory, the second's own helper files included.
const WATCHED: [&str; 8] = [
    ".envrc",
    ".env",
    "mise.toml",
    ".mise.toml",
    "mise.local.toml",
    ".mise.local.toml",
    ".tool-versions",
    ".direnv",
];

type Env = Vec<(String, String)>;

struct Read {
    at: Instant,
    fingerprint: Vec<Option<SystemTime>>,
    env: Env,
}

static READS: Mutex<BTreeMap<PathBuf, Read>> = Mutex::new(BTreeMap::new());

/// Runs `command` on its directory's env instead of the process's own: the machine-level part of
/// that is the base the shell starts from, and nothing project-scoped of the caller's crosses
/// (`crate::env`). A shell that cannot be read leaves the command on the machine-level env alone.
pub fn apply(command: &mut Command) {
    let base = crate::env::machine_level();
    let env = command
        .get_current_dir()
        .and_then(|dir| of(dir, &base))
        .unwrap_or(base);
    command.env_clear().envs(env);
}

fn of(dir: &Path, base: &Env) -> Option<Env> {
    let fingerprint = fingerprint(dir);
    let mut reads = READS.lock().ok()?;
    if let Some(read) = reads.get(dir)
        && read.fingerprint == fingerprint
        && read.at.elapsed() < FRESH_FOR
    {
        return Some(read.env.clone());
    }
    let env = Shell::of(base).env_in(dir, base).ok()?;
    reads.insert(
        dir.to_path_buf(),
        Read {
            at: Instant::now(),
            fingerprint,
            env: env.clone(),
        },
    );
    Some(env)
}

/// A file's modification time, or a directory's latest among its entries.
fn fingerprint(dir: &Path) -> Vec<Option<SystemTime>> {
    WATCHED
        .iter()
        .map(|name| {
            let path = dir.join(name);
            let own = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok();
            let inside = std::fs::read_dir(&path).ok().and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter_map(|entry| entry.metadata().and_then(|meta| meta.modified()).ok())
                    .max()
            });
            own.max(inside)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_config_file_changes_the_fingerprint_and_an_untouched_one_does_not() {
        let dir = std::env::temp_dir().join(format!("herdr-stack-dir-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".direnv")).expect("dir");
        std::fs::write(dir.join(".envrc"), "export A=1\n").expect(".envrc");
        let before = fingerprint(&dir);
        assert_eq!(fingerprint(&dir), before);

        let later = SystemTime::now() + Duration::from_secs(5);
        let helper = dir.join(".direnv/.envrc.app");
        std::fs::write(&helper, "export B=1\n").expect("helper");
        std::fs::File::options()
            .write(true)
            .open(&helper)
            .and_then(|file| file.set_modified(later))
            .expect("touch");
        let after = fingerprint(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_ne!(after, before);
    }
}
