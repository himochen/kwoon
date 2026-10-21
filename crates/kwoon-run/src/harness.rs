//! What kwoon knows about each supported harness: the settings it seeds, the options
//! that turn the harness's inner sandbox off, where the harness keeps its provider
//! credential and its session log, and how a log shows a model response.

use std::ffi::OsString;
use std::path::Path;

use crate::contain::Component;
use crate::home::SeedFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    ClaudeCode,
    Codex,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::ClaudeCode, Kind::Codex];

    pub fn name(self) -> &'static str {
        match self {
            Kind::ClaudeCode => "claude-code",
            Kind::Codex => "codex",
        }
    }

    pub fn from_name(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// The settings file kwoon writes itself: the inner sandbox off, and no hooks. The
    /// home is otherwise empty, so no user plugin or MCP server exists to load. The
    /// command line also refuses MCP servers (Claude Code) and hooks, plugins, and apps
    /// (Codex); see [`Kind::command`].
    ///
    /// Only documented keys are used: Claude Code with `-p` silently ignores a settings
    /// file that fails validation, which would drop every setting in it.
    pub fn settings(self) -> SeedFile {
        match self {
            Kind::ClaudeCode => SeedFile {
                path: CLAUDE_SETTINGS.into(),
                bytes: br#"{
  "sandbox": { "enabled": false },
  "disableAllHooks": true
}
"#
                .to_vec(),
            },
            Kind::Codex => SeedFile {
                path: ".codex/config.toml".into(),
                bytes: br#"sandbox_mode = "danger-full-access"
approval_policy = "never"
"#
                .to_vec(),
            },
        }
    }

    /// Where a provider credential given as a file is copied, relative to the home.
    pub fn credential_file(self) -> &'static str {
        match self {
            Kind::ClaudeCode => ".claude/.credentials.json",
            Kind::Codex => ".codex/auth.json",
        }
    }

    /// The command for one non-interactive prompt, with the inner sandbox off.
    pub fn command(self, executable: &Path, home: &Path, prompt: &str) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec![executable.into()];
        match self {
            Kind::ClaudeCode => {
                argv.extend(["-p".into(), prompt.into(), "--settings".into()]);
                argv.push(home.join(CLAUDE_SETTINGS).into());
                // No --mcp-config is given, so no MCP server is loaded from anywhere.
                argv.push("--strict-mcp-config".into());
            }
            Kind::Codex => {
                argv.extend(
                    [
                        "exec",
                        "--sandbox",
                        "danger-full-access",
                        "--skip-git-repo-check",
                        "--disable",
                        "hooks",
                        "--disable",
                        "plugins",
                        "--disable",
                        "apps",
                    ]
                    .map(OsString::from),
                );
                argv.push(prompt.into());
            }
        }
        argv
    }

    /// Variables kwoon sets for every run of this harness, before host settings.
    pub fn default_env(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Kind::ClaudeCode => &[("DISABLE_AUTOUPDATER", "1")],
            Kind::Codex => &[],
        }
    }

    /// The session-log files, relative to the home.
    pub fn log_pattern(self) -> &'static [Component] {
        match self {
            Kind::ClaudeCode => &[
                Component::Name(".claude"),
                Component::Name("projects"),
                Component::Any {
                    prefix: "",
                    suffix: "",
                },
                Component::Any {
                    prefix: "",
                    suffix: ".jsonl",
                },
            ],
            Kind::Codex => &[
                Component::Name(".codex"),
                Component::Name("sessions"),
                Component::Any {
                    prefix: "",
                    suffix: "",
                },
                Component::Any {
                    prefix: "",
                    suffix: "",
                },
                Component::Any {
                    prefix: "",
                    suffix: "",
                },
                Component::Any {
                    prefix: "rollout-",
                    suffix: ".jsonl",
                },
            ],
        }
    }

    /// Whether one session log holds a response from the model. The log is untrusted
    /// data: it is parsed line by line and never echoed.
    pub fn log_shows_model_response(self, log: &[u8]) -> bool {
        log.split(|b| *b == b'\n')
            .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
            .any(|entry| match self {
                Kind::ClaudeCode => {
                    entry["type"] == "assistant"
                        && entry["message"]["role"] == "assistant"
                        && entry["message"]["content"]
                            .as_array()
                            .is_some_and(|c| !c.is_empty())
                }
                Kind::Codex => {
                    entry["type"] == "response_item"
                        && entry["payload"]["type"] == "message"
                        && entry["payload"]["role"] == "assistant"
                }
            })
    }
}

const CLAUDE_SETTINGS: &str = ".claude/settings.json";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_model_response_only_in_well_formed_entries() {
        let claude = Kind::ClaudeCode;
        assert!(claude.log_shows_model_response(
            br#"{"type":"user","message":{"role":"user","content":"hi"}}
{"type":"assistant","message":{"role":"assistant","model":"m","content":[{"type":"text","text":"ready"}]}}
"#
        ));
        assert!(!claude.log_shows_model_response(
            br#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#
        ));
        assert!(!claude.log_shows_model_response(b"not json\n"));

        let codex = Kind::Codex;
        assert!(codex.log_shows_model_response(
            br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}"#
        ));
        assert!(!codex.log_shows_model_response(
            br#"{"type":"response_item","payload":{"type":"message","role":"user"}}"#
        ));
    }
}
