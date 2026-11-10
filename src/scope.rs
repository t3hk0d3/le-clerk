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
    /// The directory is inside a git checkout, but `git` could not be run
    /// or failed there, so its project is unknown.
    #[error(
        "cannot tell the project of {}: it is inside a git checkout but git failed ({reason}); \
         make git work there (it must be on PATH and trust the repository)",
        .dir.display()
    )]
    Git {
        /// The directory being resolved.
        dir: PathBuf,
        /// What git said, or why it could not run.
        reason: String,
    },
}

impl ErrorCode for ScopeError {
    fn code(&self) -> &'static str {
        match self {
            ScopeError::NoSuchDirectory(_) => "no_such_directory",
            ScopeError::Git { .. } => "git_failed",
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
    /// "Not in a git repository" (git fails and no `.git` exists in `cwd`
    /// or above it) gives `cwd`; git failing (or missing) inside a checkout
    /// is an error, so a repository's work is never filed under one of its
    /// subdirectories.
    ///
    /// # Errors
    /// [`ScopeError::NoSuchDirectory`] when `cwd` cannot be canonicalised;
    /// [`ScopeError::Git`] when git fails inside a git checkout.
    pub fn resolve(cwd: &Path) -> Result<ProjectKey, ScopeError> {
        Self::resolve_with_git(cwd, Path::new("git"))
    }

    /// [`ProjectResolver::resolve`], running `git` as the git program.
    ///
    /// # Errors
    /// As [`ProjectResolver::resolve`].
    pub fn resolve_with_git(cwd: &Path, git: &Path) -> Result<ProjectKey, ScopeError> {
        let canonical_cwd = cwd
            .canonicalize()
            .map_err(|_| ScopeError::NoSuchDirectory(cwd.to_owned()))?;
        Ok(ProjectKey(
            git_common_dir(&canonical_cwd, git)?.unwrap_or(canonical_cwd),
        ))
    }
}

/// The canonical common git dir of `cwd`; `None` when `cwd` is not in a
/// git repository.
fn git_common_dir(cwd: &Path, git: &Path) -> Result<Option<PathBuf>, ScopeError> {
    let failed = |reason: String| {
        if in_git_checkout(cwd) {
            Err(ScopeError::Git {
                dir: cwd.to_owned(),
                reason,
            })
        } else {
            Ok(None)
        }
    };
    let out = match Command::new(git)
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
    {
        Ok(out) => out,
        Err(e) => return failed(format!("{} could not be run: {e}", git.display())),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return failed(format!("{}: {}", out.status, stderr.trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.trim_end_matches(['\n', '\r']);
    if line.is_empty() {
        return failed("git named no git dir".to_owned());
    }
    // The output may be relative to `cwd`; joining an absolute path replaces it.
    match cwd.join(line).canonicalize() {
        Ok(dir) => Ok(Some(dir)),
        Err(e) => failed(format!("its git dir {line} cannot be resolved: {e}")),
    }
}

/// Whether `dir` or a directory above it holds a `.git` entry (a checkout's
/// git dir, or a linked worktree's `.git` file).
fn in_git_checkout(dir: &Path) -> bool {
    dir.ancestors()
        .any(|d| d.join(".git").symlink_metadata().is_ok())
}
