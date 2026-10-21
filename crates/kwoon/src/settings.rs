//! Host settings (plan D6, "Policy file and host settings"): a TOML file in kwoon's
//! configuration directory. An isolated verification context has its own file, with
//! its own state directory, credentials, and fixture allowlist.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const FILE_NAME: &str = "settings.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub context: Context,
    pub paths: Paths,
    #[serde(default)]
    pub credentials: BTreeMap<String, ForgeCredential>,
    #[serde(default)]
    pub harness: BTreeMap<String, Harness>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Context {
    Normal,
    /// The isolated verification context. Its fixture repositories and endpoints are
    /// the only ones it may contact.
    Test {
        fixture_repositories: Vec<String>,
        fixture_endpoints: Vec<String>,
    },
}

impl Context {
    pub fn classification(&self) -> &'static str {
        match self {
            Context::Normal => "normal",
            Context::Test { .. } => "test",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    /// kwoon's state directory. Never bound into a wrapper.
    pub state_dir: PathBuf,
    /// Where run directories (run clone and private run home) are created. It must lie
    /// outside the state and configuration directories.
    pub runs_dir: PathBuf,
    #[serde(default = "default_bwrap")]
    pub bwrap: PathBuf,
}

fn default_bwrap() -> PathBuf {
    "/usr/bin/bwrap".into()
}

/// One of kwoon's forge credentials. T2 adds its kind, owner, and routing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeCredential {
    pub file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    pub executable: PathBuf,
    /// Host directories the harness needs (its installation), bound read-only.
    #[serde(default)]
    pub ro_binds: Vec<PathBuf>,
    pub timeout_seconds: u64,
    pub provider_credential: Option<ProviderCredential>,
    /// Variables set inside the wrapper, besides `HOME`, `PATH`, `LANG`, and `TERM`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Where the provider credential copied into each run comes from.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum ProviderCredential {
    /// A variable of kwoon's own environment, set under the same name inside.
    Env(String),
    /// A file, copied into the run home where the harness looks for it.
    File(PathBuf),
}

impl Settings {
    pub fn load(config_dir: &Path) -> Result<Settings, String> {
        let path = config_dir.join(FILE_NAME);
        let text = fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        let settings: Settings =
            toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        settings.validate(config_dir)?;
        Ok(settings)
    }

    fn validate(&self, config_dir: &Path) -> Result<(), String> {
        let paths = &self.paths;
        for (name, path) in [
            ("paths.state_dir", &paths.state_dir),
            ("paths.runs_dir", &paths.runs_dir),
            ("paths.bwrap", &paths.bwrap),
        ] {
            if !path.is_absolute() {
                return Err(format!("{name} must be an absolute path"));
            }
        }
        for (name, guarded) in [
            ("state", &paths.state_dir),
            ("configuration", &config_dir.to_path_buf()),
        ] {
            if paths.runs_dir.starts_with(guarded) || guarded.starts_with(&paths.runs_dir) {
                return Err(format!(
                    "paths.runs_dir and the {name} directory must not contain each other"
                ));
            }
        }
        if let Context::Test {
            fixture_repositories,
            fixture_endpoints,
        } = &self.context
            && (fixture_repositories.is_empty() || fixture_endpoints.is_empty())
        {
            return Err("a test context lists its fixture repositories and endpoints".into());
        }
        for (name, harness) in &self.harness {
            if kwoon_run::harness::Kind::from_name(name).is_none() {
                return Err(format!("unknown harness {name:?}"));
            }
            if harness.timeout_seconds == 0 {
                return Err(format!("harness.{name}.timeout_seconds must be positive"));
            }
            if !harness.executable.is_absolute()
                || harness.ro_binds.iter().any(|p| !p.is_absolute())
            {
                return Err(format!("harness.{name} paths must be absolute"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Settings, String> {
        let settings: Settings = toml::from_str(text).map_err(|e| e.to_string())?;
        settings.validate(Path::new("/cfg"))?;
        Ok(settings)
    }

    const BASE: &str = r#"
[context]
kind = "test"
fixture_repositories = ["kwoon-fixtures/sandbox"]
fixture_endpoints = ["https://api.github.com"]

[paths]
state_dir = "/state"
runs_dir = "/runs"
"#;

    #[test]
    fn reads_a_test_context() {
        let settings = parse(BASE).unwrap();
        assert_eq!(settings.context.classification(), "test");
        assert_eq!(settings.paths.bwrap, Path::new("/usr/bin/bwrap"));
    }

    #[test]
    fn refuses_unknown_fields_and_harnesses() {
        assert!(parse(&format!("{BASE}\nextra = 1\n")).is_err());
        let unknown =
            format!("{BASE}\n[harness.other]\nexecutable = \"/x\"\ntimeout_seconds = 1\n");
        assert!(parse(&unknown).unwrap_err().contains("unknown harness"));
    }

    #[test]
    fn refuses_a_runs_dir_inside_the_state_dir() {
        let text = BASE.replace("runs_dir = \"/runs\"", "runs_dir = \"/state/runs\"");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn a_test_context_needs_its_allowlist() {
        let text = BASE.replace("[\"kwoon-fixtures/sandbox\"]", "[]");
        assert!(parse(&text).is_err());
    }
}
