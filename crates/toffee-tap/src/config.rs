//! Config schema.
//!
//! filebeat-style: declarative TOML, one `[[source]]` block per source.
//! No DSL — just enough to drive the runner.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Optional registry path. Defaults to
    /// `$XDG_STATE_HOME/toffee/tap.registry.json`.
    #[serde(default)]
    pub registry: Option<PathBuf>,
    #[serde(default, rename = "source")]
    pub sources: Vec<SourceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceConfig {
    ClaudeCode(FileSourceConfig),
    Codex(FileSourceConfig),
    Stdin(StdinSourceConfig),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileSourceConfig {
    /// Override discovery root. Defaults to the source's standard
    /// location.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Explicit scope. If absent, the mapper tries `scope_auto`.
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    /// Infer scope from the turn's `cwd_hint`. Default true.
    #[serde(default = "default_true")]
    pub scope_auto: bool,
    /// Fallback scope when both override and auto-inference produce
    /// nothing.
    #[serde(default)]
    pub scope_fallback: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StdinSourceConfig {
    /// Optional "user prefix" marker (e.g. `"> "`). Lines starting with
    /// this prefix begin a user turn; everything else is the assistant.
    #[serde(default)]
    pub user_prefix: Option<String>,
    /// Explicit scope. Required for stdin since we can't infer from a cwd.
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    #[serde(default)]
    pub scope_fallback: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Config {
    /// The CLI default if no `--config` is supplied: a Claude Code + Codex
    /// pair pointing at standard paths, both with auto scope.
    pub fn default_pair() -> Self {
        Config {
            registry: None,
            sources: vec![
                SourceConfig::ClaudeCode(FileSourceConfig {
                    path: None,
                    scope: None,
                    scope_auto: true,
                    scope_fallback: vec!["global".into()],
                }),
                SourceConfig::Codex(FileSourceConfig {
                    path: None,
                    scope: None,
                    scope_auto: true,
                    scope_fallback: vec!["global".into()],
                }),
            ],
        }
    }

    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let toml = r#"
            registry = "/tmp/r.json"

            [[source]]
            kind = "claude_code"
            scope = ["project:foo"]
            scope_auto = false

            [[source]]
            kind = "codex"
            path = "/custom/codex"

            [[source]]
            kind = "stdin"
            user_prefix = "> "
            scope = ["project:tmux"]
        "#;
        let c = Config::from_toml(toml).unwrap();
        assert_eq!(c.sources.len(), 3);
        match &c.sources[0] {
            SourceConfig::ClaudeCode(f) => {
                assert_eq!(f.scope.as_deref(), Some(&["project:foo".to_string()][..]))
            }
            _ => panic!(),
        }
    }
}
