//! What nbv remembers between sessions: the kernel last picked for each notebook (§13).
//! One JSON file, `$XDG_STATE_HOME/nbv/kernels.json` (by default `~/.local/state/nbv/`),
//! mapping a notebook's absolute path to the kernel command picked for it.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::kernel::KernelCommand;

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("neither XDG_STATE_HOME nor HOME is set, so nbv has nowhere to remember kernels")]
    NoStateDir,
    #[error("cannot read {0}: {1}")]
    Read(PathBuf, std::io::Error),
    #[error("{0} is not valid (delete it to forget every kernel choice): {1}")]
    Invalid(PathBuf, serde_json::Error),
    #[error("cannot write {0}: {1}")]
    Write(PathBuf, std::io::Error),
}

pub struct KernelMemory {
    file: PathBuf,
    kernels: BTreeMap<PathBuf, KernelCommand>,
}

impl KernelMemory {
    /// The memory in the user's state directory.
    pub fn open() -> Result<KernelMemory, MemoryError> {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let state = var("XDG_STATE_HOME")
            .or_else(|| var("HOME").map(|h| h.join(".local/state")))
            .ok_or(MemoryError::NoStateDir)?;
        KernelMemory::at(state.join("nbv/kernels.json"))
    }

    /// The memory kept in `file`, empty if the file does not exist yet.
    pub fn at(file: PathBuf) -> Result<KernelMemory, MemoryError> {
        let kernels = match fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| MemoryError::Invalid(file.clone(), e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(MemoryError::Read(file, e)),
        };
        Ok(KernelMemory { file, kernels })
    }

    /// The kernel last picked for `notebook`.
    pub fn get(&self, notebook: &Path) -> Option<&KernelCommand> {
        self.kernels.get(notebook)
    }

    /// Remembers `cmd` as the kernel for `notebook`, on disk at once.
    pub fn remember(&mut self, notebook: &Path, cmd: &KernelCommand) -> Result<(), MemoryError> {
        self.kernels.insert(notebook.into(), cmd.clone());
        self.save()
    }

    /// Follows a notebook to its new path.
    pub fn renamed(&mut self, from: &Path, to: &Path) -> Result<(), MemoryError> {
        match self.kernels.remove(from) {
            Some(cmd) => {
                self.kernels.insert(to.into(), cmd);
                self.save()
            }
            None => Ok(()),
        }
    }

    fn save(&self) -> Result<(), MemoryError> {
        let err = |e| MemoryError::Write(self.file.clone(), e);
        let dir = self.file.parent().expect("the memory file is in a directory");
        fs::create_dir_all(dir).map_err(err)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(err)?;
        let bytes = serde_json::to_vec_pretty(&self.kernels).expect("kernel commands serialise");
        tmp.write_all(&bytes).map_err(err)?;
        tmp.persist(&self.file).map_err(|e| err(e.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str) -> KernelCommand {
        KernelCommand {
            argv: vec!["/venv/bin/python".into(), "-m".into(), "ipykernel_launcher".into()],
            env: Default::default(),
            interrupt_via_message: false,
            display_name: name.into(),
            language: "python".into(),
            spec: None,
        }
    }

    #[test]
    fn remembers_across_sessions_and_follows_renames() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nbv/kernels.json");
        let (a, b) = (Path::new("/n/a.ipynb"), Path::new("/n/b.ipynb"));
        let mut m = KernelMemory::at(file.clone()).unwrap();
        assert!(m.get(a).is_none());
        m.remember(a, &cmd("first")).unwrap();
        m.remember(a, &cmd("second")).unwrap();
        assert_eq!(KernelMemory::at(file.clone()).unwrap().get(a), Some(&cmd("second")));

        m.renamed(a, b).unwrap();
        let m = KernelMemory::at(file.clone()).unwrap();
        assert!(m.get(a).is_none());
        assert_eq!(m.get(b), Some(&cmd("second")));
    }

    #[test]
    fn a_damaged_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("kernels.json");
        std::fs::write(&file, "not json").unwrap();
        assert!(matches!(KernelMemory::at(file), Err(MemoryError::Invalid(..))));
    }
}
