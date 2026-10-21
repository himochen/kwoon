//! The kwoon command.

mod settings;
mod state;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use kwoon_run::doctor::{self, Credential, Harness, Host, Status};
use kwoon_run::harness::Kind;

use settings::{ProviderCredential, Settings};

const USAGE: &str = "usage: kwoon [--config-dir <dir>] doctor [--harness <name>]";

fn main() -> ExitCode {
    // Before anything else, so no inherited descriptor can reach a child.
    if let Err(err) = kwoon_run::spawn::mark_descriptors_cloexec() {
        eprintln!("kwoon: cannot mark inherited descriptors close-on-exec: {err}");
        return ExitCode::FAILURE;
    }
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("__probe") {
        return probe(&args[1..]);
    }
    let mut config_dir = None;
    let mut rest = args.as_slice();
    while let [flag, value, tail @ ..] = rest {
        if flag != "--config-dir" {
            break;
        }
        config_dir = Some(PathBuf::from(value));
        rest = tail;
    }
    let result = match rest {
        [command, options @ ..] if command == "doctor" => match options {
            [] => doctor(config_dir, None),
            [flag, name] if flag == "--harness" => doctor(config_dir, Some(name)),
            _ => return usage(),
        },
        _ => return usage(),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("kwoon: {err}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

/// `kwoon __probe <request>`: runs inside the wrapper only.
fn probe(args: &[String]) -> ExitCode {
    let [request] = args else {
        return usage();
    };
    match doctor::probe_main(request) {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("kwoon: {err}");
            ExitCode::FAILURE
        }
    }
}

fn default_config_dir() -> Result<PathBuf, String> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("kwoon"));
    }
    let home = env::var_os("HOME").ok_or("neither --config-dir nor HOME is set")?;
    Ok(PathBuf::from(home).join(".config/kwoon"))
}

/// Returns whether every item is verified.
fn doctor(config_dir: Option<PathBuf>, only: Option<&String>) -> Result<bool, String> {
    let config_dir = match config_dir {
        Some(dir) => dir,
        None => default_config_dir()?,
    };
    let config_dir =
        fs::canonicalize(&config_dir).map_err(|err| format!("{}: {err}", config_dir.display()))?;
    let settings = Settings::load(&config_dir)?;
    state::open(&settings.paths.state_dir, settings.context.classification())?;

    let mut protected = vec![settings.paths.state_dir.clone(), config_dir.clone()];
    let mut forge_secrets = Vec::new();
    for (id, credential) in &settings.credentials {
        let bytes = fs::read(&credential.file)
            .map_err(|err| format!("cannot read forge credential {id}: {err}"))?;
        forge_secrets.push((id.clone(), bytes.trim_ascii().to_vec()));
        protected.push(credential.file.clone());
    }
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        protected.push(home.join(".config/gh"));
        protected.push(home.join(".git-credentials"));
    }
    let host = Host {
        bwrap: settings.paths.bwrap.clone(),
        probe: env::current_exe()
            .map_err(|err| format!("cannot find the kwoon executable: {err}"))?,
        runs_dir: settings.paths.runs_dir.clone(),
        state_dir: settings.paths.state_dir.clone(),
        protected: doctor::protected_paths(&protected),
        forge_secrets,
        ssh_auth_sock: env::var("SSH_AUTH_SOCK").ok(),
        dbus_address: env::var("DBUS_SESSION_BUS_ADDRESS").ok(),
        lang: env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into()),
    };

    let names: Vec<&String> = match only {
        Some(name) if settings.harness.contains_key(name) => vec![name],
        Some(name) => {
            return Err(format!(
                "harness {name:?} is not configured in host settings"
            ));
        }
        None => settings.harness.keys().collect(),
    };
    if names.is_empty() {
        return Err("no harness is configured in host settings".into());
    }

    println!(
        "kwoon doctor ({} context)",
        settings.context.classification()
    );
    let mut all_verified = true;
    for name in names {
        let configured = &settings.harness[name];
        let harness = Harness {
            kind: Kind::from_name(name).expect("validated in settings"),
            executable: configured.executable.clone(),
            ro_binds: configured.ro_binds.clone(),
            env: configured
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            credential: provider_credential(name, configured.provider_credential.as_ref())?,
            timeout: Duration::from_secs(configured.timeout_seconds),
        };
        println!("harness {name}");
        for item in doctor::check(&host, &harness) {
            all_verified &= item.status == Status::Verified;
            if item.detail.is_empty() {
                println!("  {}: {}", item.name, item.status);
            } else {
                println!("  {}: {}: {}", item.name, item.status, item.detail);
            }
        }
    }
    Ok(all_verified)
}

fn provider_credential(
    harness: &str,
    source: Option<&ProviderCredential>,
) -> Result<Option<Credential>, String> {
    Ok(match source {
        None => None,
        Some(ProviderCredential::Env(name)) => {
            let value = env::var(name).map_err(|_| {
                format!("harness {harness}: provider credential variable {name} is not set")
            })?;
            Some(Credential::Env {
                name: name.clone(),
                value,
            })
        }
        Some(ProviderCredential::File(path)) => {
            let bytes = fs::read(path).map_err(|err| {
                format!(
                    "harness {harness}: cannot read provider credential {}: {err}",
                    path.display()
                )
            })?;
            Some(Credential::File { bytes })
        }
    })
}
