//! The agent-config form: one field, edited in place, saved to the plugin's own config.
//!
//! Deliberately not a text editor: no cursor movement, no selection. It exists so the empty state can
//! be escaped without leaving the popup, and it is the only place the TUI writes anything.

use crate::agent::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub command: String,
}

impl Form {
    /// Pre-filled from the current config when there is one, so editing never silently drops a value.
    pub fn open() -> Form {
        let command = Config::load()
            .map(|config| config.command.join(" "))
            .unwrap_or_default();
        Form { command }
    }

    pub fn insert(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        self.command.push(c);
    }

    pub fn backspace(&mut self) {
        self.command.pop();
    }

    /// Split on whitespace into argv, because each word is quoted separately before being typed into a
    /// shell: one string holding two words would be quoted as a single filename.
    pub fn to_config(&self) -> Result<Config, String> {
        let command: Vec<String> = self
            .command
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if command.is_empty() {
            return Err("command is empty".into());
        }
        Ok(Config { command })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(command: &str) -> Form {
        Form {
            command: command.to_string(),
        }
    }

    #[test]
    fn typing_appends_and_backspace_removes() {
        let mut form = form("");
        form.insert('c');
        form.insert('x');
        form.backspace();
        assert_eq!(form.command, "c");
    }

    #[test]
    fn backspace_on_an_empty_field_is_not_an_error() {
        let mut form = form("");
        form.backspace();
        assert_eq!(form.command, "");
    }

    #[test]
    fn control_characters_never_enter_the_field() {
        let mut form = form("");
        form.insert('\t');
        form.insert('\n');
        assert_eq!(form.command, "");
    }

    #[test]
    fn a_multi_word_command_becomes_argv() {
        let config = form(" my-wrapper  --flag ").to_config().unwrap();
        assert_eq!(config.command, vec!["my-wrapper", "--flag"]);
    }

    #[test]
    fn a_blank_command_is_rejected() {
        assert!(form("   ").to_config().is_err());
        assert!(form("").to_config().is_err());
    }
}
