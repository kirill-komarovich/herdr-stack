//! Handing manifest-writing to a coding agent.
//!
//! The plugin never writes a `.herdr-dev.toml` itself — which services you actually start, which are
//! containerised duplicates of a local unit, and which exiting service is a one-shot are judgement
//! calls a parser cannot make. So the empty state splits a pane, runs the configured agent in it and
//! asks it to run the `herdr-dev-manifest` skill.
//!
//! **The plugin knows nothing about how the agent is installed.** Its whole configuration is a `command`
//! to run, which may be a wrapper, a profile launcher, or a canonical binary. `agent.start` is deliberately not used:
//! it can only run a kind's canonical executable, which would ignore a wrapper.
//!
//! The prompt rides in as the command's own argument, so the agent comes up with the work already
//! submitted. Nothing waits for Herdr to recognise the agent first.

use std::path::{Path, PathBuf};

use serde_json::json;
use toml_edit::DocumentMut;

use crate::herdr::{self, Error, Snapshot, expand_tilde};

const CONFIG: &str = ".config/herdr/plugins/config/herdr-dev/config.toml";

/// The skill is user-invocation only, so it has to be called the way a person would call it.
pub const SKILL_COMMAND: &str = "/herdr-dev-manifest";

pub fn config_path() -> PathBuf {
    expand_tilde("~").join(CONFIG)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub command: Vec<String>,
}

impl Config {
    pub fn load() -> Result<Config, String> {
        let path = config_path();
        let text = std::fs::read_to_string(&path).map_err(|_| {
            format!(
                "no agent configured: write {} with [agent] command",
                path.display()
            )
        })?;
        Config::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let doc = text
            .parse::<DocumentMut>()
            .map_err(|error| format!("agent config: {error}"))?;
        let table = doc
            .get("agent")
            .and_then(|item| item.as_table_like())
            .ok_or("agent config: no [agent] table")?;

        let command: Vec<String> = match table.get("command") {
            Some(item) if item.as_str().is_some() => {
                vec![item.as_str().unwrap_or_default().to_string()]
            }
            Some(item) => item
                .as_array()
                .ok_or("agent config: [agent] command must be a string or an array of strings")?
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_string).ok_or_else(|| {
                        "agent config: [agent] command holds a non-string".to_string()
                    })
                })
                .collect::<Result<_, _>>()?,
            None => return Err("agent config: [agent] command is missing".into()),
        };
        if command.iter().all(|word| word.trim().is_empty()) {
            return Err("agent config: [agent] command is empty".into());
        }

        Ok(Config { command })
    }

    fn command_line(&self) -> String {
        self.command
            .iter()
            .map(|word| quote(word))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The prompt is one argument, so an agent that takes an opening prompt starts with it submitted.
    pub fn command_line_with(&self, prompt: &str) -> String {
        format!("{} {}", self.command_line(), quote(prompt))
    }
}

/// The pane's shell reads this as a command line, so anything outside a conservative set is quoted.
fn quote(word: &str) -> String {
    let plain = word
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '=' | ':' | '@'));
    if plain && !word.is_empty() {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Writes only the two keys it owns, so anything else in the file survives being edited from the TUI.
pub fn save(config: &Config) -> Result<(), String> {
    save_to(&config_path(), config)
}

pub fn save_to(path: &Path, config: &Config) -> Result<(), String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc = existing
        .parse::<DocumentMut>()
        .map_err(|error| format!("agent config: {error}"))?;

    if doc
        .get("agent")
        .and_then(|item| item.as_table_like())
        .is_none()
    {
        doc["agent"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    // `harness` was read by the detection wait, which the prompt-as-argument launch removed.
    if let Some(table) = doc["agent"].as_table_like_mut() {
        table.remove("harness");
    }
    let mut command = toml_edit::Array::new();
    for word in &config.command {
        command.push(word.as_str());
    }
    doc["agent"]["command"] = toml_edit::value(command);

    let parent = path
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, doc.to_string())
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| format!("{}: {error}", path.display()))
}

#[derive(Debug)]
pub struct Launched {
    pub pane_id: String,
    pub dir: PathBuf,
}

/// Where the manifest should land: the repository holding the pane the user was looking at.
pub fn target_dir() -> Option<PathBuf> {
    let pane_cwd = active_pane_cwd().or_else(|| {
        Snapshot::fetch()
            .ok()
            .and_then(|snapshot| snapshot.focused_pane_cwd())
    })?;
    Some(repo_root(&pane_cwd))
}

/// A `[[keys.command]]` popup inherits `HERDR_ACTIVE_PANE_CWD`, so the common case costs no round
/// trip. Measured on this machine; plugin panes get a `HERDR_PLUGIN_*` family instead.
fn active_pane_cwd() -> Option<PathBuf> {
    let cwd = std::env::var_os("HERDR_ACTIVE_PANE_CWD")?;
    if cwd.is_empty() {
        return None;
    }
    Some(expand_tilde(PathBuf::from(cwd)))
}

fn repo_root(from: &Path) -> PathBuf {
    from.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(from)
        .to_path_buf()
}

pub fn prompt(dir: &Path) -> String {
    format!("{SKILL_COMMAND} {}", dir.display())
}

pub fn launch(config: &Config, dir: &Path) -> Result<Launched, String> {
    let pane = herdr::request(
        "pane.split",
        json!({"direction": "right", "cwd": dir, "focus": true}),
    )
    .map_err(stringify)?;
    let pane_id = pane
        .get("pane")
        .and_then(|pane| pane.get("pane_id"))
        .and_then(|id| id.as_str())
        .ok_or("pane.split: reply carries no pane id")?
        .to_string();

    herdr::request(
        "pane.send_input",
        json!({"pane_id": pane_id, "text": config.command_line_with(&prompt(dir)), "keys": ["enter"]}),
    )
    .map_err(stringify)?;

    Ok(Launched {
        pane_id,
        dir: dir.to_path_buf(),
    })
}

fn stringify(error: Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_repository_root_wins_over_a_pane_sitting_deeper() {
        let root = std::env::temp_dir().join(format!("herdr-dev-agent-{}", std::process::id()));
        let deep = root.join("app/models");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(repo_root(&deep), root);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_directory_outside_any_repository_is_its_own_target() {
        let dir = std::env::temp_dir();
        assert_eq!(repo_root(&dir), dir);
    }

    #[test]
    fn the_prompt_calls_the_skill_as_a_person_would_and_names_the_directory() {
        assert_eq!(
            prompt(Path::new("/repos/storefront")),
            "/herdr-dev-manifest /repos/storefront"
        );
    }

    #[test]
    fn the_prompt_rides_in_as_one_quoted_argument() {
        let config = Config {
            command: vec!["my-wrapper".into()],
        };
        assert_eq!(
            config.command_line_with("/herdr-dev-manifest /repos/a b"),
            "my-wrapper '/herdr-dev-manifest /repos/a b'"
        );
    }

    #[test]
    fn a_command_may_be_a_bare_string_or_an_argv_array() {
        let one = Config::parse("[agent]\ncommand = \"my-wrapper\"\n").unwrap();
        assert_eq!(one.command, vec!["my-wrapper".to_string()]);
        let many = Config::parse(
            "[agent]\nharness = \"claude\"\ncommand = [\"my-wrapper\", \"--flag\"]\n",
        )
        .unwrap();
        assert_eq!(many.command_line(), "my-wrapper --flag");
    }

    #[test]
    fn a_command_with_shell_metacharacters_is_quoted_not_executed() {
        let harness = Config::parse("[agent]\ncommand = [\"a b; rm -rf /\"]\n").unwrap();
        assert_eq!(harness.command_line(), "'a b; rm -rf /'");
    }

    #[test]
    fn a_config_without_an_agent_table_says_so() {
        let error = Config::parse("[other]\nx = 1\n").unwrap_err();
        assert!(error.contains("[agent]"));
    }

    #[test]
    fn a_command_is_required_and_a_stale_harness_key_is_tolerated() {
        assert!(Config::parse("[agent]\n").is_err());
        assert!(Config::parse("[agent]\ncommand = \"\"\n").is_err());
        assert_eq!(
            Config::parse("[agent]\nharness = \"claude\"\ncommand = \"x\"\n")
                .unwrap()
                .command,
            vec!["x".to_string()]
        );
    }

    #[test]
    fn saving_then_loading_round_trips_and_keeps_a_foreign_key() {
        let dir = std::env::temp_dir().join(format!("herdr-dev-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# mine\nkeep_me = true\n\n[agent]\nharness = \"old\"\ncommand = \"old-binary\"\n",
        )
        .unwrap();

        let config = Config {
            command: vec!["my-wrapper".into(), "--flag".into()],
        };
        save_to(&path, &config).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(Config::parse(&written).unwrap(), config);
        assert!(!written.contains("harness"));
        assert!(written.contains("keep_me = true"));
        assert!(written.contains("# mine"));
        assert!(!written.contains("old-binary"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_into_a_directory_that_does_not_exist_yet_creates_it() {
        let dir = std::env::temp_dir().join(format!("herdr-dev-cfg-new-{}", std::process::id()));
        let path = dir.join("nested/config.toml");
        save_to(
            &path,
            &Config {
                command: vec!["codex".into()],
            },
        )
        .unwrap();
        assert!(path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
