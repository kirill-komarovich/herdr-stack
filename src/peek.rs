//! `L`: the log of the row under the cursor, drawn over the list (§12).
//!
//! A peek is not a viewer. It scrolls, it follows, it clips — no search, no wrapping, and no colour:
//! an escape sequence left in the text would reach the popup's own terminal as a command rather than
//! as content, so it is stripped here rather than rendered.
//!
//! Neither source blocks the event loop. A local unit's log is read by byte offset through the same
//! `Follower` the overlay uses, which is what lets a peek notice a restart truncating the file under
//! it; a compose service's log is a child process whose pipes are drained by threads of their own.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use crate::manifest::Project;
use crate::rows::contract_home;
use crate::store::{Identity, Store};
use crate::tail::{Follower, RESTARTED};
use crate::unit;

pub const NO_REPO_LOG: &str = "a repo row has no single log; expand it with tab";
const WAITING: &str = "waiting for output…";
const ENDED: &str = "compose logs ended";

/// How many lines stay scrollable. §8 lets a log grow for a week, so something has to give, and it is
/// the oldest lines.
const CAPACITY: usize = 4000;
/// How much of an existing log the first read takes in.
const FIRST_READ: u64 = 256 * 1024;

/// Which log a row peeks, decided before anything is opened: a compose peek spawns a child process,
/// and knowing *what* a row would peek is worth asking without starting one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Log { unit: String, path: PathBuf },
    Compose { service: String, root: PathBuf },
}

impl Source {
    /// The row's kind decides. Only our own local units keep a log file; a compose service's output
    /// belongs to docker, and a collapsed repo row stands for a whole manifest rather than a unit.
    pub fn of(store: &Store, project: &Project, kind: &str, name: &str) -> Result<Source, String> {
        match kind {
            unit::LOCAL => {
                let identity = Identity {
                    path: project.root.clone(),
                    name: project.name.clone(),
                };
                Ok(Source::Log {
                    unit: name.to_string(),
                    path: store
                        .slot(&identity)
                        .log_path(&unit::key(unit::LOCAL, name)),
                })
            }
            unit::DOCKER => Ok(Source::Compose {
                service: name.to_string(),
                root: project.root.clone(),
            }),
            _ => Err(NO_REPO_LOG.to_string()),
        }
    }

    pub fn heading(&self) -> String {
        match self {
            Source::Log { unit, path } => format!("{unit}  local   {}", contract_home(path)),
            Source::Compose { service, root } => {
                format!("{service}  docker  compose logs in {}", contract_home(root))
            }
        }
    }

    pub fn open(self) -> Result<Peek, String> {
        let heading = self.heading();
        let feed = match &self {
            Source::Log { path, .. } => Feed::File(Follower::watching_tail(path, FIRST_READ)),
            Source::Compose { service, root } => Feed::Compose(Stream::of(root, service)?),
        };
        Ok(Peek {
            heading,
            feed,
            lines: Vec::new(),
            partial: String::new(),
            top: 0,
            follow: true,
            viewport: 1,
            trouble: None,
        })
    }
}

pub struct Peek {
    heading: String,
    feed: Feed,
    lines: Vec<String>,
    /// The tail of the last read, held back until its newline arrives.
    partial: String,
    top: usize,
    follow: bool,
    viewport: usize,
    trouble: Option<String>,
}

impl Peek {
    pub fn heading(&self) -> &str {
        &self.heading
    }

    pub fn following(&self) -> bool {
        self.follow
    }

    pub fn toggle_follow(&mut self) {
        self.follow = !self.follow;
    }

    pub fn trouble(&self) -> Option<&str> {
        self.trouble.as_deref()
    }

    /// Whatever the source has produced since the last call, and never a wait for it.
    pub fn pump(&mut self) {
        let mut fed: Vec<u8> = Vec::new();
        let mut restarted = false;
        match &mut self.feed {
            Feed::File(follower) => match follower.read() {
                Ok(fresh) => {
                    restarted = fresh.restarted;
                    fed = fresh.bytes;
                }
                Err(error) => self.trouble = Some(error.to_string()),
            },
            Feed::Compose(stream) => match stream.drained() {
                Ok(bytes) => fed = bytes,
                Err(bytes) => {
                    fed = bytes;
                    self.trouble = Some(stream.epitaph());
                }
            },
        }
        // A fresh generation makes everything on screen history, which is exactly what §12 says a peek
        // must not go on showing.
        if restarted {
            self.lines.clear();
            self.partial.clear();
            self.top = 0;
            self.push(RESTARTED.to_string());
        }
        self.absorb(&fed);
    }

    /// Positive is towards the newest line. Any manual move stops following: the two would otherwise
    /// fight over where the screen sits.
    pub fn scroll(&mut self, lines: isize) {
        self.follow = false;
        self.top = match lines {
            0 => self.top,
            up if up < 0 => self.top.saturating_sub(up.unsigned_abs()),
            down => (self.top + down.unsigned_abs()).min(self.ceiling()),
        };
    }

    pub fn page(&mut self, pages: isize) {
        self.scroll(pages * self.viewport.max(1) as isize);
    }

    /// The visible lines, clipped to `width` — long lines are cut, never reflowed. Drawing is also
    /// where the viewport becomes known, so paging has a page to work with.
    pub fn view(&mut self, height: usize, width: usize) -> Vec<String> {
        self.viewport = height.max(1);
        self.top = if self.follow {
            self.ceiling()
        } else {
            self.top.min(self.ceiling())
        };
        self.lines[self.top..]
            .iter()
            .take(self.viewport)
            .map(|line| clip(line, width))
            .collect()
    }

    /// Where the screen sits in the log, or that nothing has arrived yet.
    pub fn position(&self) -> String {
        match self.lines.len() {
            0 => WAITING.to_string(),
            total => format!("{}/{total}", (self.top + self.viewport).min(total)),
        }
    }

    fn ceiling(&self) -> usize {
        self.lines.len().saturating_sub(self.viewport)
    }

    fn absorb(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.partial.push_str(&String::from_utf8_lossy(bytes));
        while let Some(at) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=at).collect();
            self.push(readable(&line));
        }
    }

    fn push(&mut self, line: String) {
        self.lines.push(line);
        if let Some(excess) = self.lines.len().checked_sub(CAPACITY) {
            self.lines.drain(..excess);
            self.top = self.top.saturating_sub(excess);
        }
    }
}

enum Feed {
    File(Follower),
    Compose(Stream),
}

/// `compose logs --follow` as a child process, with a thread per pipe so a quiet log never holds the
/// event loop and a chatty one never fills the pipe buffer and wedges docker.
struct Stream {
    child: Child,
    chunks: Receiver<Vec<u8>>,
}

impl Stream {
    fn of(root: &Path, service: &str) -> Result<Stream, String> {
        let mut child = crate::docker::logs_command(root, service)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("{}: {error}", crate::docker::DOCKER))?;
        let (sender, chunks) = channel();
        if let Some(out) = child.stdout.take() {
            drain(out, sender.clone());
        }
        if let Some(err) = child.stderr.take() {
            drain(err, sender);
        }
        Ok(Stream { child, chunks })
    }

    /// `Err` carries what was left in the channel by a stream that has ended.
    fn drained(&mut self) -> Result<Vec<u8>, Vec<u8>> {
        let mut bytes = Vec::new();
        loop {
            match self.chunks.try_recv() {
                Ok(chunk) => bytes.extend_from_slice(&chunk),
                Err(TryRecvError::Empty) => return Ok(bytes),
                Err(TryRecvError::Disconnected) => return Err(bytes),
            }
        }
    }

    fn epitaph(&mut self) -> String {
        match self.child.try_wait() {
            Ok(Some(status)) if !status.success() => format!("{ENDED}: {status}"),
            _ => ENDED.to_string(),
        }
    }
}

/// The peek owns the child, so leaving the peek ends it.
impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn drain(mut pipe: impl Read + Send + 'static, sender: Sender<Vec<u8>>) {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        while let Ok(read) = pipe.read(&mut buffer) {
            if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });
}

/// One line with everything a terminal would act on taken out: escape sequences, carriage returns and
/// the rest of the control characters. Colour is not rendered (§12), and a tab is spent here rather
/// than left for the terminal to place.
fn readable(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        match character {
            '\u{1b}' => skip_escape(&mut characters),
            '\t' => out.push_str("    "),
            control if control.is_control() => {}
            character => out.push(character),
        }
    }
    out
}

fn skip_escape(characters: &mut std::str::Chars) {
    match characters.next() {
        // CSI: parameters up to a final byte in 0x40..=0x7e.
        Some('[') => {
            for character in characters.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&character) {
                    break;
                }
            }
        }
        // OSC: a string terminated by BEL or by ESC \.
        Some(']') => {
            let mut previous = ' ';
            for character in characters.by_ref() {
                if character == '\u{7}' || (previous == '\u{1b}' && character == '\\') {
                    break;
                }
                previous = character;
            }
        }
        // An escape with an intermediate byte — charset designation and its like — takes one more.
        Some(intermediate) if ('\u{20}'..='\u{2f}').contains(&intermediate) => {
            let _ = characters.next();
        }
        _ => {}
    }
}

fn clip(line: &str, width: usize) -> String {
    line.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("herdr-dev-peek-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch");
            Scratch(dir)
        }

        fn log(&self) -> PathBuf {
            self.0.join("local-vite.log")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn project(text: &str) -> Project {
        Project::parse(text, Path::new("/repos/storefront/.herdr-dev.toml")).expect("manifest")
    }

    fn peeking(path: &Path) -> Peek {
        Source::Log {
            unit: "vite".into(),
            path: path.to_path_buf(),
        }
        .open()
        .expect("a file peek opens nothing")
    }

    fn shown(peek: &mut Peek, height: usize) -> Vec<String> {
        peek.view(height, 80)
    }

    #[test]
    fn a_local_row_peeks_its_log_file_and_a_docker_row_peeks_compose() {
        let manifest = project(
            "[local.vite]\ncmd = [\"bin/vite\"]\n[docker]\nnames = [\"db\"]\n\
             [includes.accounts]\npath = \"/repos/accounts\"\n",
        );
        let store = Store::at("/state/herdr-dev");
        let log = Source::of(&store, &manifest, unit::LOCAL, "vite").expect("a log");
        match &log {
            Source::Log { unit, path } => {
                assert_eq!(unit, "vite");
                assert!(path.ends_with("logs/local-vite.log"), "{path:?}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            Source::of(&store, &manifest, unit::DOCKER, "db"),
            Ok(Source::Compose {
                service: "db".into(),
                root: manifest.root.clone(),
            })
        );
        assert_eq!(
            Source::of(&store, &manifest, "", "accounts"),
            Err(NO_REPO_LOG.to_string())
        );
    }

    #[test]
    fn a_peek_of_a_log_that_does_not_exist_yet_says_it_is_waiting_rather_than_refusing() {
        let scratch = Scratch::new("absent");
        let mut peek = peeking(&scratch.log());
        peek.pump();
        assert!(shown(&mut peek, 10).is_empty());
        assert_eq!(peek.position(), WAITING);

        std::fs::write(scratch.log(), "started\n").expect("log");
        peek.pump();
        assert_eq!(shown(&mut peek, 10), ["started"]);
    }

    #[test]
    fn a_restart_under_the_peek_replaces_what_is_on_screen_with_the_fresh_generation() {
        let scratch = Scratch::new("restart");
        std::fs::write(scratch.log(), "old one\nold two\n").expect("log");
        let mut peek = peeking(&scratch.log());
        peek.pump();
        assert_eq!(shown(&mut peek, 10), ["old one", "old two"]);

        std::fs::rename(scratch.log(), scratch.0.join("local-vite.log.1")).expect("rotate");
        std::fs::write(scratch.log(), "fresh one\n").expect("fresh log");
        peek.pump();
        assert_eq!(shown(&mut peek, 10), [RESTARTED, "fresh one"]);

        std::fs::write(scratch.log(), "fresh one\nfresh two\n").expect("append");
        peek.pump();
        assert_eq!(shown(&mut peek, 10), [RESTARTED, "fresh one", "fresh two"]);
    }

    #[test]
    fn following_shows_the_newest_lines_and_scrolling_stops_following() {
        let scratch = Scratch::new("follow");
        let lines: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        std::fs::write(scratch.log(), &lines).expect("log");
        let mut peek = peeking(&scratch.log());
        peek.pump();
        assert_eq!(shown(&mut peek, 3), ["line 8", "line 9", "line 10"]);

        peek.scroll(-2);
        assert!(!peek.following());
        assert_eq!(shown(&mut peek, 3), ["line 6", "line 7", "line 8"]);
        assert_eq!(peek.position(), "8/10");

        // Paused, new lines arrive without moving the screen.
        std::fs::write(scratch.log(), format!("{lines}line 11\n")).expect("append");
        peek.pump();
        assert_eq!(shown(&mut peek, 3), ["line 6", "line 7", "line 8"]);

        peek.toggle_follow();
        assert_eq!(shown(&mut peek, 3), ["line 9", "line 10", "line 11"]);
        assert_eq!(peek.position(), "11/11");
    }

    #[test]
    fn scrolling_and_paging_stop_at_both_ends() {
        let scratch = Scratch::new("ends");
        std::fs::write(
            scratch.log(),
            (1..=10).map(|n| format!("line {n}\n")).collect::<String>(),
        )
        .expect("log");
        let mut peek = peeking(&scratch.log());
        peek.pump();
        shown(&mut peek, 4);

        peek.page(-99);
        assert_eq!(shown(&mut peek, 4)[0], "line 1");
        peek.page(1);
        assert_eq!(shown(&mut peek, 4)[0], "line 5");
        peek.page(99);
        assert_eq!(
            shown(&mut peek, 4),
            ["line 7", "line 8", "line 9", "line 10"]
        );
        peek.scroll(1);
        assert_eq!(shown(&mut peek, 4)[0], "line 7");
    }

    #[test]
    fn a_line_longer_than_the_popup_is_clipped_rather_than_wrapped() {
        let scratch = Scratch::new("clip");
        std::fs::write(scratch.log(), format!("{}\nnext\n", "x".repeat(200))).expect("log");
        let mut peek = peeking(&scratch.log());
        peek.pump();
        let view = peek.view(10, 20);
        assert_eq!(view, ["x".repeat(20), "next".to_string()]);
    }

    #[test]
    fn colour_and_the_rest_of_the_control_characters_never_reach_the_screen() {
        assert_eq!(readable("\u{1b}[32mgreen\u{1b}[0m done"), "green done");
        assert_eq!(readable("\u{1b}]0;a title\u{7}shell"), "shell");
        assert_eq!(readable("\u{1b}]0;a title\u{1b}\\shell"), "shell");
        assert_eq!(readable("bare\rrewrite"), "barerewrite");
        assert_eq!(readable("a\tb"), "a    b");
        assert_eq!(readable("\u{1b}(Bplain"), "plain");
    }

    #[test]
    fn a_line_only_arrives_once_its_newline_does() {
        let scratch = Scratch::new("partial");
        std::fs::write(scratch.log(), "half").expect("log");
        let mut peek = peeking(&scratch.log());
        peek.pump();
        assert!(shown(&mut peek, 5).is_empty());

        std::fs::write(scratch.log(), "half a line\n").expect("finish the line");
        peek.pump();
        assert_eq!(shown(&mut peek, 5), ["half a line"]);
    }

    #[test]
    fn the_oldest_lines_go_rather_than_the_peek_growing_without_bound() {
        let scratch = Scratch::new("capacity");
        let mut peek = peeking(&scratch.log());
        for line in 0..CAPACITY + 50 {
            peek.push(format!("line {line}"));
        }
        assert_eq!(peek.lines.len(), CAPACITY);
        assert_eq!(peek.lines[0], format!("line {}", 50));
    }
}
