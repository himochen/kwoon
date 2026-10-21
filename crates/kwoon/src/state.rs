//! The state directory and its classification. The classification is written once,
//! when kwoon creates the directory, and never rewritten: a test state directory cannot
//! be opened by a normal context, nor a normal one by a test context.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

pub const CLASSIFICATION_FILE: &str = "classification";

/// Opens the state directory for a context of `classification`, creating and
/// classifying it if it does not exist yet.
pub fn open(state_dir: &Path, classification: &str) -> Result<(), String> {
    let file = state_dir.join(CLASSIFICATION_FILE);
    match DirBuilder::new().mode(0o700).create(state_dir) {
        Ok(()) => {
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o400)
                .open(&file)
                .map_err(|err| format!("cannot classify {}: {err}", state_dir.display()))?;
            out.write_all(format!("{classification}\n").as_bytes())
                .and_then(|()| out.sync_all())
                .map_err(|err| format!("cannot classify {}: {err}", state_dir.display()))?;
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(format!("cannot create {}: {err}", state_dir.display())),
    }
    let found = fs::read_to_string(&file).map_err(|err| {
        format!(
            "{} has no readable classification: {err}",
            state_dir.display()
        )
    })?;
    if found.trim_end_matches('\n') != classification {
        return Err(format!(
            "{} is classified {:?}; this context is {classification:?}",
            state_dir.display(),
            found.trim_end()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_once_and_refuses_the_other_context() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        open(&state, "test").unwrap();
        open(&state, "test").unwrap();
        assert!(
            open(&state, "normal")
                .unwrap_err()
                .contains("classified \"test\"")
        );
    }

    #[test]
    fn refuses_an_unclassified_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            open(dir.path(), "normal")
                .unwrap_err()
                .contains("no readable classification")
        );
    }
}
