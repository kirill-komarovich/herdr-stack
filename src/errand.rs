//! The daemon at arm's length: a thread holds the socket, and the popup hands it errands.
//!
//! Every round trip used to be made at the keyboard, between a keystroke and the frame it caused. A
//! status read runs `compose ps` and a docker start waits up to ten seconds on a healthcheck, so the
//! whole popup — cursor, scroll, `q` — stood still for as long as the daemon took. Here a keystroke
//! only posts the errand, the answer arrives whenever it arrives, and every frame in between is drawn.
//!
//! Reads and verbs go down connections of their own, because they wait on different things: a status
//! read is a `compose ps` the popup asks for on a beat, and a verb is the user's own and must not
//! queue behind one. Each connection is served by one thread in the order posted, so a stop pressed
//! after a start is still performed after it. Holding a connection for the popup's whole life is also
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
    acts: Sender<Errand>,
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
        let acts = serve(
            Held {
                endpoint,
                link: None,
            },
            answering,
        );
        Errands {
            reads,
            acts,
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
        let _ = self.acts.send(Errand {
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
