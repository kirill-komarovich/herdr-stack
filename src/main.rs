use herdr_stack::attach;
use herdr_stack::cli;
use herdr_stack::daemon;
use herdr_stack::mode::{self, Mode, USAGE};
use herdr_stack::tail;
use herdr_stack::tui;

fn main() {
    let mode = match mode::from_args(std::env::args().skip(1)) {
        Ok(mode) => mode,
        Err(error) => {
            eprintln!("herdr-stack: {error}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    match mode {
        Mode::Tui => {
            if let Err(error) = tui::run() {
                eprintln!("herdr-stack: {error}");
                std::process::exit(1);
            }
        }
        Mode::Daemon { root } => {
            if let Err(error) = daemon::serve(&root) {
                eprintln!("herdr-stack: {error}");
                std::process::exit(1);
            }
        }
        Mode::Tail => {
            if let Err(error) = tail::run() {
                eprintln!("herdr-stack: {error}");
                std::process::exit(1);
            }
        }
        Mode::Attach => {
            if let Err(error) = attach::run() {
                eprintln!("herdr-stack: {error}");
                std::process::exit(1);
            }
        }
        // The agent-facing modes answer on stdout and complain on stderr, so a caller may parse the
        // one without stripping the other, and say by their exit status whether the verb took.
        Mode::Status { ask } => match cli::status(&ask) {
            Ok(said) => cli::print(&said),
            Err(error) => {
                eprintln!("herdr-stack: {error}");
                std::process::exit(1);
            }
        },
        Mode::Control { act, ask, units } => match cli::control(act, &ask, &units) {
            Ok((said, done)) => {
                cli::print(&said);
                if !done {
                    std::process::exit(1);
                }
            }
            Err(error) => {
                eprintln!("herdr-stack: {error}");
                std::process::exit(1);
            }
        },
    }
}
