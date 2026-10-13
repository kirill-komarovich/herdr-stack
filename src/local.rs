//! The spawn recipe and the session signals, and nothing about who called for them.
//!
//! Three parts of the spawn are load-bearing and none of them is optional: the user's own shell with
//! cwd set, running its prompt hooks before it becomes the unit (`crate::shell`), so the toolchain and
//! every port come from whatever the repo's directory exports rather than the manifest, and a restart
//! re-reads them; **`setsid(2)`**, so the unit outlives the pane the popup was drawn over — redirecting
//! stdio alone is measurably not enough, it is the shared session that kills; and stdio pointed at a
//! log with stdin from `/dev/null`, so a dev server never wedges a Herdr pipe or takes SIGPIPE from a
//! dying server.
//!
//! A `tty` unit swaps the last of those three for a terminal of its own and keeps the other two: the
//! session is now the terminal's, so it is still nobody else's, and the log is fed from the master end
//! rather than written to directly. Everything downstream of the log is unchanged by the swap.

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::SystemTime;

use crate::shell::{self, Shell};
use crate::terminal::{self, Terminal};

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

/// Everything about the spawn except the fork, so the recipe can be read off a `Command`. Env layers
/// innermost last: `base` — the machine-level part of the daemon's own, `crate::env` for why only
/// that part — with `PATH` replaced by a fresh one, then the manifest's, then whatever the shell's
/// config and hooks export in the unit's directory.
pub fn command<I>(spec: &Spec, shell: &Shell, log: &Path, base: I) -> Command
where
    I: IntoIterator<Item = (String, String)>,
{
    let quiet = !spec.tty && !spec.env.contains_key(shell::QUIET_TERM.0);
    let piped_log = (!spec.tty).then_some(log);
    let mut command = shell.command(&spec.cmd, piped_log, quiet);
    command.current_dir(&spec.cwd);
    command.env_clear();
    command.envs(base);
    command.env("PATH", shell::FRESH_PATH);
    if spec.tty {
        command.env(TERM.0, TERM.1);
    } else if quiet {
        command.env(shell::QUIET_TERM.0, shell::QUIET_TERM.1);
    }
    command.envs(&spec.env);
    command
}

/// The caller owns the log's rotation. A `tty` unit never writes to `log` itself; the thread reading
/// its terminal does.
pub fn spawn(spec: &Spec, shell: &Shell, log: File, log_path: &Path) -> std::io::Result<Spawned> {
    let mut command = command(spec, shell, log_path, crate::env::machine_level());
    command.stdin(Stdio::null());
    let pty = match spec.tty {
        false => {
            on_pipes(&mut command);
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

/// `setsid(2)` leaves the child leading a new session *and* a new process group, which is what a session
/// signal later needs. `Command::process_group(0)` would make it a group leader first and then
/// `setsid` returns EPERM — measured — so the two cannot be combined.
///
/// The shell itself writes to `/dev/null`: an interactive shell with no terminal complains on the way
/// up, and fish writes its terminal setup straight to stdout whatever it is redirected to. Only the
/// unit it becomes writes to the log, which the script opens by path.
fn on_pipes(command: &mut Command) {
    command.stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
}

pub fn term_session(sid: u32) -> std::io::Result<()> {
    signal_session(sid, libc::SIGTERM)
}

pub fn kill_session(sid: u32) -> std::io::Result<()> {
    signal_session(sid, libc::SIGKILL)
}

/// A unit is everything in the session `setsid(2)` gave it, not its process group: pnpm starts vite in
/// a group of its own, which a signal to the unit's group never reaches. Measured as well: `TERM` to a
/// `bin/vite dev` wrapper alone killed the wrapper and left both its children running, orphaned.
///
/// Every group in the session is signalled whole, which also reaches a child forked since the table
/// was read; a group never spans two sessions, so none of them is anyone else's.
fn signal_session(sid: u32, signal: i32) -> std::io::Result<()> {
    let mut groups: Vec<u32> = session_members(sid)?
        .iter()
        .map(|member| member.group)
        .collect();
    groups.push(sid);
    groups.sort_unstable();
    groups.dedup();
    for group in groups {
        unsafe { libc::killpg(group as i32, signal) };
    }
    Ok(())
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
        (Some(recorded), Some(current)) => recorded == current && session_of(pid) == Some(pid),
        // Without a recorded start time a pid says nothing, and killing a group on a guess is worse
        // than leaving a unit behind.
        _ => false,
    }
}

/// The session a process is in. A unit is spawned with `setsid(2)`, so anything that does not lead its
/// own session is not one of ours and must not be signalled as one.
pub fn session_of(pid: u32) -> Option<u32> {
    match unsafe { libc::getsid(pid as i32) } {
        -1 => None,
        sid => Some(sid as u32),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Member {
    pid: u32,
    group: u32,
}

/// Stopping is not done when the wrapper is reaped — it is done when the tree it forked is gone, which
/// is the whole hazard the session signal exists for. Neither `ps` lists a session's members (macOS's
/// `sess` column reads 0), so the table is read whole and each pid's session asked of the kernel.
pub fn session_empty(sid: u32) -> bool {
    // Unanswerable is not the same as empty: escalating on a failed `ps` is the safer half.
    session_members(sid).is_ok_and(|members| members.is_empty())
}

fn session_members(sid: u32) -> std::io::Result<Vec<Member>> {
    let output = Command::new("/bin/ps")
        .arg("-ax")
        .arg("-o")
        .arg("pid=,pgid=,stat=")
        .output()?;
    Ok(live_members(
        &String::from_utf8_lossy(&output.stdout),
        sid,
        session_of,
    ))
}

/// A zombie is not counted: it runs nothing and no signal reaches it, and where nothing reaps orphans
/// — a container whose init is not one — it would hold every stop to the full grace and escalation.
fn live_members(table: &str, sid: u32, session_of: impl Fn(u32) -> Option<u32>) -> Vec<Member> {
    table
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let pid = columns.next()?.parse().ok()?;
            let group = columns.next()?.parse().ok()?;
            let stat = columns.next().unwrap_or_default();
            (!stat.starts_with('Z')).then_some(Member { pid, group })
        })
        .filter(|member| session_of(member.pid) == Some(sid))
        .collect()
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

    fn bash() -> Shell {
        Shell::new("/bin/bash")
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
        command(&spec(), &bash(), Path::new("/state/log"), base)
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
        command(&spec, &bash(), Path::new("/state/log"), base)
    }

    #[test]
    fn a_tty_unit_is_told_what_terminal_it_is_on_rather_than_inheriting_one() {
        let command = built_on_a_terminal(&[("TERM", "vt100")], &[]);
        assert_eq!(
            env_of(&command).get("TERM").map(String::as_str),
            Some("xterm-256color")
        );
        assert!(!argv_of(&command)[4].contains("unset TERM"));
    }

    #[test]
    fn a_manifest_that_names_a_terminal_type_itself_is_not_overruled() {
        let command = built_on_a_terminal(&[], &[("TERM", "xterm")]);
        assert_eq!(
            env_of(&command).get("TERM").map(String::as_str),
            Some("xterm")
        );
        assert!(!argv_of(&command)[4].contains("unset TERM"));
    }

    #[test]
    fn a_unit_runs_in_the_shell_from_its_own_directory_and_becomes_its_argv() {
        let command = built(&[]);
        let argv = argv_of(&command);
        assert_eq!(&argv[..4], ["/bin/bash", "-l", "-i", "-c"]);
        // The hooks' complaints and then the unit itself write to the log; the shell never does.
        assert!(
            argv[4].ends_with("exec \"$@\" >>\"$herdr_stack_log\" 2>&1"),
            "{}",
            argv[4]
        );
        assert_eq!(&argv[6..], ["/state/log", "bin/vite", "dev"]);
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new("/repos/storefront"))
        );
    }

    #[test]
    fn a_unit_on_pipes_is_given_a_term_only_for_the_shell_to_start_quietly() {
        let command = built(&[]);
        assert_eq!(
            env_of(&command).get("TERM").map(String::as_str),
            Some("dumb")
        );
        assert!(argv_of(&command)[4].contains("unset TERM"));
    }

    #[test]
    fn the_daemons_path_never_reaches_the_units_shell() {
        let env = env_of(&built(&[("PATH", "/elsewhere/mise/shims:/usr/bin")]));
        assert_eq!(env.get("PATH").map(String::as_str), Some(shell::FRESH_PATH));
    }

    #[test]
    fn the_units_env_is_layered_over_the_daemons_own_and_wins() {
        let env = env_of(&built(&[
            ("LANG", "en_US.UTF-8"),
            ("VITE_RUBY_HOST", "from-the-daemon"),
        ]));
        assert_eq!(env.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
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
    fn a_session_counts_its_members_in_any_group_and_no_zombie() {
        let table = "  101   100 S\n  102   102 S+\n  103   100 Z\n  104   200 S\n";
        let sessions = |pid| match pid {
            101..=103 => Some(100),
            _ => Some(200),
        };
        assert_eq!(
            live_members(table, 100, sessions),
            [
                Member {
                    pid: 101,
                    group: 100
                },
                Member {
                    pid: 102,
                    group: 102
                }
            ]
        );
        assert!(live_members(table, 300, sessions).is_empty());
    }

    #[test]
    fn a_session_with_a_member_is_not_empty_and_one_that_never_existed_is() {
        let session = session_of(std::process::id()).expect("this process has a session");
        assert!(!session_empty(session));
        // No session can be numbered near the top of a u32: pids are bounded far below it.
        assert!(session_empty(u32::MAX - 1));
    }

    #[test]
    fn a_pid_with_no_recorded_start_time_is_never_treated_as_a_leftover() {
        assert!(!still_running(std::process::id(), None));
    }
}
