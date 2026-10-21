//! The run directory and its private run home (plan D5). The home is a fresh directory
//! under the run's own directory, never the operator's home, and everything in it is a
//! copy that kwoon wrote, so nothing a run changes there reaches the host.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The name of the private run home inside a run directory.
pub const HOME_NAME: &str = "home";
pub const CLONE_NAME: &str = "clone";

pub struct RunDir {
    pub root: PathBuf,
    pub clone: PathBuf,
    pub home: PathBuf,
}

impl RunDir {
    /// Creates a new, empty run directory under `runs_dir`. The name is unique, and the
    /// directory is created exclusively, so nothing that existed before is reused.
    pub fn create(runs_dir: &Path, label: &str) -> io::Result<RunDir> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(runs_dir)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = runs_dir.join(format!("{label}-{}-{nanos}", std::process::id()));
        let mut dir = DirBuilder::new();
        dir.mode(0o700);
        dir.create(&root)?;
        let run = RunDir {
            clone: root.join(CLONE_NAME),
            home: root.join(HOME_NAME),
            root,
        };
        dir.create(&run.clone)?;
        dir.create(&run.home)?;
        Ok(run)
    }

    /// Removes the run directory. `fs::remove_dir_all` removes symbolic links without
    /// following them and opens each directory relative to its parent, so a run cannot
    /// redirect the removal outside its own directory.
    pub fn remove(self) -> io::Result<()> {
        fs::remove_dir_all(&self.root)
    }
}

/// A file kwoon writes into the private run home.
#[derive(Debug, Clone)]
pub struct SeedFile {
    /// Relative to the home.
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

/// Writes `files` into the fresh `home`, each as a new file of mode 0600.
pub fn seed(home: &Path, files: &[SeedFile]) -> io::Result<()> {
    for file in files {
        if file.path.is_absolute()
            || file
                .path
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "seed path {} is not a plain relative path",
                    file.path.display()
                ),
            ));
        }
        let target = home.join(&file.path);
        if let Some(parent) = target.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)?;
        out.write_all(&file.bytes)?;
    }
    Ok(())
}

/// The token prefixes GitHub uses for its credentials.
pub const GITHUB_TOKEN_PREFIXES: [&str; 6] =
    ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"];

/// What must never be seeded into a run: GitHub token prefixes, and the exact bytes of
/// each of kwoon's forge credentials. Returns one problem per match, naming the seeded
/// item and what it matched, never the bytes.
pub fn scan_seed(items: &[(String, &[u8])], forge_secrets: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, bytes) in items {
        for prefix in GITHUB_TOKEN_PREFIXES {
            if contains(bytes, prefix.as_bytes()) {
                problems.push(format!("{name} contains the GitHub token prefix {prefix}"));
            }
        }
        for (id, secret) in forge_secrets {
            if !secret.is_empty() && contains(bytes, secret) {
                problems.push(format!("{name} contains forge credential {id}"));
            }
        }
    }
    problems
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_copies_that_the_run_can_change_without_touching_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let run = RunDir::create(dir.path(), "t").unwrap();
        seed(
            &run.home,
            &[SeedFile {
                path: ".claude/settings.json".into(),
                bytes: b"{}".to_vec(),
            }],
        )
        .unwrap();
        let seeded = run.home.join(".claude/settings.json");
        assert_eq!(fs::read(&seeded).unwrap(), b"{}");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&seeded).unwrap().permissions().mode() & 0o777,
            0o600
        );
        run.remove().unwrap();
    }

    #[test]
    fn refuses_seed_paths_that_leave_the_home() {
        let dir = tempfile::tempdir().unwrap();
        for path in ["../x", "/etc/x", "a/../../x"] {
            let file = SeedFile {
                path: path.into(),
                bytes: Vec::new(),
            };
            assert!(seed(dir.path(), &[file]).is_err(), "{path}");
        }
    }

    #[test]
    fn scan_names_the_match_without_its_bytes() {
        let secret = b"s3cret-value".to_vec();
        let problems = scan_seed(
            &[
                ("a".into(), b"token ghp_abc".as_slice()),
                ("b".into(), b"has s3cret-value inside".as_slice()),
                ("c".into(), b"clean".as_slice()),
            ],
            &[("fixture".into(), secret)],
        );
        assert_eq!(
            problems,
            [
                "a contains the GitHub token prefix ghp_",
                "b contains forge credential fixture"
            ]
        );
    }
}
