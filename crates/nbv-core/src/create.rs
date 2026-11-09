//! Exclusive creation of a standard, empty Python notebook.
use std::{io::Write, path::Path};

/// Creates a notebook without replacing any existing file (including symlinks).
/// Kernel selection can subsequently be changed in the notebook header.
pub fn create(path: &Path) -> std::io::Result<()> {
    if path.extension().and_then(|s| s.to_str()) != Some("ipynb") {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "new notebooks require an .ipynb extension"));
    }
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "nbformat": 4, "nbformat_minor": 5, "cells": [],
        "metadata": {
            "kernelspec": {"name": "python3", "display_name": "Python 3", "language": "python"},
            "language_info": {"name": "python"}
        }
    }))?;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitOptions, Notebook};
    #[test]
    fn creation_round_trip_and_collision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.ipynb");
        create(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let mut nb = Notebook::open(&path).unwrap();
        assert!(nb.order().is_empty());
        assert_eq!(nb.kernel_name(), Some("python3"));
        nb.commit(CommitOptions::default()).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap(),
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        );
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(create(&path).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(create(&dir.path().join("bad.json")).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn existing_symlinks_and_directories_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link.ipynb");
        std::os::unix::fs::symlink(dir.path().join("missing"), &link).unwrap();
        assert_eq!(create(&link).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert!(link.is_symlink());
        let folder = dir.path().join("folder.ipynb");
        std::fs::create_dir(&folder).unwrap();
        assert!(create(&folder).is_err());
        assert!(folder.is_dir());
    }
}
