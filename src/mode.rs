use std::fmt;
use std::path::PathBuf;

use crate::cli::{Act, Ask};
use crate::state;

pub const USAGE: &str = "usage: herdr-dev [daemon [--state-root <dir>]|tail|attach]
       herdr-dev status [--project <dir>]
       herdr-dev start|stop|restart [--project <dir>] <unit>...";

const STATE_ROOT: &str = "--state-root";
const PROJECT: &str = "--project";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Tui,
    /// The root is spelled out in `state`; naming another one is for tests and for driving a daemon
    /// by hand, and the TUI never passes it.
    Daemon {
        root: PathBuf,
    },
    /// The log pane's mode. The log to follow arrives in the environment rather than in argv,
    /// because the manifest declares one entrypoint for every unit.
    Tail,
    /// The attach pane's mode. Which unit to type at arrives in the environment for the same reason.
    Attach,
    /// The agent-facing modes. A project is named here or taken from the working directory — never
    /// from Herdr's focused pane, which a caller with no session has no relationship to.
    Status {
        ask: Ask,
    },
    Control {
        act: Act,
        ask: Ask,
        units: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeError {
    UnknownMode(String),
    UnexpectedArgument(String),
    MissingValue(String),
    MissingUnit(String),
}

impl fmt::Display for ModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModeError::UnknownMode(arg) => write!(f, "unknown mode `{arg}`"),
            ModeError::UnexpectedArgument(arg) => write!(f, "unexpected argument `{arg}`"),
            ModeError::MissingValue(flag) => write!(f, "`{flag}` needs a directory"),
            ModeError::MissingUnit(verb) => write!(f, "`{verb}` needs a unit to act on"),
        }
    }
}

/// `args` excludes argv[0].
pub fn from_args<I, S>(args: I) -> Result<Mode, ModeError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args = args.into_iter().peekable();
    let mode = match args.next() {
        None => Mode::Tui,
        Some(arg) => match arg.as_ref() {
            "daemon" => Mode::Daemon {
                root: match args.next() {
                    None => state::root(),
                    Some(flag) if flag.as_ref() == STATE_ROOT => match args.next() {
                        Some(dir) => PathBuf::from(dir.as_ref()),
                        None => return Err(ModeError::MissingValue(STATE_ROOT.into())),
                    },
                    Some(other) => {
                        return Err(ModeError::UnexpectedArgument(other.as_ref().to_string()));
                    }
                },
            },
            "tail" => Mode::Tail,
            "attach" => Mode::Attach,
            "status" => Mode::Status {
                ask: options(&mut args)?,
            },
            verb @ ("start" | "stop" | "restart") => {
                let act = match verb {
                    "start" => Act::Start,
                    "stop" => Act::Stop,
                    _ => Act::Restart,
                };
                let ask = options(&mut args)?;
                // Everything left is a unit, so a verb is the one mode that ends the argument scan
                // rather than falling through to the check for a stray word.
                let units: Vec<String> = args.map(|arg| arg.as_ref().to_string()).collect();
                if units.is_empty() {
                    return Err(ModeError::MissingUnit(verb.to_string()));
                }
                return Ok(Mode::Control { act, ask, units });
            }
            other => return Err(ModeError::UnknownMode(other.to_string())),
        },
    };
    if let Some(extra) = args.next() {
        return Err(ModeError::UnexpectedArgument(extra.as_ref().to_string()));
    }
    Ok(mode)
}

/// The flags an agent-facing mode takes, in any order and all optional. Only a flag is consumed:
/// what follows a verb is a unit name, and a scan that ate one would turn a typo into a silent
/// no-op.
fn options<I, S>(args: &mut std::iter::Peekable<I>) -> Result<Ask, ModeError>
where
    I: Iterator<Item = S>,
    S: AsRef<str>,
{
    let mut ask = Ask::default();
    while let Some(flag) = args.peek().map(|arg| arg.as_ref().to_string()) {
        let held = match flag.as_str() {
            PROJECT => &mut ask.project,
            STATE_ROOT => &mut ask.state,
            _ => break,
        };
        args.next();
        match args.next() {
            Some(dir) => *held = Some(PathBuf::from(dir.as_ref())),
            None => return Err(ModeError::MissingValue(flag)),
        }
    }
    Ok(ask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_is_the_tui() {
        assert_eq!(from_args(Vec::<String>::new()), Ok(Mode::Tui));
    }

    #[test]
    fn daemon_and_the_two_panes_are_their_own_modes() {
        assert_eq!(
            from_args(["daemon"]),
            Ok(Mode::Daemon {
                root: state::root()
            })
        );
        assert_eq!(from_args(["tail"]), Ok(Mode::Tail));
        assert_eq!(from_args(["attach"]), Ok(Mode::Attach));
    }

    #[test]
    fn a_daemon_may_be_pointed_at_another_state_root() {
        assert_eq!(
            from_args(["daemon", "--state-root", "/tmp/scratch"]),
            Ok(Mode::Daemon {
                root: PathBuf::from("/tmp/scratch")
            })
        );
        assert_eq!(
            from_args(["daemon", "--state-root"]),
            Err(ModeError::MissingValue("--state-root".into()))
        );
        assert_eq!(
            from_args(["daemon", "--root", "/tmp"]),
            Err(ModeError::UnexpectedArgument("--root".into()))
        );
    }

    fn ask(project: &str) -> Ask {
        Ask {
            project: Some(PathBuf::from(project)),
            state: None,
        }
    }

    #[test]
    fn status_takes_a_project_or_the_working_directory() {
        assert_eq!(
            from_args(["status"]),
            Ok(Mode::Status {
                ask: Ask::default()
            })
        );
        assert_eq!(
            from_args(["status", "--project", "/repos/storefront"]),
            Ok(Mode::Status {
                ask: ask("/repos/storefront")
            })
        );
    }

    #[test]
    fn a_verb_takes_its_flags_first_and_then_every_word_as_a_unit() {
        assert_eq!(
            from_args([
                "restart",
                "--project",
                "/repos/storefront",
                "rails",
                "docker-db"
            ]),
            Ok(Mode::Control {
                act: Act::Restart,
                ask: ask("/repos/storefront"),
                units: vec!["rails".to_string(), "docker-db".to_string()],
            })
        );
        assert_eq!(
            from_args(["start", "vite"]),
            Ok(Mode::Control {
                act: Act::Start,
                ask: Ask::default(),
                units: vec!["vite".to_string()],
            })
        );
    }

    #[test]
    fn a_verb_without_a_unit_says_so_rather_than_acting_on_everything() {
        assert_eq!(
            from_args(["stop"]),
            Err(ModeError::MissingUnit("stop".into()))
        );
        assert_eq!(
            from_args(["stop", "--project", "/repos/storefront"]),
            Err(ModeError::MissingUnit("stop".into()))
        );
    }

    #[test]
    fn a_flag_without_a_directory_is_not_allowed_to_eat_the_unit_after_it() {
        assert_eq!(
            from_args(["start", "--project"]),
            Err(ModeError::MissingValue(PROJECT.into()))
        );
        assert_eq!(
            from_args(["status", "--state-root"]),
            Err(ModeError::MissingValue(STATE_ROOT.into()))
        );
    }

    #[test]
    fn a_unit_named_like_nothing_we_parse_is_still_a_unit() {
        assert_eq!(
            from_args(["start", "-x"]),
            Ok(Mode::Control {
                act: Act::Start,
                ask: Ask::default(),
                units: vec!["-x".to_string()],
            })
        );
    }

    #[test]
    fn unknown_mode_names_the_argument() {
        let err = from_args(["tui"]).unwrap_err();
        assert_eq!(err, ModeError::UnknownMode("tui".into()));
        assert!(err.to_string().contains("`tui`"));
    }

    #[test]
    fn help_flag_is_not_a_mode() {
        assert!(from_args(["--help"]).is_err());
    }

    #[test]
    fn a_mode_takes_no_further_arguments() {
        assert_eq!(
            from_args(["daemon", "--state-root", "/tmp", "extra"]),
            Err(ModeError::UnexpectedArgument("extra".into()))
        );
        assert_eq!(
            from_args(["tail", "/tmp/x.log"]),
            Err(ModeError::UnexpectedArgument("/tmp/x.log".into()))
        );
    }
}
