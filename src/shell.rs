//! A unit runs in the user's own shell. Started in the unit's directory, the shell reads the user's
//! config and runs its prompt hooks once, so whatever mise, direnv or anything else hooked into the
//! prompt would export there is exported, and then it `exec`s the unit's argv. Nothing here knows
//! which tools those are. A restart is a fresh shell, so it gets whatever the hooks say now.
//!
//! The hooks have to be run by hand: they fire when a prompt is drawn, and a shell given `-c` never
//! draws one. Each shell keeps them somewhere different, which is the one thing told apart here.
//!
//! It is a login shell started on `FRESH_PATH`, as a new terminal is, whatever Herdr's `shell_mode`
//! says about panes. The daemon's own `PATH` is whatever the pane that first ran a verb had, often
//! with another directory's tools already activated in it, and mise leaves a path already present
//! where it found it: a shim from that `PATH` would then shadow the toolchain this directory names.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use toml_edit::DocumentMut;

use crate::herdr::expand_tilde;

/// Herdr's own config, read for the shell its panes run, so a unit gets the shell a new pane would.
const HERDR_CONFIG: &str = ".config/herdr/config.toml";
const FALLBACK: &str = "/bin/sh";
/// What a shell given `-c` calls itself: `$0` to zsh and bash, which put the argv after it.
const NAME: &str = "herdr-stack";

/// What a fresh session starts from before the shell's login config builds the rest.
pub const FRESH_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub program: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Fish,
    Zsh,
    Bash,
    Other,
}

impl Shell {
    /// The shell a new Herdr pane would get: Herdr's `default_shell`, then `$SHELL`, then `/bin/sh`.
    /// Read from `env` rather than the process, because a daemon's own env is not the user's, and
    /// found on that `PATH` here because the unit's own will not have it.
    pub fn of(env: &[(String, String)]) -> Shell {
        let var = |key: &str| {
            env.iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
                .filter(|value| !value.is_empty())
        };
        let default_shell = var("HOME")
            .and_then(|home| std::fs::read_to_string(Path::new(home).join(HERDR_CONFIG)).ok())
            .and_then(|text| text.parse::<DocumentMut>().ok())
            .and_then(|doc| {
                doc.get("terminal")?
                    .get("default_shell")?
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .map(expand_tilde)
            });
        let program = default_shell
            .or_else(|| var("SHELL").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(FALLBACK));
        Shell {
            program: located(program, var("PATH").unwrap_or_default()),
        }
    }

    pub fn new(program: impl Into<PathBuf>) -> Shell {
        Shell {
            program: program.into(),
        }
    }

    fn family(&self) -> Family {
        match self
            .program
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
        {
            "fish" => Family::Fish,
            "zsh" => Family::Zsh,
            "bash" => Family::Bash,
            _ => Family::Other,
        }
    }

    /// The shell, told to run the prompt hooks and then become `argv`. A unit on pipes is handed
    /// `log`, appended to by path; `unset_term` drops a `TERM` the shell was given only so that it
    /// would start quietly.
    pub fn command(&self, argv: &[String], log: Option<&Path>, unset_term: bool) -> Command {
        let family = self.family();
        let mut command = Command::new(&self.program);
        if family != Family::Other {
            // Interactive as well, because that is when zsh and bash read the rc files the hooks
            // live in.
            command.arg("-l").arg("-i");
        }
        command
            .arg("-c")
            .arg(script(family, log.is_some(), unset_term));
        if family != Family::Fish {
            command.arg(NAME);
        }
        command.args(log);
        command.args(argv.iter().map(OsString::from));
        command
    }
}

fn located(program: PathBuf, path: &str) -> PathBuf {
    if program.components().count() > 1 {
        return program;
    }
    path.split(':')
        .map(|dir| Path::new(dir).join(&program))
        .find(|candidate| candidate.is_file())
        .unwrap_or(program)
}

/// The hooks' stdout goes nowhere: it is a terminal's business, and the unit's log is not one. Their
/// stderr joins the log, so `direnv: loading .envrc` lands at the top of it where it explains the env.
///
/// A unit on pipes reopens its log by path, the first argument after `$0`, rather than inheriting a
/// descriptor: macOS's bash 3.2 closes every one above stderr when it starts as an interactive login
/// shell.
fn script(family: Family, piped: bool, unset_term: bool) -> String {
    let (hooks, unset, exec) = match family {
        // `fish_greeting` and the first prompt's terminal setup both hang off `fish_prompt`.
        Family::Fish => (
            "set -g fish_greeting; emit fish_prompt >/dev/null",
            "set -e TERM; ",
            "exec $argv",
        ),
        Family::Zsh => (
            "for hook in $precmd_functions; do $hook; done >/dev/null",
            "unset TERM; ",
            "exec \"$@\"",
        ),
        Family::Bash => (
            "for hook in \"${PROMPT_COMMAND[@]}\"; do eval \"$hook\"; done >/dev/null",
            "unset TERM; ",
            "exec \"$@\"",
        ),
        Family::Other => ("", "unset TERM; ", "exec \"$@\""),
    };
    let (take_log, hooks_err, exec_out) = match (piped, family) {
        (false, _) => ("", "", ""),
        (true, Family::Fish) => (
            "set -l herdr_stack_log $argv[1]; set -e argv[1]; ",
            " 2>>$herdr_stack_log",
            " >>$herdr_stack_log 2>&1",
        ),
        (true, _) => (
            "herdr_stack_log=$1; shift; ",
            " 2>>\"$herdr_stack_log\"",
            " >>\"$herdr_stack_log\" 2>&1",
        ),
    };
    let hooks = match hooks {
        "" => String::new(),
        hooks => format!("{hooks}{hooks_err}; "),
    };
    let unset = if unset_term { unset } else { "" };
    format!("{take_log}{hooks}{unset}{exec}{exec_out}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv_of(command: &Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn unit() -> Vec<String> {
        vec!["bin/vite".into(), "dev".into()]
    }

    fn home_with(name: &str, config: Option<&str>) -> PathBuf {
        let home =
            std::env::temp_dir().join(format!("herdr-stack-shell-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".config/herdr")).expect("home");
        if let Some(config) = config {
            std::fs::write(home.join(HERDR_CONFIG), config).expect("config");
        }
        home
    }

    fn env(home: &Path, shell: Option<&str>) -> Vec<(String, String)> {
        let mut env = vec![("HOME".to_string(), home.to_string_lossy().into_owned())];
        if let Some(shell) = shell {
            env.push(("SHELL".to_string(), shell.to_string()));
        }
        env
    }

    #[test]
    fn a_unit_gets_the_shell_a_new_herdr_pane_would() {
        let home = home_with("herdr", Some("[terminal]\ndefault_shell = \"/opt/fish\"\n"));
        let shell = Shell::of(&env(&home, Some("/bin/zsh")));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(shell, Shell::new("/opt/fish"));
    }

    #[test]
    fn without_a_herdr_shell_it_is_the_users_and_then_sh() {
        let home = home_with("users", Some("[terminal]\nshell_mode = \"non_login\"\n"));
        let users = Shell::of(&env(&home, Some("/bin/zsh")));
        let none = Shell::of(&env(&home, None));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(users, Shell::new("/bin/zsh"));
        assert_eq!(none, Shell::new(FALLBACK));
    }

    /// The unit's shell starts on `FRESH_PATH`, which would not find a shell named bare.
    #[test]
    fn a_shell_named_bare_is_found_on_the_daemons_path() {
        let home = home_with("bare", Some("[terminal]\ndefault_shell = \"sh\"\n"));
        let mut env = env(&home, None);
        env.push(("PATH".to_string(), "/nowhere:/bin:/usr/bin".to_string()));
        let shell = Shell::of(&env);
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(shell, Shell::new("/bin/sh"));
    }

    #[test]
    fn each_shell_runs_its_own_prompt_hooks_before_becoming_the_unit() {
        let fish = argv_of(&Shell::new("/opt/homebrew/bin/fish").command(&unit(), None, false));
        assert_eq!(
            fish,
            [
                "/opt/homebrew/bin/fish",
                "-l",
                "-i",
                "-c",
                "set -g fish_greeting; emit fish_prompt >/dev/null; exec $argv",
                "bin/vite",
                "dev"
            ]
        );

        let zsh = argv_of(&Shell::new("/bin/zsh").command(&unit(), None, false));
        assert_eq!(&zsh[..4], ["/bin/zsh", "-l", "-i", "-c"]);
        assert!(zsh[4].contains("$precmd_functions"), "{}", zsh[4]);
        assert_eq!(&zsh[5..], [NAME, "bin/vite", "dev"]);

        let bash = argv_of(&Shell::new("/bin/bash").command(&unit(), None, false));
        assert!(bash[4].contains("PROMPT_COMMAND"), "{}", bash[4]);
        assert_eq!(&bash[5..], [NAME, "bin/vite", "dev"]);
    }

    #[test]
    fn a_shell_with_no_known_hooks_just_becomes_the_unit() {
        assert_eq!(
            argv_of(&Shell::new("/bin/sh").command(&unit(), Some(Path::new("/state/log")), true)),
            [
                "/bin/sh",
                "-c",
                "herdr_stack_log=$1; shift; unset TERM; exec \"$@\" >>\"$herdr_stack_log\" 2>&1",
                NAME,
                "/state/log",
                "bin/vite",
                "dev"
            ]
        );
    }
}
