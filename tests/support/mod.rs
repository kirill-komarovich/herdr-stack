//! What a pty-driven test needs and no test owns alone: a terminal of this test's own making, a screen
//! model to read back what was drawn into it, and a stand-in for Herdr's control socket.
//!
//! Nothing here touches the real state root or the real Herdr: the popup under test is given a `HOME`
//! of the caller's making, and the socket it dials is the one bound below.

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const PATIENCE: Duration = Duration::from_secs(30);
pub const STEP: Duration = Duration::from_millis(50);
const ROWS: u16 = 24;

pub fn exe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_herdr-dev"))
}

/// Not `TMPDIR`: macOS puts it under `/var/folders/…`, and a socket path built from it outgrows
/// `SUN_LEN` before it reaches `daemon.sock`.
pub fn staging(name: &str) -> PathBuf {
    PathBuf::from("/tmp").join(name)
}

/// A pty this test owns both ends of: the popup on the far side, keys written into the near side and
/// everything it drew read back from it.
pub struct Pty {
    master: File,
    child: Option<Child>,
    screen: Screen,
}

impl Pty {
    pub fn of(home: &Path, cwd: &Path, cols: u16) -> Pty {
        let (mut master, mut slave) = (0, 0);
        let mut size = libc::winsize {
            ws_row: ROWS,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            },
            0,
            "openpty: {}",
            std::io::Error::last_os_error()
        );
        let master = unsafe { File::from_raw_fd(master) };
        // Reads must never wait: the popup is drawn when it has something to draw, not on demand.
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            -1
        );

        let terminal = unsafe { File::from_raw_fd(slave) };
        let child = Command::new(exe())
            .current_dir(cwd)
            .env_clear()
            .env("HOME", home)
            .env("PATH", std::env::var("PATH").expect("PATH"))
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(terminal.try_clone().expect("stdin")))
            .stdout(Stdio::from(terminal.try_clone().expect("stdout")))
            .stderr(Stdio::from(terminal.try_clone().expect("stderr")))
            .spawn()
            .expect("the popup spawns");
        drop(terminal);
        Pty {
            master,
            child: Some(child),
            screen: Screen::of(ROWS as usize, cols as usize),
        }
    }

    pub fn press(&mut self, keys: &str) {
        self.master.write_all(keys.as_bytes()).expect("keys");
        self.master.flush().expect("keys");
    }

    /// The popup redraws only the cells that changed, so what it is showing lives in the screen rather
    /// than in the stream: a space that was already a space is never written twice.
    pub fn screen(&mut self) -> String {
        let mut buffer = [0u8; 16384];
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => self.screen.feed(&String::from_utf8_lossy(&buffer[..read])),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        self.screen.text()
    }

    pub fn wait_for(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let screen = self.screen();
            if screen.contains(needle) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "the popup never drew {needle:?}; it drew {screen:?}"
            );
            std::thread::sleep(STEP);
        }
    }

    #[allow(
        dead_code,
        reason = "a footer is read back without asking whether the popup lived"
    )]
    pub fn alive(&mut self) -> bool {
        matches!(
            self.child.as_mut().map(|child| child.try_wait()),
            Some(Ok(None))
        )
    }

    /// Seconds of processor time the popup has spent, at `ps`'s hundredth of a second.
    #[allow(dead_code, reason = "not every pty test weighs what the popup costs")]
    pub fn cpu(&mut self) -> f64 {
        let pid = self.child.as_ref().expect("a popup").id();
        let out = Command::new("/bin/ps")
            .args(["-o", "cputime=", "-p", &pid.to_string()])
            .output()
            .expect("ps runs");
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let (minutes, seconds) = text.split_once(':').unwrap_or(("0", &text));
        minutes.trim().parse::<f64>().unwrap_or(0.0) * 60.0 + seconds.parse::<f64>().unwrap_or(0.0)
    }

    pub fn close(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.close();
    }
}

/// What Herdr's control socket would say. Only `session.snapshot` is ever asked for here — the popup
/// resolves its project from it — and anything else is refused rather than guessed at.
pub fn answer_snapshots(socket: PathBuf, cwd: PathBuf) {
    let listener = UnixListener::bind(&socket).expect("a herdr socket of our own");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            while std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) > 0 {
                let asked: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                let id = asked.get("id").cloned().unwrap_or_default();
                let reply = match asked.get("method").and_then(|method| method.as_str()) {
                    Some("session.snapshot") => {
                        serde_json::json!({"id": id, "result": {"snapshot": {
                            "focused_pane_id": "w1:p1",
                            "focused_workspace_id": "w1",
                            "panes": [{"pane_id": "w1:p1", "workspace_id": "w1", "cwd": cwd}],
                        }}})
                    }
                    other => serde_json::json!({"id": id, "error": {
                        "code": "unsupported",
                        "message": format!("{other:?} is not answered in a test"),
                    }}),
                };
                let mut writer = &stream;
                if writeln!(writer, "{reply}").is_err() {
                    break;
                }
                line.clear();
            }
        }
    });
}

/// Enough of a terminal to hold a screen: where the cursor is, what is in each cell, and the handful of
/// escape sequences the popup's backend actually emits. Anything else is skipped rather than obeyed.
struct Screen {
    cells: Vec<Vec<char>>,
    row: usize,
    col: usize,
    /// A sequence split across two reads, kept until the rest of it arrives.
    pending: String,
}

impl Screen {
    fn of(rows: usize, cols: usize) -> Screen {
        Screen {
            cells: vec![vec![' '; cols]; rows],
            row: 0,
            col: 0,
            pending: String::new(),
        }
    }

    fn text(&self) -> String {
        self.cells
            .iter()
            .map(|row| row.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn feed(&mut self, text: &str) {
        let buffered = std::mem::take(&mut self.pending) + text;
        let mut rest = buffered.as_str();
        while let Some(character) = rest.chars().next() {
            if character != '\u{1b}' {
                self.put(character);
                rest = &rest[character.len_utf8()..];
                continue;
            }
            match sequence(rest) {
                None => break,
                Some(length) => {
                    self.apply(&rest[..length]);
                    rest = &rest[length..];
                }
            }
        }
        self.pending = rest.to_string();
    }

    fn apply(&mut self, sequence: &str) {
        let Some(parameters) = sequence.strip_prefix("\u{1b}[") else {
            return;
        };
        let (parameters, verb) = parameters.split_at(parameters.len() - 1);
        let number = |nth: usize| -> usize {
            parameters
                .split(';')
                .nth(nth)
                .and_then(|value| value.parse().ok())
                .unwrap_or(1)
        };
        match verb {
            "H" | "f" => {
                self.row = number(0).saturating_sub(1);
                self.col = number(1).saturating_sub(1);
            }
            "A" => self.row = self.row.saturating_sub(number(0)),
            "B" => self.row += number(0),
            "C" => self.col += number(0),
            "D" => self.col = self.col.saturating_sub(number(0)),
            "J" => self.cells = vec![vec![' '; self.width()]; self.cells.len()],
            "K" => {
                let width = self.width();
                if let Some(row) = self.cells.get_mut(self.row) {
                    row[self.col.min(width)..].fill(' ');
                }
            }
            _ => {}
        }
    }

    fn put(&mut self, character: char) {
        match character {
            '\n' => {
                self.row += 1;
                self.col = 0;
            }
            '\r' => self.col = 0,
            control if control.is_control() => {}
            character => {
                let width = self.width();
                if let Some(cell) = self
                    .cells
                    .get_mut(self.row)
                    .and_then(|row| row.get_mut(self.col))
                {
                    *cell = character;
                }
                self.col = (self.col + 1).min(width);
            }
        }
    }

    fn width(&self) -> usize {
        self.cells.first().map(|row| row.len()).unwrap_or_default()
    }
}

/// How long the escape sequence at the head of `text` is, or `None` while it is still arriving.
fn sequence(text: &str) -> Option<usize> {
    let mut characters = text.char_indices().skip(1);
    match characters.next() {
        None => None,
        // CSI: parameters up to a final byte, which is what says what the sequence was.
        Some((_, '[')) => characters
            .find(|(_, character)| ('\u{40}'..='\u{7e}').contains(character))
            .map(|(at, character)| at + character.len_utf8()),
        // OSC: a string terminated by BEL or by ESC.
        Some((_, ']')) => characters
            .find(|(_, character)| *character == '\u{7}' || *character == '\u{1b}')
            .map(|(at, character)| at + character.len_utf8()),
        Some((at, character)) => Some(at + character.len_utf8()),
    }
}
