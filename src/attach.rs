//! `attach` mode: the pane whose keystrokes are the unit's, and whose screen is the unit's terminal.
//!
//! Nothing here parses either direction. The pane's own terminal goes raw, so every key — an interrupt
//! among them — arrives as the bytes it is and is passed on untouched; the unit's line discipline is
//! what turns an interrupt into a signal, and it does that for the unit's process group rather than
//! for this pane. The one byte that never leaves is the detach key.
//!
//! Output comes from the unit's terminal rather than from its log, which is what makes colour, cursor
//! motion and a prompt redrawing itself render as they would in a shell. The log goes on receiving its
//! readable copy from the daemon throughout, and notices none of this.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};

use crate::client::{Attached, Endpoint, Link};

/// What the popup hands the pane at open time: one entrypoint serves every unit, as the tail pane's
/// does.
pub const PROJECT_ENV: &str = "HERDR_DEV_PROJECT";
pub const UNIT_ENV: &str = "HERDR_DEV_UNIT";
pub const ENTRYPOINT: &str = "attach";

/// Ctrl-], which is telnet's escape and which no shell, REPL or debugger binds. It has to be one of
/// those, because every other key including Ctrl-C is the unit's.
const DETACH: u8 = 0x1d;

/// How often the pane looks at how big it has become. A resize is rare and a comparison is free, which
/// is cheaper to be sure of than a signal handler that has to reach a thread blocked on a read.
const RESIZE_POLL: Duration = Duration::from_millis(200);

const CHUNK: usize = 8192;

pub fn run() -> io::Result<()> {
    let root = required(PROJECT_ENV)?;
    let unit = required(UNIT_ENV)?.to_string_lossy().into_owned();
    let root = PathBuf::from(root);

    let mut control = Endpoint::spelled_out().open().map_err(io::Error::other)?;
    let (columns, rows) = size()?;
    // Before attaching, so the unit's next draw is already for the pane it is being watched in.
    control
        .resize(&root, &unit, rows, columns)
        .map_err(io::Error::other)?;
    let attached = Endpoint::spelled_out()
        .open()
        .map_err(io::Error::other)?
        .attach(&root, &unit)
        .map_err(io::Error::other)?;

    println!("{unit}  attached  —  ctrl-] detach");
    enable_raw_mode()?;
    install_panic_hook();
    let outcome = converse(attached, control, root, unit.clone());
    let _ = disable_raw_mode();
    outcome
}

fn converse(attached: Attached, mut control: Link, root: PathBuf, unit: String) -> io::Result<()> {
    let Attached {
        mut reading,
        writing,
    } = attached;
    let name = unit.clone();

    typing(writing);
    watch_size(move |rows, columns| {
        let _ = control.resize(&root, &unit, rows, columns);
    });

    let mut buffer = [0u8; CHUNK];
    let mut out = io::stdout();
    loop {
        let read = match reading.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        out.write_all(&buffer[..read])?;
        out.flush()?;
    }
    // The stream running out is the unit going, which is worth saying: an attached pane that simply
    // stopped answering reads as a hang.
    let _ = disable_raw_mode();
    println!("\r\n{name} has exited");
    Ok(())
}

/// Detaching leaves the unit exactly as it was: nothing here signals it, and nothing here waits for
/// it.
fn typing(mut writing: impl Write + Send + 'static) {
    std::thread::spawn(move || {
        let mut buffer = [0u8; CHUNK];
        let mut stdin = io::stdin();
        while let Ok(read) = stdin.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let typed = &buffer[..read];
            match typed.iter().position(|byte| *byte == DETACH) {
                // Whatever was typed before it still belongs to the unit.
                Some(at) => {
                    let _ = writing.write_all(&typed[..at]);
                    let _ = disable_raw_mode();
                    println!("\r\ndetached from the unit, which is still running");
                    std::process::exit(0);
                }
                None => {
                    if writing.write_all(typed).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn watch_size(mut resized: impl FnMut(u16, u16) + Send + 'static) {
    std::thread::spawn(move || {
        let mut last = size().unwrap_or_default();
        loop {
            std::thread::sleep(RESIZE_POLL);
            let Ok(now) = size() else { continue };
            if now != last {
                last = now;
                let (columns, rows) = now;
                resized(rows, columns);
            }
        }
    });
}

fn required(key: &str) -> io::Result<std::ffi::OsString> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::other(format!(
                "{key} is unset: the unit to attach to is passed in at plugin.pane.open"
            ))
        })
}

/// A pane that panicked in raw mode would cost the user their keyboard until they closed it.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        previous(info);
        std::process::exit(1);
    }));
}
