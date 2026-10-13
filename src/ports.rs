//! Which ports a local unit is listening on, observed rather than declared.
//!
//! A manifest never carries a port — every unit starts in the user's shell with its prompt hooks run,
//! so the numbers live in whatever those hooks read and change without anyone editing a manifest. The
//! only honest answer is therefore the one the kernel gives: what is bound right now.
//!
//! The join is by **session**. A unit is `setsid(2)`'d at spawn, so its pid is its session's, and
//! whatever it forks — a `bin/vite dev` wrapper's node, a `rails` cluster's workers — stays in that
//! session even when, as pnpm does with vite, it is given a process group of its own.
//!
//! One `lsof` for the whole machine rather than one per unit: it is ~90 ms here and a stack is a dozen
//! units, so anything per-unit would be seconds.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, Stdio};

use crate::local;

/// Listening ports keyed by the session holding them.
pub type Listeners = BTreeMap<u32, BTreeSet<u16>>;

/// `Err` is "the machine could not be asked", which a caller reports rather than renders as a unit
/// listening on nothing.
pub fn listening() -> Result<Listeners, String> {
    let sockets = run(lsof_command(), "lsof")?;
    Ok(fold(&parse_lsof(&sockets), local::session_of))
}

fn lsof_command() -> Command {
    let mut command = Command::new("lsof");
    // `-P` and `-n` keep it from resolving a port to a service name or an address to a host, both of
    // which are slow and neither of which is wanted; `-F pn` is the machine-readable form.
    command
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pn"])
        .stdin(Stdio::null());
    command
}

/// `lsof` exits 1 when it merely found nothing, so the status is not read: empty output is the answer
/// in that case, and a real failure shows up as empty output too rather than as a wrong number.
fn run(mut command: Command, name: &str) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("{name}: {error}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `-F pn` emits a `p<pid>` line and then an `n<address>` line per socket under it.
fn parse_lsof(text: &str) -> BTreeMap<u32, BTreeSet<u16>> {
    let mut found: BTreeMap<u32, BTreeSet<u16>> = BTreeMap::new();
    let mut pid = None;
    for line in text.lines() {
        let Some(rest) = line.get(1..) else { continue };
        match line.as_bytes()[0] {
            b'p' => pid = rest.parse::<u32>().ok(),
            b'n' => {
                if let (Some(pid), Some(port)) = (pid, port_of(rest)) {
                    found.entry(pid).or_default().insert(port);
                }
            }
            _ => {}
        }
    }
    found
}

/// `*:5432`, `127.0.0.1:3000` and `[::1]:3000` all end in the port, and a v6 address is full of the
/// separator, so only the last one counts.
fn port_of(address: &str) -> Option<u16> {
    address.rsplit_once(':')?.1.parse().ok()
}

/// A listener whose session can no longer be asked is dropped: it exited after `lsof` saw it, and
/// guessing whose it was would be inventing an owner.
fn fold(
    listeners: &BTreeMap<u32, BTreeSet<u16>>,
    session_of: impl Fn(u32) -> Option<u32>,
) -> Listeners {
    let mut folded: Listeners = BTreeMap::new();
    for (pid, ports) in listeners {
        let Some(session) = session_of(*pid) else {
            continue;
        };
        folded.entry(session).or_default().extend(ports);
    }
    folded
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORDED: &str =
        "p662\nf10\nn*:58306\nf11\nn*:58306\np708\nf10\nn127.0.0.1:7000\nf12\nn[::1]:5000\n";

    #[test]
    fn a_socket_belongs_to_the_pid_last_named_above_it() {
        let found = parse_lsof(RECORDED);
        assert_eq!(found[&662], BTreeSet::from([58306]));
        assert_eq!(found[&708], BTreeSet::from([7000, 5000]));
    }

    #[test]
    fn the_same_port_on_two_addresses_is_one_port() {
        assert_eq!(parse_lsof("p1\nn*:3000\nn[::1]:3000\n")[&1].len(), 1);
    }

    #[test]
    fn a_line_that_is_neither_a_pid_nor_an_address_is_passed_over() {
        assert!(parse_lsof("f10\nn*:3000\ncsomething\n").is_empty());
        assert!(parse_lsof("").is_empty());
    }

    #[test]
    fn ports_land_under_the_session_whatever_group_holds_them() {
        let listeners =
            BTreeMap::from([(920, BTreeSet::from([3000])), (930, BTreeSet::from([5173]))]);
        let sessions = |pid| matches!(pid, 920 | 930).then_some(900);
        assert_eq!(
            fold(&listeners, sessions),
            Listeners::from([(900, BTreeSet::from([3000, 5173]))])
        );
    }

    #[test]
    fn a_listener_whose_session_cannot_be_asked_is_dropped_rather_than_guessed_at() {
        let listeners = BTreeMap::from([(920, BTreeSet::from([3000]))]);
        assert!(fold(&listeners, |_| None).is_empty());
    }
}
