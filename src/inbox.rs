use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Directory,
    Archive,
}

impl CandidateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::Archive => "archive",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportCandidate {
    pub path: PathBuf,
    pub kind: CandidateKind,
}

pub fn default_directory() -> Result<PathBuf> {
    Ok(std::env::current_dir()
        .context("determine current working directory")?
        .join("mods"))
}

pub fn scan(directory: &Path) -> Result<Vec<ImportCandidate>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut candidates = fs::read_dir(directory)
        .with_context(|| format!("scan mod inbox {}", directory.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                Some(ImportCandidate {
                    path,
                    kind: CandidateKind::Directory,
                })
            } else if path.is_file() && is_supported_archive(&path) {
                Some(ImportCandidate {
                    path,
                    kind: CandidateKind::Archive,
                })
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| {
        candidate
            .path
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    });
    Ok(candidates)
}

pub fn is_supported_archive(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "zip" | "7z" | "rar" | "tar" | "gz" | "bz2" | "xz" | "zst"
            )
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_directories_and_supported_archives_only() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("Extracted Mod")).unwrap();
        fs::write(temp.path().join("packed.zip"), []).unwrap();
        fs::write(temp.path().join("notes.txt"), []).unwrap();

        let candidates = scan(temp.path()).unwrap();

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].kind, CandidateKind::Directory);
        assert_eq!(candidates[1].kind, CandidateKind::Archive);
    }
}
