//! A unit runs in the user's own shell. Started in the unit's directory, the shell reads the user's
//! config and runs its prompt hooks once, so whatever mise, direnv or anything else hooked into the
//! prompt would export there is exported, and then it `exec`s the unit's argv. Nothing here knows
//! which tools those are. A restart is a fresh shell, so it gets whatever the hooks say now.
//!
//! The hooks have to be run by hand: they fire when a prompt is drawn, and a shell given `-c` never
//! draws one. Each shell keeps them somewhere different, which is the one thing told apart here.

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

/// Where a unit on pipes finds its log: the shell's own stdout and stderr go nowhere, and only the unit
/// it becomes writes here.
pub const LOG_FD: i32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub program: PathBuf,
    pub login: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Fish,
    Zsh,
    Bash,
    Other,
}

impl Shell {
    /// The shell a new Herdr pane would get: Herdr's `default_shell`, then `$SHELL`, then `/bin/sh`,
    /// with Herdr's `shell_mode` deciding whether it is a login shell. Read from `env` rather than the
    /// process, because a daemon's own env is not the user's.
    pub fn of(env: &[(String, String)]) -> Shell {
        let var = |key: &str| {
            env.iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
                .filter(|value| !value.is_empty())
        };
        let terminal = var("HOME")
            .and_then(|home| std::fs::read_to_string(Path::new(home).join(HERDR_CONFIG)).ok())
            .and_then(|text| text.parse::<DocumentMut>().ok())
            .and_then(|doc| doc.get("terminal").cloned());
        let setting = |key: &str| {
            terminal
                .as_ref()
                .and_then(|terminal| terminal.get(key))
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let program = setting("default_shell")
            .map(expand_tilde)
            .or_else(|| var("SHELL").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(FALLBACK));
        let login = match setting("shell_mode").as_deref() {
            Some("login") => true,
            Some("non_login") => false,
            // Herdr's `auto`: login shells on macOS, where `/etc/zprofile` is what puts Homebrew on
            // `PATH`, and not elsewhere.
            _ => cfg!(target_os = "macos"),
        };
        Shell { program, login }
    }

    pub fn plain(program: impl Into<PathBuf>) -> Shell {
        Shell {
            program: program.into(),
            login: false,
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

    /// The shell, told to run the prompt hooks and then become `argv`. `piped` hands the unit its log
    /// from `LOG_FD`; `unset_term` drops a `TERM` the shell was given only so that it would start
    /// quietly.
    pub fn command(&self, argv: &[String], piped: bool, unset_term: bool) -> Command {
        let family = self.family();
        let mut command = Command::new(&self.program);
        if family != Family::Other {
            if self.login {
                command.arg("-l");
            }
            // Interactive, because that is when zsh and bash read the rc files the hooks live in.
            command.arg("-i");
        }
        command.arg("-c").arg(script(family, piped, unset_term));
        if family != Family::Fish {
            command.arg(NAME);
        }
        command.args(argv.iter().map(OsString::from));
        command
    }
}

/// The hooks' stdout goes nowhere: it is a terminal's business, and the unit's log is not one. Their
/// stderr joins the log, so `direnv: loading .envrc` lands at the top of it where it explains the env.
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
    let fd = LOG_FD;
    let (hooks_err, exec_out) = match piped {
        true => (format!(" 2>&{fd}"), format!(" >&{fd} 2>&{fd} {fd}>&-")),
        false => (String::new(), String::new()),
    };
    let hooks = match hooks {
        "" => String::new(),
        hooks => format!("{hooks}{hooks_err}; "),
    };
    let unset = if unset_term { unset } else { "" };
    format!("{hooks}{unset}{exec}{exec_out}")
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

    fn home_with(config: Option<&str>) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "herdr-stack-shell-{}-{}",
            std::process::id(),
            config.map_or(0, str::len)
        ));
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
        let home = home_with(Some(
            "[terminal]\ndefault_shell = \"/opt/fish\"\nshell_mode = \"login\"\n",
        ));
        let shell = Shell::of(&env(&home, Some("/bin/zsh")));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(
            shell,
            Shell {
                program: PathBuf::from("/opt/fish"),
                login: true
            }
        );
    }

    #[test]
    fn without_a_herdr_shell_it_is_the_users_and_then_sh() {
        let home = home_with(Some("[terminal]\nshell_mode = \"non_login\"\n"));
        let users = Shell::of(&env(&home, Some("/bin/zsh")));
        let none = Shell::of(&env(&home, None));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(users, Shell::plain("/bin/zsh"));
        assert_eq!(none, Shell::plain(FALLBACK));
    }

    #[test]
    fn auto_mode_is_a_login_shell_on_macos_only() {
        let home = home_with(None);
        let shell = Shell::of(&env(&home, Some("/bin/zsh")));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(shell.login, cfg!(target_os = "macos"));
    }

    #[test]
    fn each_shell_runs_its_own_prompt_hooks_before_becoming_the_unit() {
        let fish = argv_of(&Shell::plain("/opt/homebrew/bin/fish").command(&unit(), false, false));
        assert_eq!(
            fish,
            [
                "/opt/homebrew/bin/fish",
                "-i",
                "-c",
                "set -g fish_greeting; emit fish_prompt >/dev/null; exec $argv",
                "bin/vite",
                "dev"
            ]
        );

        let zsh = argv_of(&Shell::plain("/bin/zsh").command(&unit(), false, false));
        assert_eq!(&zsh[..3], ["/bin/zsh", "-i", "-c"]);
        assert!(zsh[3].contains("$precmd_functions"), "{}", zsh[3]);
        assert_eq!(&zsh[4..], [NAME, "bin/vite", "dev"]);

        let bash = argv_of(&Shell::plain("/bin/bash").command(&unit(), false, false));
        assert!(bash[3].contains("PROMPT_COMMAND"), "{}", bash[3]);
        assert_eq!(&bash[4..], [NAME, "bin/vite", "dev"]);
    }

    #[test]
    fn a_login_shell_is_asked_for_as_one() {
        let shell = Shell {
            program: PathBuf::from("/bin/zsh"),
            login: true,
        };
        assert_eq!(
            &argv_of(&shell.command(&unit(), false, false))[..3],
            ["/bin/zsh", "-l", "-i"]
        );
    }

    #[test]
    fn a_shell_with_no_known_hooks_just_becomes_the_unit() {
        assert_eq!(
            argv_of(&Shell::plain("/bin/sh").command(&unit(), true, true)),
            [
                "/bin/sh",
                "-c",
                "unset TERM; exec \"$@\" >&3 2>&3 3>&-",
                NAME,
                "bin/vite",
                "dev"
            ]
        );
    }
}
