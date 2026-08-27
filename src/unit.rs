//! The unit state model, and the one wire codec for it.
//!
//! Both ends of the daemon socket build and read `Status` through here, so a state, an uptime and an
//! exit have exactly one spelling on the wire and in the rows.

use std::time::Duration;

use serde_json::{Value, json};

pub const LOCAL: &str = "local";
pub const DOCKER: &str = "docker";

/// The state key: a local and a docker unit may share a name, and this is what keeps them apart.
pub fn key(kind: &str, name: &str) -> String {
    format!("{kind}-{name}")
}

/// The name back out of a key, when the key is of that kind. A docker unit's file sits beside a local
/// one in the same directory, so a reader of either has to say which it means.
pub fn name_of<'a>(key: &'a str, kind: &str) -> Option<&'a str> {
    key.strip_prefix(kind)?.strip_prefix('-')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Down,
    Starting,
    Up,
    Done,
    Dead,
    Unknown,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Down => "down",
            State::Starting => "starting",
            State::Up => "up",
            State::Done => "done",
            State::Dead => "dead",
            State::Unknown => "unknown",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            State::Down => "·",
            State::Starting => "◐",
            State::Up => "●",
            State::Done => "✔",
            State::Dead => "✗",
            State::Unknown => "?",
        }
    }

    pub fn read(label: &str) -> Option<State> {
        [
            State::Down,
            State::Starting,
            State::Up,
            State::Done,
            State::Dead,
            State::Unknown,
        ]
        .into_iter()
        .find(|state| state.label() == label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

impl Exit {
    pub fn of(status: std::process::ExitStatus) -> Exit {
        use std::os::unix::process::ExitStatusExt;
        match (status.code(), status.signal()) {
            (Some(code), _) => Exit::Code(code),
            (None, Some(signal)) => Exit::Signal(signal),
            (None, None) => Exit::Code(-1),
        }
    }

    pub fn label(self) -> String {
        match self {
            Exit::Code(code) => format!("exit {code}"),
            Exit::Signal(signal) => format!("signal {signal}"),
        }
    }
}

/// What one row needs to know: everything else about a unit lives in its log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub uptime: Option<Duration>,
    pub exit: Option<Exit>,
    /// The note column, when the state itself carries one: `unhealthy` on a service that is up all
    /// the same, or how old a row is once docker stopped answering.
    pub note: Option<String>,
}

impl Status {
    pub fn of(state: State) -> Status {
        Status {
            state,
            uptime: None,
            exit: None,
            note: None,
        }
    }

    /// The uptime-or-exit column: a running unit shows how long, anything else shows how it ended.
    /// The exit survives a stop, so a unit stopped after crashing still says what it crashed with.
    pub fn timing(&self) -> String {
        match (self.uptime, self.exit) {
            (Some(uptime), _) => elapsed(uptime),
            (None, Some(exit)) => exit.label(),
            (None, None) => String::new(),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut value = json!({"state": self.state.label()});
        let object = value.as_object_mut().expect("object");
        if let Some(uptime) = self.uptime {
            object.insert("uptime_ms".into(), json!(uptime.as_millis() as u64));
        }
        match self.exit {
            Some(Exit::Code(code)) => {
                object.insert("exit_code".into(), json!(code));
            }
            Some(Exit::Signal(signal)) => {
                object.insert("signal".into(), json!(signal));
            }
            None => {}
        }
        if let Some(note) = &self.note {
            object.insert("note".into(), json!(note));
        }
        value
    }

    pub fn read(value: &Value) -> Result<Status, String> {
        let label = value
            .get("state")
            .and_then(Value::as_str)
            .ok_or("unit status: no state")?;
        let state = State::read(label).ok_or_else(|| format!("unit status: no state `{label}`"))?;
        let exit = match (value.get("exit_code"), value.get("signal")) {
            (Some(code), _) => code.as_i64().map(|code| Exit::Code(code as i32)),
            (None, Some(signal)) => signal.as_i64().map(|signal| Exit::Signal(signal as i32)),
            (None, None) => None,
        };
        Ok(Status {
            state,
            uptime: value
                .get("uptime_ms")
                .and_then(Value::as_u64)
                .map(Duration::from_millis),
            exit,
            note: value
                .get("note")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

/// Nine columns at most, so the largest two units win and the rest is dropped.
pub fn elapsed(uptime: Duration) -> String {
    let seconds = uptime.as_secs();
    let (minutes, seconds) = (seconds / 60, seconds % 60);
    let (hours, minutes) = (minutes / 60, minutes % 60);
    let (days, hours) = (hours / 24, hours % 24);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds}s"),
        (0, 0, _) => format!("{minutes}m{seconds:02}s"),
        (0, _, _) => format!("{hours}h{minutes:02}m"),
        _ => format!("{days}d{hours:02}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unit_key_carries_its_kind_so_a_local_and_a_docker_name_may_collide() {
        assert_eq!(key(LOCAL, "vite"), "local-vite");
        assert_eq!(key(DOCKER, "vite"), "docker-vite");
    }

    #[test]
    fn a_key_yields_its_name_only_to_the_kind_that_owns_it() {
        assert_eq!(name_of("local-vite", LOCAL), Some("vite"));
        assert_eq!(name_of("local-vite", DOCKER), None);
        assert_eq!(name_of("docker-db", DOCKER), Some("db"));
        assert_eq!(name_of("localish-vite", LOCAL), None);
    }

    #[test]
    fn every_state_survives_a_round_trip_through_its_label() {
        for state in [
            State::Down,
            State::Starting,
            State::Up,
            State::Done,
            State::Dead,
            State::Unknown,
        ] {
            assert_eq!(State::read(state.label()), Some(state));
        }
        assert_eq!(State::read("crashed"), None);
    }

    #[test]
    fn an_uptime_reads_shortest_first_and_never_outgrows_its_column() {
        assert_eq!(elapsed(Duration::from_secs(9)), "9s");
        assert_eq!(elapsed(Duration::from_secs(724)), "12m04s");
        assert_eq!(elapsed(Duration::from_secs(2460)), "41m00s");
        assert_eq!(elapsed(Duration::from_secs(3600 * 3 + 240)), "3h04m");
        assert_eq!(elapsed(Duration::from_secs(86400 * 2 + 3600 * 5)), "2d05h");
        assert!(
            elapsed(Duration::from_secs(86400 * 99)).len() <= 9,
            "the timing column is nine wide"
        );
    }

    #[test]
    fn a_running_unit_shows_its_uptime_and_anything_else_shows_how_it_ended() {
        let up = Status {
            state: State::Up,
            uptime: Some(Duration::from_secs(61)),
            exit: Some(Exit::Code(1)),
            note: None,
        };
        assert_eq!(up.timing(), "1m01s");

        let dead = Status {
            state: State::Dead,
            uptime: None,
            exit: Some(Exit::Signal(9)),
            note: None,
        };
        assert_eq!(dead.timing(), "signal 9");
        assert_eq!(Status::of(State::Down).timing(), "");
    }

    #[test]
    fn a_status_survives_the_wire_whole() {
        let statuses = [
            Status {
                state: State::Up,
                uptime: Some(Duration::from_millis(41_000)),
                exit: None,
                note: Some("unhealthy".into()),
            },
            Status {
                state: State::Dead,
                uptime: None,
                exit: Some(Exit::Code(7)),
                note: None,
            },
            Status {
                state: State::Down,
                uptime: None,
                exit: Some(Exit::Signal(15)),
                note: None,
            },
        ];
        for status in statuses {
            assert_eq!(Status::read(&status.to_value()), Ok(status.clone()));
        }
    }

    #[test]
    fn a_status_without_a_state_is_refused_rather_than_guessed() {
        assert!(Status::read(&json!({})).is_err());
        assert!(Status::read(&json!({"state": "wedged"})).is_err());
    }
}
