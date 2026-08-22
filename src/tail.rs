//! Following one log file by byte offset, and the got-shorter rule of §8.
//!
//! A unit's log is truncated at spawn and the previous generation is renamed aside, so a follower
//! that only ever moved forward would sit past the end of a fresh file and go silent for the rest of
//! the run. Both readers of a log need the same rule — this overlay and the in-place peek — so it
//! lives here rather than in either of them.

use std::fs::File;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

/// The three names the plugin manifest and the TUI must agree on: the pane is declared once and
/// carries the log path in at open time, so one entrypoint serves every unit.
pub const PLUGIN_ID: &str = "herdr-dev";
pub const ENTRYPOINT: &str = "tail";
pub const LOG_ENV: &str = "HERDR_DEV_LOG";

const POLL: Duration = Duration::from_millis(150);
pub const RESTARTED: &str = "── log restarted ──";

/// What one read of the log turned up.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Fresh {
    pub bytes: Vec<u8>,
    /// The file got shorter or was replaced, so `bytes` starts a new generation and whatever a caller
    /// has already shown belongs to the previous one.
    pub restarted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    device: u64,
    inode: u64,
    offset: u64,
}

pub struct Follower {
    path: PathBuf,
    seen: Option<Seen>,
    first_read_tail: u64,
}

impl Follower {
    pub fn watching(path: impl Into<PathBuf>) -> Follower {
        Follower {
            path: path.into(),
            seen: None,
            first_read_tail: u64::MAX,
        }
    }

    /// Starts roughly `bytes` from the end of a log that already exists rather than at its start: §8
    /// lets a log grow for a week, and a reader that took all of it in would read the week into
    /// memory. The line the offset lands in the middle of is dropped rather than shown half.
    pub fn watching_tail(path: impl Into<PathBuf>, bytes: u64) -> Follower {
        Follower {
            path: path.into(),
            seen: None,
            first_read_tail: bytes,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Everything written since the last read. A log that does not exist yet is not an error: the
    /// unit may simply not have been started, and the follower picks it up when it appears.
    pub fn read(&mut self) -> io::Result<Fresh> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                self.seen = None;
                return Ok(Fresh::default());
            }
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        let (device, inode, length) = (metadata.dev(), metadata.ino(), metadata.len());

        let mut fresh = Fresh::default();
        let first = self.seen.is_none();
        // Rotation is rename-then-create, so the fresh log is a different inode and may already have
        // outgrown the old offset by the time we look: length alone would miss it.
        let offset = match self.seen {
            Some(seen) if seen.inode == inode && seen.device == device && seen.offset <= length => {
                seen.offset
            }
            Some(_) => {
                fresh.restarted = true;
                0
            }
            None => length.saturating_sub(self.first_read_tail),
        };
        file.seek(SeekFrom::Start(offset))?;
        file.read_to_end(&mut fresh.bytes)?;
        self.seen = Some(Seen {
            device,
            inode,
            offset: offset + fresh.bytes.len() as u64,
        });
        if first && offset > 0 && !fresh.bytes.is_empty() {
            let line_break = fresh.bytes.iter().position(|byte| *byte == b'\n');
            fresh
                .bytes
                .drain(..=line_break.unwrap_or(fresh.bytes.len() - 1));
        }
        Ok(fresh)
    }
}

/// `tail` mode: the plugin pane's whole job. It never exits on its own — the pane closing is what
/// ends it.
pub fn run() -> io::Result<()> {
    let path = std::env::var_os(LOG_ENV)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            io::Error::other(format!(
                "{LOG_ENV} is unset: the log to follow is passed in at plugin.pane.open"
            ))
        })?;
    let mut follower = Follower::watching(PathBuf::from(path));
    let mut out = io::stdout();
    writeln!(out, "{}", follower.path().display())?;
    loop {
        pump(&mut follower, &mut out)?;
        sleep(POLL);
    }
}

/// One read, written out. A restart is announced, because output silently jumping back to the top of
/// a fresh log reads as corruption.
fn pump(follower: &mut Follower, out: &mut impl Write) -> io::Result<()> {
    let fresh = follower.read()?;
    if fresh.restarted {
        writeln!(out, "{RESTARTED}")?;
    }
    if fresh.bytes.is_empty() && !fresh.restarted {
        return Ok(());
    }
    out.write_all(&fresh.bytes)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    use toml_edit::DocumentMut;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("herdr-dev-tail-{name}"));
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

    fn text(fresh: &Fresh) -> String {
        String::from_utf8_lossy(&fresh.bytes).into_owned()
    }

    #[test]
    fn a_follower_reads_the_file_once_and_then_only_what_was_appended() {
        let scratch = Scratch::new("append");
        std::fs::write(scratch.log(), "one\n").expect("write");
        let mut follower = Follower::watching(scratch.log());
        assert_eq!(text(&follower.read().expect("read")), "one\n");
        assert_eq!(follower.read().expect("read"), Fresh::default());

        std::fs::write(scratch.log(), "one\ntwo\n").expect("append");
        let fresh = follower.read().expect("read");
        assert_eq!(
            (text(&fresh), fresh.restarted),
            ("two\n".to_string(), false)
        );
    }

    #[test]
    fn a_log_truncated_under_the_follower_is_read_from_the_start_of_the_fresh_file() {
        let scratch = Scratch::new("truncate");
        std::fs::write(scratch.log(), "a long first generation\n").expect("write");
        let mut follower = Follower::watching(scratch.log());
        follower.read().expect("read");

        std::fs::write(scratch.log(), "second\n").expect("truncate");
        let fresh = follower.read().expect("read");
        assert_eq!((text(&fresh), fresh.restarted), ("second\n".into(), true));
        assert_eq!(follower.read().expect("read"), Fresh::default());
    }

    /// The rotation §8 actually performs: rename aside, create anew. The replacement can be longer
    /// than the offset the follower held, so only the inode gives it away.
    #[test]
    fn a_log_rotated_out_from_under_the_follower_is_read_from_the_start_too() {
        let scratch = Scratch::new("rotate");
        std::fs::write(scratch.log(), "first\n").expect("write");
        let mut follower = Follower::watching(scratch.log());
        follower.read().expect("read");

        std::fs::rename(scratch.log(), scratch.0.join("local-vite.log.1")).expect("rotate");
        std::fs::write(scratch.log(), "a longer second generation\n").expect("write");
        let fresh = follower.read().expect("read");
        assert!(fresh.restarted);
        assert_eq!(text(&fresh), "a longer second generation\n");
    }

    #[test]
    fn a_follower_started_at_the_tail_takes_in_whole_lines_only_and_then_follows_as_usual() {
        let scratch = Scratch::new("tail-start");
        std::fs::write(scratch.log(), "one\ntwo\nthree\n").expect("write");
        let mut follower = Follower::watching_tail(scratch.log(), 9);
        assert_eq!(text(&follower.read().expect("read")), "three\n");

        std::fs::write(scratch.log(), "one\ntwo\nthree\nfour\n").expect("append");
        assert_eq!(text(&follower.read().expect("read")), "four\n");
    }

    #[test]
    fn a_follower_started_at_the_tail_of_a_short_log_still_reads_all_of_it() {
        let scratch = Scratch::new("tail-short");
        std::fs::write(scratch.log(), "one\ntwo\n").expect("write");
        let mut follower = Follower::watching_tail(scratch.log(), 4096);
        assert_eq!(text(&follower.read().expect("read")), "one\ntwo\n");
    }

    #[test]
    fn a_log_that_does_not_exist_yet_is_waited_for_rather_than_refused() {
        let scratch = Scratch::new("absent");
        let mut follower = Follower::watching(scratch.log());
        assert_eq!(follower.read().expect("read"), Fresh::default());

        std::fs::write(scratch.log(), "started\n").expect("write");
        assert_eq!(text(&follower.read().expect("read")), "started\n");
    }

    #[test]
    fn a_restart_is_announced_in_the_pane() {
        let scratch = Scratch::new("pump");
        std::fs::write(scratch.log(), "first\n").expect("write");
        let mut follower = Follower::watching(scratch.log());
        let mut out: Vec<u8> = Vec::new();
        pump(&mut follower, &mut out).expect("pump");
        std::fs::write(scratch.log(), "x\n").expect("truncate");
        pump(&mut follower, &mut out).expect("pump");
        assert_eq!(
            String::from_utf8_lossy(&out),
            format!("first\n{RESTARTED}\nx\n")
        );
    }

    /// The manifest is the contract these constants stand for; §3 also spells out what it must not
    /// declare.
    #[test]
    fn the_plugin_manifest_declares_one_relative_pane_and_nothing_else() {
        let doc = include_str!("../herdr-plugin.toml")
            .parse::<DocumentMut>()
            .expect("manifest parses");
        assert_eq!(doc["id"].as_str(), Some(PLUGIN_ID));
        for absent in ["actions", "startup", "events", "build"] {
            assert!(doc.get(absent).is_none(), "manifest declares {absent}");
        }
        let panes = doc["panes"].as_array_of_tables().expect("panes");
        assert_eq!(panes.len(), 1);
        let pane = panes.get(0).expect("pane");
        assert_eq!(pane["id"].as_str(), Some(ENTRYPOINT));
        assert_eq!(pane["placement"].as_str(), Some("overlay"));
        let command: Vec<&str> = pane["command"]
            .as_array()
            .expect("command")
            .iter()
            .map(|word| word.as_str().expect("word"))
            .collect();
        assert_eq!(command, ["./target/release/herdr-dev", ENTRYPOINT]);
    }
}
