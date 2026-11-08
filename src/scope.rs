//! Resolving a working directory to a project.

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::error::ErrorCode;
use crate::model::ProjectKey;

/// Why a directory could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScopeError {
    /// The directory does not exist or cannot be canonicalised.
    #[error("no such directory: {0}")]
    NoSuchDirectory(PathBuf),
}

impl ErrorCode for ScopeError {
    fn code(&self) -> &'static str {
        match self {
            ScopeError::NoSuchDirectory(_) => "no_such_directory",
        }
    }
}

/// Maps a working directory to the project it belongs to.
#[derive(Debug, Clone, Copy)]
pub struct ProjectResolver;

impl ProjectResolver {
    /// The project of `cwd`: the canonical common git dir when inside a git
    /// repository (linked worktrees share it), else the canonical `cwd`.
    ///
    /// # Errors
    /// [`ScopeError::NoSuchDirectory`] when `cwd` cannot be canonicalised.
    pub fn resolve(cwd: &Path) -> Result<ProjectKey, ScopeError> {
        let canonical_cwd = cwd
            .canonicalize()
            .map_err(|_| ScopeError::NoSuchDirectory(cwd.to_owned()))?;
        Ok(ProjectKey(
            git_common_dir(&canonical_cwd).unwrap_or(canonical_cwd),
        ))
    }
}

fn git_common_dir(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let line = text.trim_end_matches(['\n', '\r']);
    if line.is_empty() {
        return None;
    }
    // The output may be relative to `cwd`; joining an absolute path replaces it.
    cwd.join(line).canonicalize().ok()
}
