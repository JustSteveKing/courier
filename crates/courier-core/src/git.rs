//! What git knows about a collection: which requests are new, changed or gone, what branch
//! you're on, and what a change looks like.
//!
//! This asks the `git` command rather than reading the repository itself, so it sees exactly
//! what you would see in a terminal — your config, your hooks, your ignore rules. It only
//! ever reads: committing stays in your own tools.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How a file differs from the last commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Not in the last commit: added, or not tracked at all.
    New,
    Modified,
    /// In the last commit, gone from the working tree.
    Deleted,
}

impl Change {
    /// A single character for the sidebar.
    pub fn mark(self) -> &'static str {
        match self {
            Self::New => "+",
            Self::Modified => "●",
            Self::Deleted => "−",
        }
    }
}

/// A repository and what it makes of a collection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// The branch, or the short commit when the head is detached.
    pub branch: Option<String>,
    /// Paths that differ from the last commit, absolute.
    pub changes: HashMap<PathBuf, Change>,
}

impl Status {
    pub fn change_for(&self, path: &Path) -> Option<Change> {
        self.changes.get(path).copied()
    }

    /// Whether anything at or below `path` changed, for folder and collection rows.
    pub fn changed_within(&self, path: &Path) -> bool {
        self.changes.keys().any(|changed| changed.starts_with(path))
    }
}

/// The repository `dir` is in, if any. Returns the working tree's root.
pub fn repository_of(dir: &Path) -> Option<PathBuf> {
    let output = run(dir, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(output.trim());
    root.is_dir().then_some(root)
}

/// What git makes of `dir` right now. Anything unreadable comes back as no changes rather
/// than an error: git awareness is a convenience, never in the way.
pub fn status_of(dir: &Path) -> Status {
    let mut status = Status {
        branch: branch_of(dir),
        changes: HashMap::new(),
    };
    // -z keeps paths exactly as they are, even with odd characters in them.
    let Some(output) = run(
        dir,
        &["status", "--porcelain", "-z", "--untracked-files=all", "--", "."],
    ) else {
        return status;
    };
    let root = repository_of(dir).unwrap_or_else(|| dir.to_path_buf());
    let mut fields = output.split('\0').filter(|field| !field.is_empty());
    while let Some(entry) = fields.next() {
        // "XY path", and for a rename the next field is where it came from.
        let (codes, path) = entry.split_at(entry.len().min(2));
        let path = path.trim_start();
        if path.is_empty() {
            continue;
        }
        let renamed = codes.starts_with('R') || codes.starts_with('C');
        if renamed && let Some(from) = fields.next() {
            status.changes.insert(root.join(from), Change::Deleted);
        }
        let change = if codes.contains('D') {
            Change::Deleted
        } else if codes.contains('A') || codes.contains('?') || renamed {
            Change::New
        } else {
            Change::Modified
        };
        status.changes.insert(root.join(path), change);
    }
    status
}

fn branch_of(dir: &Path) -> Option<String> {
    let branch = run(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim().to_string();
    if branch == "HEAD" {
        // A detached head has no branch name; the short commit is what a terminal shows.
        return run(dir, &["rev-parse", "--short", "HEAD"]).map(|commit| commit.trim().to_string());
    }
    (!branch.is_empty()).then_some(branch)
}

/// The diff of one file against the last commit, as `git diff` would print it. A file that
/// isn't tracked yet has no diff, so its contents are shown as added lines instead.
pub fn diff(path: &Path) -> Option<String> {
    let dir = path.parent()?;
    let diff = run(dir, &["diff", "HEAD", "--", &path.to_string_lossy()])?;
    if !diff.trim().is_empty() {
        return Some(diff);
    }
    let untracked = run(dir, &["ls-files", "--others", "--", &path.to_string_lossy()])?;
    if untracked.trim().is_empty() {
        return None;
    }
    let contents = std::fs::read_to_string(path).ok()?;
    let added: String = contents.lines().map(|line| format!("+{line}\n")).collect();
    Some(format!("--- /dev/null\n+++ {}\n{added}", path.display()))
}

/// Runs a git command in `dir`, or nothing if git isn't there or the command failed.
fn run(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        // Keep git from asking for anything: this runs behind the interface.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository with one commit, or `None` when git isn't installed.
    fn repository() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .ok()
                .filter(|output| output.status.success())
        };
        git(&["init", "--initial-branch=main"])?;
        std::fs::write(dir.path().join("kept.yaml"), "name: Kept\n").unwrap();
        std::fs::write(dir.path().join("edited.yaml"), "name: Edited\n").unwrap();
        std::fs::write(dir.path().join("gone.yaml"), "name: Gone\n").unwrap();
        git(&["add", "."])?;
        git(&["commit", "-m", "first"])?;
        Some(dir)
    }

    #[test]
    fn sees_what_changed_since_the_last_commit() {
        let Some(dir) = repository() else {
            eprintln!("skipping: git isn't installed");
            return;
        };
        let path = dir.path();
        std::fs::write(path.join("edited.yaml"), "name: Edited twice\n").unwrap();
        std::fs::remove_file(path.join("gone.yaml")).unwrap();
        std::fs::write(path.join("added.yaml"), "name: Added\n").unwrap();

        let status = status_of(path);
        assert_eq!(status.branch.as_deref(), Some("main"));
        let root = repository_of(path).unwrap();
        assert_eq!(status.change_for(&root.join("edited.yaml")), Some(Change::Modified));
        assert_eq!(status.change_for(&root.join("gone.yaml")), Some(Change::Deleted));
        assert_eq!(
            status.change_for(&root.join("added.yaml")),
            Some(Change::New),
            "a file that was never committed counts as new"
        );
        assert_eq!(
            status.change_for(&root.join("kept.yaml")),
            None,
            "an untouched file is quiet"
        );
        assert!(status.changed_within(&root), "the folder above knows something changed");
        assert!(!status.changed_within(&root.join("elsewhere")));

        let edited = diff(&root.join("edited.yaml")).expect("an edit has a diff");
        assert!(edited.contains("-name: Edited"), "{edited}");
        assert!(edited.contains("+name: Edited twice"), "{edited}");

        let added = diff(&root.join("added.yaml")).expect("a new file shows as added lines");
        assert!(added.contains("+name: Added"), "{added}");
        assert_eq!(
            diff(&root.join("kept.yaml")),
            None,
            "nothing to show for an untouched file"
        );
    }

    #[test]
    fn says_nothing_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        // A temp dir could sit inside someone's repository; only assert when it doesn't.
        if repository_of(dir.path()).is_none() {
            let status = status_of(dir.path());
            assert_eq!(status, Status::default());
            assert!(diff(&dir.path().join("nothing.yaml")).is_none());
        }
    }
}
