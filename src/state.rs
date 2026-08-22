//! The state root, spelled out.
//!
//! It is the exact path Herdr builds for a plugin id, written out here rather than read from the
//! environment: the TUI is handed no `HERDR_PLUGIN_STATE_DIR` while the tail overlay is, and the two
//! must agree.

use std::path::{Path, PathBuf};

use crate::herdr::expand_tilde;

const ROOT: &str = ".local/state/herdr/plugins/herdr-dev";

pub fn root() -> PathBuf {
    expand_tilde("~").join(ROOT)
}

pub fn socket_path(root: &Path) -> PathBuf {
    root.join("daemon.sock")
}

pub fn lock_path(root: &Path) -> PathBuf {
    root.join("daemon.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_is_the_plugin_state_directory_under_home() {
        let home = std::env::var("HOME").expect("HOME");
        assert_eq!(
            root(),
            PathBuf::from(format!("{home}/.local/state/herdr/plugins/herdr-dev"))
        );
    }

    #[test]
    fn the_socket_and_the_lock_sit_side_by_side_in_the_root() {
        let root = PathBuf::from("/state/herdr-dev");
        assert_eq!(socket_path(&root), root.join("daemon.sock"));
        assert_eq!(lock_path(&root), root.join("daemon.lock"));
    }
}
