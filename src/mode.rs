use std::fmt;
use std::path::PathBuf;

use crate::state;

pub const USAGE: &str = "usage: herdr-dev [daemon [--state-root <dir>]|tail]";

const STATE_ROOT: &str = "--state-root";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Tui,
    /// The root is spelled out in `state`; naming another one is for tests and for driving a daemon
    /// by hand, and the TUI never passes it.
    Daemon {
        root: PathBuf,
    },
    /// The overlay pane's mode. The log to follow arrives in the environment rather than in argv,
    /// because the manifest declares one entrypoint for every unit.
    Tail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeError {
    UnknownMode(String),
    UnexpectedArgument(String),
    MissingValue(String),
}

impl fmt::Display for ModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModeError::UnknownMode(arg) => write!(f, "unknown mode `{arg}`"),
            ModeError::UnexpectedArgument(arg) => write!(f, "unexpected argument `{arg}`"),
            ModeError::MissingValue(flag) => write!(f, "`{flag}` needs a directory"),
        }
    }
}

/// `args` excludes argv[0].
pub fn from_args<I, S>(args: I) -> Result<Mode, ModeError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args = args.into_iter();
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
            other => return Err(ModeError::UnknownMode(other.to_string())),
        },
    };
    if let Some(extra) = args.next() {
        return Err(ModeError::UnexpectedArgument(extra.as_ref().to_string()));
    }
    Ok(mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_is_the_tui() {
        assert_eq!(from_args(Vec::<String>::new()), Ok(Mode::Tui));
    }

    #[test]
    fn daemon_and_tail_are_their_own_modes() {
        assert_eq!(
            from_args(["daemon"]),
            Ok(Mode::Daemon {
                root: state::root()
            })
        );
        assert_eq!(from_args(["tail"]), Ok(Mode::Tail));
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
