//! The spawn recipe and the group signals, and nothing about who called for them.
//!
//! Three parts of the spawn are load-bearing and none of them is optional: the directory's own env
//! loaders with cwd set — `mise exec --` and `direnv exec`, whichever are installed — so the toolchain
//! and every port come from the repo's own files rather than the manifest, and a restart re-reads them;
//! **`setsid(2)`**, so the unit outlives the pane the popup was drawn over — redirecting stdio alone
//! is measurably not enough, it is the shared session that kills; and stdio pointed at a log with
//! stdin from `/dev/null`, so a dev server never wedges a Herdr pipe or takes SIGPIPE from a dying
//! server.
//!
//! A `tty` unit swaps the last of those three for a terminal of its own and keeps the other two: the
//! session is now the terminal's, so it is still nobody else's, and the log is fed from the master end
//! rather than written to directly. Everything downstream of the log is unchanged by the swap.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::SystemTime;

use crate::installed;
use crate::terminal::{self, Terminal};

/// Most likely first: see `crate::installed`.
const MISE: [&str; 4] = [
    "~/.local/bin/mise",
    "/opt/homebrew/bin/mise",
    "/usr/local/bin/mise",
    "/usr/bin/mise",
];
const DIRENV: [&str; 5] = [
    "~/.local/bin/direnv",
    "~/.local/share/mise/shims/direnv",
    "/opt/homebrew/bin/direnv",
    "/usr/local/bin/direnv",
    "/usr/bin/direnv",
];

/// A missing toolchain must fail fast into the log rather than hang on a silent download.
const NO_AUTO_INSTALL: (&str, &str) = ("MISE_EXEC_AUTO_INSTALL", "false");

/// A unit's env is cleared and rebuilt at spawn, so the terminal type of a `tty` unit is decided here
/// rather than inherited from whatever happened to start the daemon. A manifest may still say
/// otherwise: this sits under its `env` rather than over it.
const TERM: (&str, &str) = ("TERM", "xterm-256color");

/// The env here is the manifest's two layers only; the process layer under it is what `crate::env`
/// lets through of the daemon's own, added at spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub name: String,
    pub cmd: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub tty: bool,
}

#[derive(Debug)]
pub struct Spawned {
    pub child: Child,
    pub pid: u32,
    /// The moment of the fork, at full resolution — not `ps -o lstart=`'s one second.
    pub started_at: SystemTime,
    pub ps_start: Option<String>,
    pub terminal: Option<Arc<Terminal>>,
}

/// What turns a unit's directory into the env a shell opened there would have. Each is used when it is
/// installed, whether or not the repo has a file for it: neither does anything to a directory it has
/// no config for, and a global mise config applies everywhere.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaders {
    pub mise: Option<PathBuf>,
    pub direnv: Option<PathBuf>,
}

impl Loaders {
    /// Looked up at every spawn, so installing one takes effect on the next restart.
    pub fn installed() -> Loaders {
        Loaders {
            mise: installed::first(&MISE),
            direnv: installed::first(&DIRENV),
        }
    }
}

pub fn mise_path() -> Option<PathBuf> {
    installed::first(&MISE)
}

pub fn direnv_path() -> Option<PathBuf> {
    installed::first(&DIRENV)
}

/// Everything about the spawn except the fork, so the recipe can be read off a `Command`. Env layers
/// innermost last: `base` — the machine-level part of the daemon's own, `crate::env` for why only
/// that part — then the manifest's, then ours.
pub fn command<I>(spec: &Spec, loaders: &Loaders, base: I) -> Command
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut argv: Vec<OsString> = Vec::new();
    // mise outermost: an `.envrc` is a script, and `layout ruby` or `use node` in it needs the repo's
    // toolchain already on `PATH` when it runs.
    if let Some(mise) = &loaders.mise {
        argv.extend([mise.into(), "exec".into(), "--".into()]);
    }
    if let Some(direnv) = &loaders.direnv {
        argv.extend([direnv.into(), "exec".into(), spec.cwd.clone().into()]);
    }
    argv.extend(spec.cmd.iter().map(OsString::from));
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command.current_dir(&spec.cwd);
    command.env_clear();
    command.envs(base);
    if spec.tty {
        command.env(TERM.0, TERM.1);
    }
    command.envs(&spec.env);
    command.env(NO_AUTO_INSTALL.0, NO_AUTO_INSTALL.1);
    command
}

/// The caller owns the log's rotation. A `tty` unit never writes to `log` itself; the thread reading
/// its terminal does.
pub fn spawn(spec: &Spec, log: File) -> std::io::Result<Spawned> {
    let mut command = command(spec, &Loaders::installed(), crate::env::machine_level());
    command.stdin(Stdio::null());
    let pty = match spec.tty {
        false => {
            on_pipes(&mut command, &log)?;
            None
        }
        // `login_tty(3)` puts the child in a session of its own as `setsid(2)` would, and the terminal
        // it takes over is that session's rather than anyone else's.
        true => {
            let pair = terminal::open()?;
            command.stdout(Stdio::null()).stderr(Stdio::null());
            unsafe { command.pre_exec(terminal::take_over(&pair.slave)) };
            Some(pair)
        }
    };

    let started_at = SystemTime::now();
    let child = command.spawn()?;
    let pid = child.id();
    // The child holds its own copy now, and ours would keep the master from ever reading the end.
    let terminal = pty.map(|pair| {
        drop(pair.slave);
        terminal::pump(Arc::clone(&pair.terminal), log);
        pair.terminal
    });
    Ok(Spawned {
        child,
        pid,
        started_at,
        ps_start: ps_field(pid, "lstart"),
        terminal,
    })
}

/// `setsid(2)` leaves the child leading a new session *and* a new process group, which is what a group
/// signal later needs. `Command::process_group(0)` would make it a group leader first and then
/// `setsid` returns EPERM — measured — so the two cannot be combined.
fn on_pipes(command: &mut Command, log: &File) -> std::io::Result<()> {
    command
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?));
    unsafe {
        command.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    Ok(())
}

pub fn term_group(pid: u32) -> std::io::Result<()> {
    signal_group(pid, libc::SIGTERM)
}

pub fn kill_group(pid: u32) -> std::io::Result<()> {
    signal_group(pid, libc::SIGKILL)
}

/// Measured: `TERM` to a `bin/vite dev` wrapper killed the wrapper and left both its children
/// running, orphaned.
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

/// Whether the pid a record names is still the process that record was written for. The one surviving
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

/// Stopping is not done when the wrapper is reaped — it is done when the tree it forked is gone, which
/// is the whole hazard the group signal exists for. macOS documents its own `ps -g` as ignored, so the
/// whole table is the only way to ask.
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
    use std::path::Path;

    fn only_mise() -> Loaders {
        Loaders {
            mise: Some(PathBuf::from("/opt/mise")),
            direnv: None,
        }
    }

    fn argv_of(command: &Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn spec() -> Spec {
        Spec {
            name: "vite".into(),
            cmd: vec!["bin/vite".into(), "dev".into()],
            cwd: PathBuf::from("/repos/storefront"),
            env: BTreeMap::from([("VITE_RUBY_HOST".to_string(), "127.0.0.1".to_string())]),
            tty: false,
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
        command(&spec(), &only_mise(), base)
    }

    fn built_on_a_terminal(base: &[(&str, &str)], env: &[(&str, &str)]) -> Command {
        let mut spec = spec();
        spec.tty = true;
        spec.env = env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let base = base
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()));
        command(&spec, &only_mise(), base)
    }

    #[test]
    fn a_tty_unit_is_told_what_terminal_it_is_on_rather_than_inheriting_one() {
        let env = env_of(&built_on_a_terminal(&[("TERM", "dumb")], &[]));
        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-256color"));
        // A unit on pipes has no terminal to name, so nothing is decided for it.
        assert_eq!(
            env_of(&built(&[("TERM", "dumb")]))
                .get("TERM")
                .map(String::as_str),
            Some("dumb")
        );
    }

    #[test]
    fn a_manifest_that_names_a_terminal_type_itself_is_not_overruled() {
        let env = env_of(&built_on_a_terminal(&[], &[("TERM", "xterm")]));
        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm"));
    }

    #[test]
    fn a_unit_runs_through_mise_exec_from_its_own_directory() {
        let command = built(&[]);
        assert_eq!(
            argv_of(&command),
            ["/opt/mise", "exec", "--", "bin/vite", "dev"]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/repos/storefront")));
    }

    #[test]
    fn direnv_loads_the_units_own_directory_inside_mise() {
        let loaders = Loaders {
            mise: Some(PathBuf::from("/opt/mise")),
            direnv: Some(PathBuf::from("/opt/direnv")),
        };
        assert_eq!(
            argv_of(&command(&spec(), &loaders, [])),
            [
                "/opt/mise",
                "exec",
                "--",
                "/opt/direnv",
                "exec",
                "/repos/storefront",
                "bin/vite",
                "dev"
            ]
        );
        let direnv_alone = Loaders {
            mise: None,
            direnv: Some(PathBuf::from("/opt/direnv")),
        };
        assert_eq!(
            argv_of(&command(&spec(), &direnv_alone, [])),
            ["/opt/direnv", "exec", "/repos/storefront", "bin/vite", "dev"]
        );
    }

    #[test]
    fn with_no_loader_installed_a_unit_runs_its_own_argv() {
        let command = command(&spec(), &Loaders::default(), []);
        assert_eq!(argv_of(&command), ["bin/vite", "dev"]);
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
