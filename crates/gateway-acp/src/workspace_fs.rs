//! The client-side file system the gateway offers agents.
//!
//! ACP lets an agent ask its *client* to read and write files. The gateway
//! answers those requests so that agents behave the same whether they run
//! under an IDE or under the gateway — but only inside the session's declared
//! roots.
//!
//! ## Why a scope check exists at all
//!
//! The agent process could open any file the user can; the sandbox is not the
//! point. The point is that a *remote* client (a phone on another network)
//! started this session, and the gateway should not be the component that
//! turns "read my project" into "read `~/.ssh/id_ed25519`" through a relative
//! path or a symlink. Paths are therefore resolved against the roots after
//! canonicalisation, which also defeats `..` traversal and symlink escapes.

use std::path::{Component, Path, PathBuf};

/// The directories a session may touch.
#[derive(Clone, Debug)]
pub struct WorkspaceScope {
    roots: Vec<PathBuf>,
}

impl WorkspaceScope {
    /// Build a scope from a working directory plus any additional roots.
    #[must_use]
    pub fn new(cwd: &Path, additional: &[PathBuf]) -> Self {
        let mut roots = Vec::with_capacity(additional.len() + 1);
        for root in std::iter::once(cwd).chain(additional.iter().map(PathBuf::as_path)) {
            roots.push(canonical_or_self(root));
        }
        Self { roots }
    }

    /// Resolve `path` inside the scope.
    ///
    /// # Errors
    /// Returns a message suitable for an ACP error when the path is relative,
    /// escapes every root, or cannot be resolved.
    pub fn resolve(&self, path: &Path) -> Result<PathBuf, String> {
        if !path.is_absolute() {
            return Err(format!("path must be absolute: {}", path.display()));
        }
        // Existing files are canonicalised outright; for a file about to be
        // created, the deepest *existing* ancestor is canonicalised and the
        // missing tail re-appended, so a symlinked parent cannot be used to
        // escape and a not-yet-created directory does not defeat the check.
        let resolved = resolve_partially_missing(path);
        if resolved.components().any(|c| c == Component::ParentDir) {
            return Err(format!("path escapes the workspace: {}", path.display()));
        }
        if self.roots.iter().any(|root| resolved.starts_with(root)) {
            Ok(resolved)
        } else {
            Err(format!(
                "path is outside the session workspace: {}",
                path.display()
            ))
        }
    }

    /// Read a file, optionally a window of lines (1-based, as ACP defines).
    ///
    /// # Errors
    /// Returns a message when the path is out of scope or unreadable.
    pub async fn read_text(
        &self,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, String> {
        let resolved = self.resolve(path)?;
        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|error| format!("cannot read {}: {error}", resolved.display()))?;
        Ok(window(&content, line, limit))
    }

    /// Write a file, creating parent directories inside the scope.
    ///
    /// # Errors
    /// Returns a message when the path is out of scope or unwritable.
    pub async fn write_text(&self, path: &Path, content: &str) -> Result<(), String> {
        let resolved = self.resolve(path)?;
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        tokio::fs::write(&resolved, content)
            .await
            .map_err(|error| format!("cannot write {}: {error}", resolved.display()))
    }
}

fn window(content: &str, line: Option<u32>, limit: Option<u32>) -> String {
    if line.is_none() && limit.is_none() {
        return content.to_owned();
    }
    let start = line.unwrap_or(1).saturating_sub(1) as usize;
    let mut selected: Vec<&str> = content.lines().skip(start).collect();
    if let Some(limit) = limit {
        selected.truncate(limit as usize);
    }
    selected.join("\n")
}

fn canonical_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_partially_missing(path: &Path) -> PathBuf {
    let mut missing_tail = Vec::new();
    let mut current = path;
    loop {
        if let Ok(existing) = current.canonicalize() {
            let mut resolved = existing;
            for part in missing_tail.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        let (Some(parent), Some(name)) = (current.parent(), current.file_name()) else {
            // Reached the filesystem root without finding anything that
            // exists; nothing can be canonicalised, so use the path as given.
            return path.to_path_buf();
        };
        missing_tail.push(name.to_os_string());
        current = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn files_inside_the_workspace_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let scope = WorkspaceScope::new(dir.path(), &[]);
        let file = dir.path().join("nested/notes.md");

        scope
            .write_text(&file, "line1\nline2\nline3\n")
            .await
            .unwrap();
        assert_eq!(
            scope.read_text(&file, None, None).await.unwrap(),
            "line1\nline2\nline3\n"
        );
        assert_eq!(
            scope.read_text(&file, Some(2), Some(1)).await.unwrap(),
            "line2"
        );
    }

    #[tokio::test]
    async fn traversal_and_absolute_escapes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let scope = WorkspaceScope::new(dir.path(), &[]);

        let outside = dir.path().join("../escaped.txt");
        assert!(scope.write_text(&outside, "nope").await.is_err());
        assert!(
            scope
                .read_text(Path::new("/etc/passwd"), None, None)
                .await
                .is_err()
        );
        assert!(
            scope
                .read_text(Path::new("relative.txt"), None, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_symlinked_file_pointing_outside_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "ssh key").unwrap();

        let link = dir.path().join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let scope = WorkspaceScope::new(dir.path(), &[]);
        #[cfg(unix)]
        assert!(scope.read_text(&link, None, None).await.is_err());
    }

    #[tokio::test]
    async fn additional_roots_are_honoured() {
        let project = tempfile::tempdir().unwrap();
        let docs = tempfile::tempdir().unwrap();
        let note = docs.path().join("note.md");
        std::fs::write(&note, "hi").unwrap();

        let scope = WorkspaceScope::new(project.path(), &[docs.path().to_path_buf()]);
        assert_eq!(scope.read_text(&note, None, None).await.unwrap(), "hi");
    }
}
