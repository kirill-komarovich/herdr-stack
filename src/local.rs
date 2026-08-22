//! The spawn recipe of §6 and the group signals of §10, and nothing about who called for them.
//!
//! Three parts of the spawn are load-bearing and none of them is optional: `mise exec --` with cwd
//! set, so the toolchain and every port come from the repo's own mise files rather than the manifest;
//! **`setsid(2)`**, so the unit outlives the pane the popup was drawn over — redirecting stdio alone
//! is measurably not enough, it is the shared session that kills; and stdio pointed at a log with
//! stdin from `/dev/null`, so a dev server never wedges a Herdr pipe or takes SIGPIPE from a dying
//! server.

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::SystemTime;

use crate::herdr::expand_tilde;

/// Spelled out for the same reason the state root is: a popup inherits no `PATH` worth trusting.
pub const MISE: &str = "~/.local/bin/mise";

/// A missing toolchain must fail fast into the log rather than hang on a silent download.
const NO_AUTO_INSTALL: (&str, &str) = ("MISE_EXEC_AUTO_INSTALL", "false");

/// What the daemon was told to run. The env here is the manifest's two layers only; the process layer
/// under it is the daemon's own, added at spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub name: String,
    pub cmd: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// A unit the daemon has just become the parent of.
#[derive(Debug)]
pub struct Spawned {
    pub child: Child,
    pub pid: u32,
    /// The moment of the fork, at full resolution — not `ps -o lstart=`'s one second.
    pub started_at: SystemTime,
    pub ps_start: Option<String>,
}

pub fn mise_path() -> PathBuf {
    expand_tilde(MISE)
}

/// Everything about the spawn except the fork, so the recipe can be read off a `Command`. Env layers
/// innermost last: `base` — the daemon's own — then the manifest's, then ours.
pub fn command<I>(spec: &Spec, mise: &Path, base: I) -> Command
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut command = Command::new(mise);
    command.arg("exec").arg("--").args(&spec.cmd);
    command.current_dir(&spec.cwd);
    command.env_clear();
    command.envs(base);
    command.envs(&spec.env);
    command.env(NO_AUTO_INSTALL.0, NO_AUTO_INSTALL.1);
    command
}

/// `log` becomes both stdout and stderr; the caller owns its rotation.
pub fn spawn(spec: &Spec, log: File) -> std::io::Result<Spawned> {
    let errors = log.try_clone()?;
    let mut command = command(spec, &mise_path(), std::env::vars());
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(errors));
    // `setsid(2)` leaves the child leading a new session *and* a new process group, which is what a
    // group signal later needs. `Command::process_group(0)` would make it a group leader first and
    // then `setsid` returns EPERM — measured — so the two cannot be combined.
    unsafe {
        command.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }

    let started_at = SystemTime::now();
    let child = command.spawn()?;
    let pid = child.id();
    Ok(Spawned {
        child,
        pid,
        started_at,
        ps_start: ps_field(pid, "lstart"),
    })
}

pub fn term_group(pid: u32) -> std::io::Result<()> {
    signal_group(pid, libc::SIGTERM)
}

pub fn kill_group(pid: u32) -> std::io::Result<()> {
    signal_group(pid, libc::SIGKILL)
}

/// The whole group, never the leader alone: measured, `TERM` to a `bin/vite dev` wrapper killed the
/// wrapper and left both its children running, orphaned.
fn signal_group(pid: u32, signal: i32) -> std::io::Result<()> {
    match unsafe { libc::killpg(pid as i32, signal) } {
        -1 => Err(std::io::Error::last_os_error()),
        _ => Ok(()),
    }
}

pub fn alive(pid: u32) -> bool {
    match unsafe { libc::kill(pid as i32, 0) } {
        0 => true,
        // A live process we do not own signals EPERM rather than ESRCH.
        _ => std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM),
    }
}

/// Whether the pid a record names is still the process that record was written for. §9's one surviving
/// role for `ps`: telling a leftover from a reused pid, so it can be killed. Never to adopt, never for
/// display.
pub fn still_running(pid: u32, ps_start: Option<&str>) -> bool {
    if !alive(pid) {
        return false;
    }
    match (ps_start, ps_field(pid, "lstart")) {
        (Some(recorded), Some(current)) => recorded == current && leads_its_group(pid),
        // Without a recorded start time a pid says nothing, and killing a group on a guess is worse
        // than leaving a unit behind.
        _ => false,
    }
}

/// A unit is spawned with `setsid(2)`, so anything whose group is not its own pid is not one of ours
/// and its group must not be signalled.
fn leads_its_group(pid: u32) -> bool {
    ps_field(pid, "pgid")
        .and_then(|group| group.parse::<u32>().ok())
        .is_some_and(|group| group == pid)
}

/// Whether anything is left in the unit's process group. Stopping is not done when the wrapper is
/// reaped — it is done when the tree it forked is gone, which is the whole hazard the group signal
/// exists for. macOS documents its own `ps -g` as ignored, so the whole table is the only way to ask.
pub fn group_empty(pgid: u32) -> bool {
    let Ok(output) = Command::new("/bin/ps")
        .arg("-ax")
        .arg("-o")
        .arg("pid=,pgid=")
        .output()
    else {
        // Unanswerable is not the same as empty: escalating on a failed `ps` is the safer half.
        return false;
    };
    !String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let mut columns = line.split_whitespace();
        let (_, group) = (columns.next(), columns.next());
        group.and_then(|group| group.parse::<u32>().ok()) == Some(pgid)
    })
}

fn ps_field(pid: u32, field: &str) -> Option<String> {
    let output = Command::new("/bin/ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg(format!("{field}="))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            name: "vite".into(),
            cmd: vec!["bin/vite".into(), "dev".into()],
            cwd: PathBuf::from("/repos/storefront"),
            env: BTreeMap::from([("VITE_RUBY_HOST".to_string(), "127.0.0.1".to_string())]),
        }
    }

    fn env_of(command: &Command) -> BTreeMap<String, String> {
        command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((
                    key.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect()
    }

    fn built(base: &[(&str, &str)]) -> Command {
        let base = base
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()));
        command(&spec(), Path::new("/opt/mise"), base)
    }

    #[test]
    fn a_unit_runs_through_mise_exec_from_its_own_directory() {
        let command = built(&[]);
        assert_eq!(command.get_program(), "/opt/mise");
        let argv: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, ["exec", "--", "bin/vite", "dev"]);
        assert_eq!(command.get_current_dir(), Some(Path::new("/repos/storefront")));
    }

    #[test]
    fn auto_install_is_off_so_a_missing_toolchain_fails_into_the_log() {
        let env = env_of(&built(&[(NO_AUTO_INSTALL.0, "true")]));
        assert_eq!(
            env.get(NO_AUTO_INSTALL.0).map(String::as_str),
            Some("false")
        );
    }

    #[test]
    fn the_units_env_is_layered_over_the_daemons_own_and_wins() {
        let env = env_of(&built(&[
            ("PATH", "/usr/bin"),
            ("VITE_RUBY_HOST", "from-the-daemon"),
        ]));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(
            env.get("VITE_RUBY_HOST").map(String::as_str),
            Some("127.0.0.1"),
            "the unit's own env must sit innermost"
        );
    }

    #[test]
    fn ps_reads_a_start_time_and_a_group_for_a_live_process_and_nothing_for_a_dead_one() {
        let mine = std::process::id();
        assert!(ps_field(mine, "lstart").is_some());
        assert!(alive(mine));
        // The test binary was not spawned with `setsid(2)`, so it is not its own group leader and
        // must never be taken for a leftover of ours.
        assert!(!still_running(mine, Some("Mon Aug 18 17:32:05 2026")));

        let mut gone = Command::new("/usr/bin/true").spawn().expect("true runs");
        let pid = gone.id();
        gone.wait().expect("reaped");
        assert!(!alive(pid));
        assert!(!still_running(pid, ps_field(pid, "lstart").as_deref()));
    }

    #[test]
    fn a_group_with_a_member_is_not_empty_and_one_that_never_existed_is() {
        let group = ps_field(std::process::id(), "pgid")
            .and_then(|group| group.parse::<u32>().ok())
            .expect("this process has a group");
        assert!(!group_empty(group));
        // No group can be numbered near the top of a u32: pids are bounded far below it.
        assert!(group_empty(u32::MAX - 1));
    }

    #[test]
    fn a_pid_with_no_recorded_start_time_is_never_treated_as_a_leftover() {
        assert!(!still_running(std::process::id(), None));
    }
}
