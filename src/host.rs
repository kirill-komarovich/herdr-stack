//! Which Herdr the popup runs under: herdr 0.9, whose control socket and plugin panes it drives, or
//! Herdr 2, whose core `hc` runs the terminal it is in.
//!
//! The agent-facing verbs and the daemon need neither: only what the popup opens and where it looks
//! for a project differ.

use crate::hc::Hc;

/// `herdr` or `hc`, for when the environment says both: a core started from a herdr pane hands its
/// terminals that pane's `HERDR_*`, and a herdr server started inside an hc terminal hands its panes
/// `HC_*`.
pub const HOST_ENV: &str = "HERDR_STACK_HOST";

/// Only a herdr popup is handed this, so it outweighs an `HC_*` inherited from whatever started the
/// herdr server.
const HERDR_POPUP_ENV: &str = "HERDR_ACTIVE_PANE_CWD";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    Herdr,
    Hc(Hc),
}

impl Host {
    pub fn detect() -> Result<Host, String> {
        Host::of(|key| std::env::var(key).ok())
    }

    pub fn of(var: impl Fn(&str) -> Option<String>) -> Result<Host, String> {
        let hc = Hc::of(&var);
        match var(HOST_ENV).as_deref().filter(|value| !value.is_empty()) {
            Some("herdr") => Ok(Host::Herdr),
            Some("hc") => hc.map(Host::Hc).ok_or_else(|| {
                format!(
                    "{HOST_ENV}=hc, but this is not an hc terminal: {} is unset",
                    crate::hc::SOCKET_ENV
                )
            }),
            Some(other) => Err(format!("{HOST_ENV}={other}: expected herdr or hc")),
            None if var(HERDR_POPUP_ENV).is_some_and(|cwd| !cwd.is_empty()) => Ok(Host::Herdr),
            None => Ok(hc.map(Host::Hc).unwrap_or(Host::Herdr)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::hc::{SOCKET_ENV, TERMINAL_ENV};

    fn host(pairs: &[(&str, &str)]) -> Result<Host, String> {
        Host::of(|key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        })
    }

    const IN_HC: [(&str, &str); 2] = [(SOCKET_ENV, "/run/hc.sock"), (TERMINAL_ENV, "t4")];

    fn hc() -> Host {
        Host::Hc(Hc {
            socket: "/run/hc.sock".into(),
            terminal: "t4".into(),
        })
    }

    #[test]
    fn an_hc_terminal_is_hc_and_anything_else_is_herdr() {
        assert_eq!(host(&IN_HC), Ok(hc()));
        assert_eq!(host(&[("HERDR_ENV", "1")]), Ok(Host::Herdr));
        assert_eq!(host(&[]), Ok(Host::Herdr));
    }

    #[test]
    fn a_herdr_popup_outweighs_an_inherited_hc_terminal() {
        let mut env = IN_HC.to_vec();
        env.push((HERDR_POPUP_ENV, "/repos/storefront"));
        assert_eq!(host(&env), Ok(Host::Herdr));
    }

    #[test]
    fn the_host_can_be_named() {
        let mut env = IN_HC.to_vec();
        env.push((HOST_ENV, "herdr"));
        assert_eq!(host(&env), Ok(Host::Herdr));

        let mut env = IN_HC.to_vec();
        env.push((HERDR_POPUP_ENV, "/repos/storefront"));
        env.push((HOST_ENV, "hc"));
        assert_eq!(host(&env), Ok(hc()));

        let refused = host(&[(HOST_ENV, "hc")]).unwrap_err();
        assert!(refused.contains(SOCKET_ENV), "{refused}");
        let refused = host(&[(HOST_ENV, "tmux")]).unwrap_err();
        assert!(refused.contains("tmux"), "{refused}");
    }
}
