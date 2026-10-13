//! Tools found where they are installed rather than on `PATH`: a popup, and the daemon it starts,
//! inherit no `PATH` worth trusting.

use std::path::PathBuf;

use crate::herdr::expand_tilde;

/// First found wins, so a user-local install beats the system's as it would on a login shell's `PATH`.
pub fn first(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(expand_tilde)
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_install_found_is_the_one_used() {
        let dir =
            std::env::temp_dir().join(format!("herdr-stack-installed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let second = dir.join("second");
        let third = dir.join("third");
        std::fs::write(&second, "").expect("second");
        std::fs::write(&third, "").expect("third");
        let candidates = [dir.join("first"), second.clone(), third]
            .map(|path| path.to_string_lossy().into_owned());
        let found = first(&candidates.each_ref().map(String::as_str));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, Some(second));
    }
}
