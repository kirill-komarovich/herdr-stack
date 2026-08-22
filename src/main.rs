use herdr_dev::daemon;
use herdr_dev::mode::{self, Mode, USAGE};
use herdr_dev::tail;
use herdr_dev::tui;

fn main() {
    let mode = match mode::from_args(std::env::args().skip(1)) {
        Ok(mode) => mode,
        Err(error) => {
            eprintln!("herdr-dev: {error}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    match mode {
        Mode::Tui => {
            if let Err(error) = tui::run() {
                eprintln!("herdr-dev: {error}");
                std::process::exit(1);
            }
        }
        Mode::Daemon { root } => {
            if let Err(error) = daemon::serve(&root) {
                eprintln!("herdr-dev: {error}");
                std::process::exit(1);
            }
        }
        Mode::Tail => {
            if let Err(error) = tail::run() {
                eprintln!("herdr-dev: {error}");
                std::process::exit(1);
            }
        }
    }
}
