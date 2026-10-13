//! What crosses the boundary into a process we start, and what must not.
//!
//! A daemon outlives the shell that started it and serves every project on the machine, so its own
//! environment is an accident of birth: whichever pane first ran a verb, carrying whatever that
//! directory's mise or direnv files had already exported into it. Handed down whole, one project's `PORT`
//! becomes the floor under every other project's units.
//!
//! Neither `mise exec` nor `direnv exec` can undo that. Each layers a project's config over what it is
//! handed and clears nothing it has no opinion about, so a var the project does not declare — by design, the caller's
//! business — arrives untouched. A project that declares no `PORT` therefore inherits a stranger's.
//!
//! So nothing project-scoped crosses. What survives describes the machine and the user, and every
//! project-scoped value reaches a unit the one way the spawn recipe intends: the env loaders run for
//! the unit's own cwd.

/// The machine and the user, never a project.
const KEEP: &[&str] = &[
    "HOME",
    "LANG",
    "LOGNAME",
    "PATH",
    "SHELL",
    "SSH_AUTH_SOCK",
    "TMPDIR",
    "USER",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
];

/// Locale is a family rather than a name, and the whole of it is the user's.
const KEEP_PREFIX: &[&str] = &["LC_"];

/// `TERM` is deliberately absent: a `tty` unit is told its terminal at spawn, and a unit on pipes has
/// none to name — inheriting one only makes it colour a log that then has to strip it. `HERDR_*` is
/// absent for the same reason as a project's vars: those name the pane that happened to call, and a
/// unit spawned from it is in no pane at all.
pub fn machine_level() -> Vec<(String, String)> {
    retain(std::env::vars())
}

pub fn retain<I>(env: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    env.into_iter().filter(|(key, _)| kept(key)).collect()
}

fn kept(key: &str) -> bool {
    KEEP.contains(&key) || KEEP_PREFIX.iter().any(|prefix| key.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retained(env: &[(&str, &str)]) -> Vec<String> {
        retain(
            env.iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        )
        .into_iter()
        .map(|(key, _)| key)
        .collect()
    }

    #[test]
    fn a_projects_own_vars_do_not_cross() {
        assert_eq!(
            retained(&[
                ("PORT", "3006"),
                ("VITE_RUBY_PORT", "3041"),
                ("RAILS_ENV", "development"),
                ("SSO_CLIENT_SECRET", "shh"),
            ]),
            Vec::<String>::new(),
            "the leak this module exists to stop"
        );
    }

    #[test]
    fn the_machine_and_the_user_do_cross() {
        assert_eq!(
            retained(&[
                ("HOME", "/Users/kirill"),
                ("PATH", "/usr/bin"),
                ("LC_ALL", "C")
            ]),
            ["HOME", "PATH", "LC_ALL"]
        );
    }

    #[test]
    fn the_pane_that_happened_to_call_does_not_cross() {
        assert_eq!(
            retained(&[
                ("HERDR_PANE_ID", "wA:pH"),
                ("HERDR_STARTUP_CWD", "/Users/kirill"),
                ("TERM", "ghostty"),
                ("PWD", "/repos/backoffice"),
            ]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_value_is_carried_across_whole() {
        assert_eq!(
            retain([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
            [("PATH".to_string(), "/usr/bin:/bin".to_string())]
        );
    }
}
