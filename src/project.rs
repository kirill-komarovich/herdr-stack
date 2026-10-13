//! Which project a keypress was about.
//!
//! The answer comes from Herdr's session model, never from the inherited cwd: a popup's cwd has no
//! defined relationship to what the user was looking at.

use std::path::{Path, PathBuf};

use crate::herdr::Snapshot;
use crate::manifest::Project;

pub const MANIFEST_NAME: &str = ".herdr-stack.toml";

/// Walks `start` and its parents, innermost first.
pub fn nearest_manifest(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|dir| {
        let candidate = dir.join(MANIFEST_NAME);
        candidate.is_file().then_some(candidate)
    })
}

pub fn locate(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .find_map(|start| nearest_manifest(&start))
}

/// Everything the first frame needs: the project if one was found, and one line of complaint if
/// anything went wrong along the way.
#[derive(Debug)]
pub struct Resolution {
    pub project: Option<Project>,
    pub complaint: Option<String>,
}

impl Resolution {
    pub fn nothing() -> Resolution {
        Resolution {
            project: None,
            complaint: None,
        }
    }

    pub fn resolve() -> Resolution {
        match Snapshot::fetch() {
            Ok(snapshot) => Resolution::from_snapshot(&snapshot),
            Err(error) => Resolution {
                project: None,
                complaint: Some(error.to_string()),
            },
        }
    }

    pub fn from_snapshot(snapshot: &Snapshot) -> Resolution {
        let mut candidates: Vec<PathBuf> = snapshot.focused_pane_cwd().into_iter().collect();
        candidates.extend(snapshot.focused_workspace_cwds());
        match locate(candidates) {
            None => Resolution::nothing(),
            Some(manifest) => match Project::load(&manifest) {
                Ok(project) => Resolution {
                    project: Some(project),
                    complaint: None,
                },
                Err(error) => Resolution {
                    project: None,
                    complaint: Some(error.to_string()),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Tree {
            let root = std::env::temp_dir().join(format!("herdr-stack-project-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("temp tree");
            Tree(root)
        }

        fn dir(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(&path).expect("temp dir");
            path
        }

        fn manifest(&self, relative: &str) -> PathBuf {
            let path = self.dir(relative).join(MANIFEST_NAME);
            std::fs::write(&path, "[local.rails]\ncmd = [\"rails\", \"s\"]\n").expect("manifest");
            path
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_pane_deeper_than_the_root_still_finds_the_manifest() {
        let tree = Tree::new("deep");
        let manifest = tree.manifest("storefront");
        let deep = tree.dir("storefront/app/models/concerns");
        assert_eq!(nearest_manifest(&deep), Some(manifest));
    }

    #[test]
    fn the_innermost_manifest_wins() {
        let tree = Tree::new("nested");
        tree.manifest("outer");
        let inner = tree.manifest("outer/inner");
        assert_eq!(nearest_manifest(&tree.dir("outer/inner/src")), Some(inner));
    }

    #[test]
    fn a_directory_with_nothing_above_it_finds_nothing() {
        let tree = Tree::new("bare");
        assert_eq!(nearest_manifest(&tree.dir("nowhere/deeper")), None);
    }

    #[test]
    fn the_pane_beats_the_workspace() {
        let tree = Tree::new("pane-first");
        let pane_manifest = tree.manifest("checkout-a");
        tree.manifest("checkout-b");
        let snapshot = Snapshot::from_value(json!({
            "focused_workspace_id": "w1",
            "focused_pane_id": "w1:p1",
            "workspaces": [{"workspace_id": "w1", "worktree": {
                "checkout_path": tree.dir("checkout-b").to_str().unwrap(),
            }}],
            "panes": [{
                "pane_id": "w1:p1",
                "workspace_id": "w1",
                "cwd": tree.dir("checkout-a/app").to_str().unwrap(),
            }],
        }));
        let resolution = Resolution::from_snapshot(&snapshot);
        assert_eq!(
            resolution.project.map(|project| project.manifest),
            Some(pane_manifest)
        );
    }

    #[test]
    fn the_workspace_answers_when_the_pane_sits_outside_any_project() {
        let tree = Tree::new("workspace-fallback");
        let workspace_manifest = tree.manifest("checkout-b");
        let snapshot = Snapshot::from_value(json!({
            "focused_workspace_id": "w1",
            "focused_pane_id": "w1:p1",
            "workspaces": [{"workspace_id": "w1"}],
            "panes": [
                {"pane_id": "w1:p1", "workspace_id": "w1", "cwd": tree.dir("elsewhere").to_str().unwrap()},
                {"pane_id": "w1:p2", "workspace_id": "w1", "cwd": tree.dir("checkout-b/lib").to_str().unwrap()},
            ],
        }));
        let resolution = Resolution::from_snapshot(&snapshot);
        assert_eq!(
            resolution.project.map(|project| project.manifest),
            Some(workspace_manifest)
        );
    }

    #[test]
    fn no_manifest_anywhere_is_the_empty_state_without_a_complaint() {
        let tree = Tree::new("empty-state");
        let snapshot = Snapshot::from_value(json!({
            "focused_workspace_id": "w1",
            "focused_pane_id": "w1:p1",
            "panes": [{
                "pane_id": "w1:p1",
                "workspace_id": "w1",
                "cwd": tree.dir("nowhere").to_str().unwrap(),
            }],
        }));
        let resolution = Resolution::from_snapshot(&snapshot);
        assert!(resolution.project.is_none());
        assert_eq!(resolution.complaint, None);
    }

    #[test]
    fn a_broken_manifest_complains_instead_of_being_found() {
        let tree = Tree::new("broken");
        let dir = tree.dir("storefront");
        std::fs::write(dir.join(MANIFEST_NAME), "[local.rails\n").expect("manifest");
        let snapshot = Snapshot::from_value(json!({
            "focused_pane_id": "w1:p1",
            "panes": [{"pane_id": "w1:p1", "cwd": dir.to_str().unwrap()}],
        }));
        let resolution = Resolution::from_snapshot(&snapshot);
        assert!(resolution.project.is_none());
        assert!(
            resolution
                .complaint
                .unwrap_or_default()
                .contains(MANIFEST_NAME)
        );
    }
}
