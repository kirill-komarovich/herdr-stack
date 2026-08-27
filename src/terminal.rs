//! The master end of the terminal a `tty = true` unit runs on.
//!
//! One thread reads it and nothing else does, because two readers of one fd would each get half the
//! output. What it reads goes two ways at once: to every attached pane exactly as the unit wrote it,
//! and through `readable` into the log, which has three readers and no way to render an escape
//! sequence between them.
//!
//! Draining is not optional. A terminal nobody reads fills, and the unit writing to it blocks — so the
//! reader starts at the fork rather than when somebody first attaches.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::readable;

/// What a unit nobody has attached to is sized at. Zero is a real answer to `TIOCGWINSZ` and a program
/// that gets one draws for a screen nothing fits on.
pub const ROWS: u16 = 24;
pub const COLUMNS: u16 = 80;

/// `_IOW('t', 103, struct winsize)`, which `libc` spells out for every BSD except apple.
const TIOCSWINSZ: libc::c_ulong = 0x8008_7467;

const CHUNK: usize = 8192;

/// Said in two places — by the popup refusing to open a pane, and by the daemon refusing to hand one
/// over — so it is worded once.
pub fn runs_on_a_pipe(name: &str) -> String {
    format!("{name} runs on a pipe; declare `tty = true` to type at it")
}

#[derive(Debug)]
pub struct Terminal {
    master: File,
    watchers: Mutex<Vec<Sender<Vec<u8>>>>,
}

/// Both ends of a terminal that has just been opened. The slave belongs to the child; the parent has
/// to let go of its copy once the fork has taken one, or the master never sees the output end.
#[derive(Debug)]
pub struct Pair {
    pub terminal: Arc<Terminal>,
    pub slave: OwnedFd,
}

/// `openpty(3)` rather than `posix_openpt` and `ptsname`, whose name buffer is one per process: the
/// daemon spawns from a thread per client.
pub fn open() -> std::io::Result<Pair> {
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: ROWS,
        ws_col: COLUMNS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if opened == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Pair {
        terminal: Arc::new(Terminal {
            master: File::from(unsafe { OwnedFd::from_raw_fd(master) }),
            watchers: Mutex::new(Vec::new()),
        }),
        slave: unsafe { OwnedFd::from_raw_fd(slave) },
    })
}

impl Terminal {
    /// Keystrokes, as the unit's own terminal delivers them: an interrupt among them is the line
    /// discipline's to turn into a signal, not ours.
    pub fn typed(&self, bytes: &[u8]) -> std::io::Result<()> {
        (&self.master).write_all(bytes)
    }

    /// The unit's idea of how much room it has. The last pane to say wins, which is the only answer
    /// available when two of them disagree.
    pub fn resize(&self, rows: u16, columns: u16) -> std::io::Result<()> {
        let size = libc::winsize {
            ws_row: rows.max(1),
            ws_col: columns.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        match unsafe { libc::ioctl(self.master.as_raw_fd(), TIOCSWINSZ, &size) } {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        }
    }

    /// Everything the unit writes from here on, raw. Dropping the receiver is how a pane leaves.
    pub fn watch(&self) -> Receiver<Vec<u8>> {
        let (chunks, watched) = channel();
        self.watchers.lock().expect("watchers").push(chunks);
        watched
    }

    /// The unit has gone, so every watch ends: a pane learns that the thing it was typing at is no
    /// longer there by the stream under it running out.
    fn hang_up(&self) {
        self.watchers.lock().expect("watchers").clear();
    }

    /// A pane that has gone stops being written to rather than holding the unit's output up.
    fn broadcast(&self, chunk: &[u8]) {
        self.watchers
            .lock()
            .expect("watchers")
            .retain(|watcher| watcher.send(chunk.to_vec()).is_ok());
    }
}

/// The one reader of the master, on a thread of its own. It ends when the unit does: the last copy of
/// the slave closing is what the read finally fails on.
pub fn pump(terminal: Arc<Terminal>, mut log: File) {
    std::thread::spawn(move || {
        let mut filter = readable::Filter::new();
        let mut buffer = [0u8; CHUNK];
        // A closed slave reads as `EIO` here rather than as end-of-file, so an error is the end too.
        while let Ok(read) = (&terminal.master).read(&mut buffer) {
            if read == 0 {
                break;
            }
            let chunk = &buffer[..read];
            terminal.broadcast(chunk);
            for line in filter.absorb(chunk) {
                let _ = writeln!(log, "{line}");
            }
        }
        // What a unit wrote without a newline after it is still what it had to say — a crash message
        // and a prompt both arrive that way.
        if let Some(last) = filter.rest() {
            let _ = writeln!(log, "{last}");
        }
        terminal.hang_up();
    });
}

/// Installs `slave` as the child's controlling terminal and its whole stdio. Only ever called between
/// fork and exec, where `login_tty(3)` is `setsid`, `TIOCSCTTY` and three `dup2`s in one call that is
/// safe to make there.
pub fn take_over(slave: &OwnedFd) -> impl FnMut() -> std::io::Result<()> + Send + Sync + 'static {
    let slave = slave.as_raw_fd();
    move || match unsafe { libc::login_tty(slave) } {
        -1 => Err(std::io::Error::last_os_error()),
        _ => Ok(()),
    }
}
