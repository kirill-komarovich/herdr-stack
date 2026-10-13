//! The daemon at arm's length: a thread holds the socket, and the popup hands it errands.
//!
//! Every round trip used to be made at the keyboard, between a keystroke and the frame it caused. A
//! status read runs `compose ps` and a docker start waits up to ten seconds on a healthcheck, so the
//! whole popup — cursor, scroll, `q` — stood still for as long as the daemon took. Here a keystroke
//! only posts the errand, the answer arrives whenever it arrives, and every frame in between is drawn.
//!
//! Reads and verbs go down connections of their own, because they wait on different things: a status
//! read is a `compose ps` the popup asks for on a beat, and a verb is the user's own and must not
//! queue behind one. Each unit's verbs go down a connection of their own too, because a docker start
//! holds `compose up --wait` on a healthcheck and a start pressed on another row meanwhile must not
//! wait it out. Each connection is served by one thread in the order posted, so a stop pressed after
//! a start on the same unit is still performed after it. Holding a connection for the popup's whole life is also
//! what keeps an idle daemon from exiting underneath it.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use serde_json::Value;

use crate::client::{self, Endpoint, Link};
use crate::unit::Status;
use crate::view::Owner;

/// What came back, and for whose errand. A verb answers with a note or with nothing, a read with the
/// manifest's units, and either may answer with the sentence the daemon refused it with.
#[derive(Debug)]
pub enum Answer {
    Read(Owner, Result<BTreeMap<String, Status>, String>),
    Note(Result<Option<String>, String>),
}

struct Errand {
    method: &'static str,
    params: Value,
    /// Set on a status read alone: whose rows the answer belongs to.
    owner: Option<Owner>,
}

pub struct Errands {
    reads: Sender<Errand>,
    lanes: BTreeMap<String, Sender<Errand>>,
    endpoint: Endpoint,
    answering: Sender<Answer>,
    answers: Receiver<Answer>,
    /// Status reads posted and not yet answered. A second beat must not queue a second round behind
    /// a daemon that is still busy with the first.
    reading: usize,
}

impl Errands {
    /// Takes the popup's link over for the reads. A link that was never made is kept all the same, so
    /// an errand posted to a daemon that is not there is refused with the reason rather than dropped
    /// in silence, and the endpoint the verbs dial is the same one it was made from.
    pub fn keeping(link: Result<Link, String>, endpoint: Endpoint) -> Errands {
        let (answering, answers) = channel::<Answer>();
        let reads = serve(
            Held {
                endpoint: endpoint.clone(),
                link: Some(link),
            },
            answering.clone(),
        );
        Errands {
            reads,
            lanes: BTreeMap::new(),
            endpoint,
            answering,
            answers,
            reading: 0,
        }
    }

    pub fn read(&mut self, owner: Owner, params: Value) {
        self.reading += 1;
        let errand = Errand {
            method: "status",
            params,
            owner: Some(owner),
        };
        if self.reads.send(errand).is_err() {
            self.hung_up();
        }
    }

    pub fn verb(&mut self, method: &'static str, params: Value) {
        let endpoint = &self.endpoint;
        let answering = &self.answering;
        let lane = self.lanes.entry(lane_of(&params)).or_insert_with(|| {
            serve(
                Held {
                    endpoint: endpoint.clone(),
                    link: None,
                },
                answering.clone(),
            )
        });
        let _ = lane.send(Errand {
            method,
            params,
            owner: None,
        });
    }

    /// Whether a round of reads is still out. Only a read is counted: a verb is the user's own doing
    /// and waits for nothing.
    pub fn reading(&self) -> bool {
        self.reading > 0
    }

    /// Everything that has come back since the last frame. Never waits: a frame is drawn on the beat
    /// whether or not the daemon has said anything.
    pub fn answered(&mut self) -> Vec<Answer> {
        let mut answers = Vec::new();
        loop {
            match self.answers.try_recv() {
                Ok(answer) => {
                    if matches!(answer, Answer::Read(..)) {
                        self.reading = self.reading.saturating_sub(1);
                    }
                    answers.push(answer);
                }
                Err(TryRecvError::Empty) => return answers,
                Err(TryRecvError::Disconnected) => {
                    self.hung_up();
                    return answers;
                }
            }
        }
    }

    /// The thread is gone, so nothing outstanding is coming back. Left uncounted, the next beat would
    /// wait forever on a read that no longer exists.
    fn hung_up(&mut self) {
        self.reading = 0;
    }
}

fn lane_of(params: &Value) -> String {
    let unit = &params["unit"];
    format!(
        "{}\0{}\0{}",
        params["project"]["path"], unit["kind"], unit["name"]
    )
}

/// One thread on one connection, answering everything posted to it in order.
fn serve(mut held: Held, answering: Sender<Answer>) -> Sender<Errand> {
    let (posting, posted) = channel::<Errand>();
    std::thread::spawn(move || {
        for errand in posted {
            let reply = match held.link() {
                Ok(link) => link.request(errand.method, errand.params),
                Err(complaint) => Err(complaint.clone()),
            };
            let answer = match errand.owner {
                None => Answer::Note(reply.map(|reply| client::note_of(&reply))),
                Some(owner) => {
                    Answer::Read(owner, reply.and_then(|reply| client::statuses_of(&reply)))
                }
            };
            if answering.send(answer).is_err() {
                break;
            }
        }
    });
    posting
}

/// The connection a thread sends down, dialled at its first errand unless it was handed one already
/// open: a popup where nothing is ever started opens no second connection.
struct Held {
    endpoint: Endpoint,
    link: Option<Result<Link, String>>,
}

impl Held {
    fn link(&mut self) -> &mut Result<Link, String> {
        if self.link.is_none() {
            self.link = Some(self.endpoint.open());
        }
        self.link.as_mut().expect("a link")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::{daemon, state};

    const SLOW: Duration = Duration::from_millis(1500);

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("herdr-stack-errand-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp root");
        root
    }

    /// Serves every connection on a thread of its own, as the real daemon does, and takes `SLOW` over
    /// a start of `db` the way `compose up --wait` holds one on a healthcheck. Each verb is answered
    /// with a note naming what was done to which unit.
    fn fake_daemon(root: &Path) {
        let listener = UnixListener::bind(state::socket_path(root)).expect("bind");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut writer = &stream;
                    for line in BufReader::new(&stream).lines() {
                        let Ok(line) = line else { break };
                        let asked: Value = serde_json::from_str(&line).expect("json");
                        let method = asked["method"].as_str().unwrap_or_default().to_string();
                        let result = if method == "handshake" {
                            json!({"version": "test", "protocol": daemon::PROTOCOL, "pid": 1})
                        } else {
                            let unit = asked["params"]["unit"]["name"].as_str().unwrap_or_default();
                            if method == "start" && unit == "db" {
                                std::thread::sleep(SLOW);
                            }
                            json!({"note": format!("{method} {unit}")})
                        };
                        let reply = json!({"id": asked["id"], "result": result});
                        if writeln!(writer, "{reply}").is_err() {
                            break;
                        }
                    }
                });
            }
        });
    }

    fn params(unit: &str) -> Value {
        json!({"project": {"path": "/p", "name": "p"}, "unit": {"kind": "local", "name": unit}})
    }

    fn notes(errands: &mut Errands, wanted: usize) -> Vec<(String, Duration)> {
        let posted = Instant::now();
        let mut notes = Vec::new();
        while notes.len() < wanted && posted.elapsed() < SLOW * 4 {
            for answer in errands.answered() {
                if let Answer::Note(Ok(Some(note))) = answer {
                    notes.push((note, posted.elapsed()));
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        notes
    }

    #[test]
    fn a_verb_on_one_unit_does_not_queue_behind_a_slow_verb_on_another() {
        let root = temp_root("lanes");
        fake_daemon(&root);
        let endpoint = Endpoint::at(&root);
        let mut errands = Errands::keeping(endpoint.open(), endpoint);

        errands.verb("start", params("db"));
        errands.verb("start", params("rails"));

        let notes = notes(&mut errands, 2);
        assert_eq!(notes[0].0, "start rails", "{notes:?}");
        assert!(notes[0].1 < SLOW, "{notes:?}");
    }

    #[test]
    fn a_stop_pressed_after_a_start_on_the_same_unit_is_still_performed_after_it() {
        let root = temp_root("order");
        fake_daemon(&root);
        let endpoint = Endpoint::at(&root);
        let mut errands = Errands::keeping(endpoint.open(), endpoint);

        errands.verb("start", params("db"));
        errands.verb("stop", params("db"));

        let said: Vec<String> = notes(&mut errands, 2)
            .into_iter()
            .map(|(note, _)| note)
            .collect();
        assert_eq!(said, ["start db", "stop db"]);
    }
}
