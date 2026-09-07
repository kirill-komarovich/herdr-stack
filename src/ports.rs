//! Which ports a local unit is listening on, observed rather than declared.
//!
//! A manifest never carries a port — every unit is spawned through `mise exec --`, so the numbers live
//! in the repo's mise files and change without anyone editing a manifest. The only honest answer is
//! therefore the one the kernel gives: what is bound right now.
//!
//! The join is by **process group**. A unit is `setsid(2)`'d at spawn, so its pid is its group's, and
//! whatever it forks — a `bin/vite dev` wrapper's node, a `rails` cluster's workers — is in that group
//! and is what actually holds the socket.
//!
//! Two commands for the whole machine rather than two per unit: `lsof` is ~90 ms here and a stack is a
//! dozen units, so anything per-unit would be seconds.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, Stdio};

/// Listening ports keyed by the process group holding them.
pub type Listeners = BTreeMap<u32, BTreeSet<u16>>;

/// `Err` is "the machine could not be asked", which a caller reports rather than renders as a unit
/// listening on nothing.
pub fn listening() -> Result<Listeners, String> {
    let sockets = run(lsof_command(), "lsof")?;
    let table = run(ps_command(), "ps")?;
    Ok(fold(&parse_lsof(&sockets), &parse_groups(&table)))
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

fn ps_command() -> Command {
    let mut command = Command::new("/bin/ps");
    command
        .args(["-ax", "-o", "pid=,pgid="])
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

fn parse_groups(text: &str) -> BTreeMap<u32, u32> {
    text.lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let pid = columns.next()?.parse().ok()?;
            let group = columns.next()?.parse().ok()?;
            Some((pid, group))
        })
        .collect()
}

/// A listener whose pid the process table no longer knows is dropped: it exited between the two
/// commands, and guessing its group would be inventing one.
fn fold(listeners: &BTreeMap<u32, BTreeSet<u16>>, groups: &BTreeMap<u32, u32>) -> Listeners {
    let mut folded: Listeners = BTreeMap::new();
    for (pid, ports) in listeners {
        let Some(group) = groups.get(pid) else {
            continue;
        };
        folded.entry(*group).or_default().extend(ports);
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
    fn ports_land_under_the_group_leader_rather_than_the_pid_that_forked_last() {
        let listeners = BTreeMap::from([(920, BTreeSet::from([3000]))]);
        let groups = BTreeMap::from([(900, 900), (920, 900)]);
        assert_eq!(
            fold(&listeners, &groups),
            Listeners::from([(900, BTreeSet::from([3000]))])
        );
    }

    #[test]
    fn a_listener_the_process_table_has_lost_is_dropped_rather_than_grouped_by_guess() {
        let listeners = BTreeMap::from([(920, BTreeSet::from([3000]))]);
        assert!(fold(&listeners, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn the_process_table_is_read_as_two_columns() {
        assert_eq!(
            parse_groups("  920   900\n 1 1\nnonsense\n"),
            BTreeMap::from([(920, 900), (1, 1)])
        );
    }
}
