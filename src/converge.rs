//! Observed worktree convergence.
//!
//! Resource reservations describe what an agent *intends* to touch. This module describes what a
//! checkout has *actually* changed, so the mesh can detect collisions that nobody declared. A scan
//! is pure git observation: it never mutates the checkout and never needs the agent's cooperation
//! beyond being pointed at a directory.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail, ensure};

/// Branches consulted, in order, when the caller does not name an integration base.
const DEFAULT_BASE_REFS: [&str; 2] = ["main", "master"];

/// How a path was changed inside a worktree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl ChangeKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }

    /// Classify a git status pair such as `??`, `M `, or `MD`.
    #[must_use]
    fn from_status_code(code: &str) -> Self {
        if code == "??" || code.contains('A') {
            Self::Added
        } else if code.contains('D') {
            Self::Deleted
        } else {
            Self::Modified
        }
    }

    /// Classify a `git diff --name-status` letter.
    #[must_use]
    fn from_diff_letter(letter: char) -> Self {
        match letter {
            'A' => Self::Added,
            'D' => Self::Deleted,
            _ => Self::Modified,
        }
    }
}

/// One changed path inside an agent's checkout.
#[derive(Clone, Debug)]
pub struct FootprintEntry {
    pub path: String,
    pub change_kind: ChangeKind,
    /// Blob hash of the new content, or `None` for deletions and unhashable paths.
    pub digest: Option<String>,
}

/// A complete observation of one checkout at a point in time.
#[derive(Clone, Debug)]
pub struct WorktreeScan {
    pub base_ref: String,
    pub base_commit: String,
    pub branch: Option<String>,
    pub entries: Vec<FootprintEntry>,
}

/// One agent's stake in a contested path.
#[derive(Clone, Debug)]
pub struct Participant {
    pub agent_id: String,
    pub agent_name: String,
    pub status: String,
    pub change_kind: String,
    pub digest: Option<String>,
    pub base_commit: Option<String>,
    pub branch: Option<String>,
}

/// Collision severity for a single contested path.
///
/// `identical` means several agents produced byte-identical content: duplicated effort, safe to
/// merge. `divergent` means the same path holds different content in different checkouts, which is
/// the case that silently overwrites work. `delete_edit` means one agent removed a path another is
/// still editing, which git will merge without complaint in several common orderings.
#[must_use]
pub fn classify(participants: &[Participant]) -> &'static str {
    let deleting = participants
        .iter()
        .any(|participant| participant.change_kind == ChangeKind::Deleted.as_str());
    let editing = participants
        .iter()
        .any(|participant| participant.change_kind != ChangeKind::Deleted.as_str());
    if deleting && editing {
        return "delete_edit";
    }
    let first = participants
        .first()
        .and_then(|participant| participant.digest.as_deref());
    if first.is_some()
        && participants
            .iter()
            .all(|participant| participant.digest.as_deref() == first)
    {
        return "identical";
    }
    "divergent"
}

/// Whether the contesting agents cut their worktrees from different base commits.
///
/// A divergent base is how a clean merge still breaks behaviour: the later diff was written against
/// code that no longer exists on the integration branch.
#[must_use]
pub fn base_divergent(participants: &[Participant]) -> bool {
    let mut bases = participants
        .iter()
        .filter_map(|participant| participant.base_commit.as_deref());
    let Some(first) = bases.next() else {
        return false;
    };
    bases.any(|base| base != first)
}

/// Observe a checkout and produce its footprint relative to an integration base.
pub fn scan_worktree(checkout: &Path, base_ref: Option<&str>) -> Result<WorktreeScan> {
    ensure!(
        checkout.is_dir(),
        "checkout is not a directory: {}",
        checkout.display()
    );
    let branch = git_text(checkout, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .filter(|name| name != "HEAD" && !name.is_empty());
    let base_ref = resolve_base_ref(checkout, base_ref)?;
    let base_commit =
        git_text(checkout, &["merge-base", &base_ref, "HEAD"]).with_context(|| {
            format!("no merge base between HEAD and {base_ref}; pass an explicit --base")
        })?;

    // Later sources overwrite earlier ones, so uncommitted state wins over committed state.
    let mut kinds: BTreeMap<String, ChangeKind> = BTreeMap::new();
    for (path, kind) in committed_changes(checkout, &base_commit)? {
        kinds.insert(path, kind);
    }
    for (path, kind) in uncommitted_changes(checkout)? {
        kinds.insert(path, kind);
    }

    let hashable: Vec<String> = kinds
        .iter()
        .filter(|(path, kind)| **kind != ChangeKind::Deleted && !path.contains('\n'))
        .map(|(path, _)| path.clone())
        .collect();
    let mut digests = hash_objects(checkout, &hashable)?;

    let entries = kinds
        .into_iter()
        .map(|(path, change_kind)| FootprintEntry {
            digest: digests.remove(&path),
            path,
            change_kind,
        })
        .collect();
    Ok(WorktreeScan {
        base_ref,
        base_commit,
        branch,
        entries,
    })
}

/// Pick the integration branch to measure against.
fn resolve_base_ref(checkout: &Path, requested: Option<&str>) -> Result<String> {
    if let Some(requested) = requested {
        ensure!(
            git_text(checkout, &["rev-parse", "--verify", "--quiet", requested]).is_ok(),
            "base ref not found in checkout: {requested}"
        );
        return Ok(requested.to_owned());
    }
    for candidate in DEFAULT_BASE_REFS {
        if git_text(checkout, &["rev-parse", "--verify", "--quiet", candidate]).is_ok() {
            return Ok(candidate.to_owned());
        }
    }
    bail!("no default integration branch found; pass an explicit --base")
}

/// Changes committed on this branch since the base commit.
fn committed_changes(checkout: &Path, base_commit: &str) -> Result<Vec<(String, ChangeKind)>> {
    let raw = git_bytes(
        checkout,
        &[
            "diff",
            "--name-status",
            "--no-renames",
            "-z",
            base_commit,
            "HEAD",
        ],
    )?;
    // `-z` emits alternating NUL-terminated status and path fields.
    let mut fields = split_nul(&raw).into_iter();
    let mut changes = Vec::new();
    while let Some(status) = fields.next() {
        let Some(path) = fields.next() else {
            break;
        };
        let Some(letter) = status.chars().next() else {
            continue;
        };
        changes.push((path, ChangeKind::from_diff_letter(letter)));
    }
    Ok(changes)
}

/// Uncommitted changes, including untracked files.
fn uncommitted_changes(checkout: &Path) -> Result<Vec<(String, ChangeKind)>> {
    let raw = git_bytes(
        checkout,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    )?;
    // `-z` emits one NUL-terminated `XY <path>` record per change.
    Ok(split_nul(&raw)
        .into_iter()
        .filter_map(|record| {
            let code = record.get(..2)?;
            let path = record.get(3..)?;
            if path.is_empty() {
                return None;
            }
            Some((path.to_owned(), ChangeKind::from_status_code(code)))
        })
        .collect())
}

/// Hash the working-tree content of each path in one git process.
fn hash_objects(checkout: &Path, paths: &[String]) -> Result<BTreeMap<String, String>> {
    if paths.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["hash-object", "--stdin-paths"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("git hash-object stdin unavailable"))?;
        for path in paths {
            writeln!(stdin, "{path}")?;
        }
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "git hash-object failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let digests: Vec<String> = String::from_utf8(output.stdout)?
        .lines()
        .map(str::to_owned)
        .collect();
    ensure!(
        digests.len() == paths.len(),
        "git hash-object returned {} digests for {} paths",
        digests.len(),
        paths.len()
    );
    Ok(paths.iter().cloned().zip(digests).collect())
}

/// Split NUL-terminated git output, dropping the trailing empty field.
fn split_nul(raw: &[u8]) -> Vec<String> {
    raw.split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect()
}

fn git_bytes(directory: &Path, arguments: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()?;
    ensure!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn git_text(directory: &Path, arguments: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git_bytes(directory, arguments)?)?
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(change_kind: &str, digest: Option<&str>, base: Option<&str>) -> Participant {
        Participant {
            agent_id: "agent".to_owned(),
            agent_name: "agent".to_owned(),
            status: "running".to_owned(),
            change_kind: change_kind.to_owned(),
            digest: digest.map(ToOwned::to_owned),
            base_commit: base.map(ToOwned::to_owned),
            branch: None,
        }
    }

    #[test]
    fn matching_content_is_duplicated_effort_not_a_conflict() {
        let participants = vec![
            participant("modified", Some("aaa"), None),
            participant("modified", Some("aaa"), None),
        ];
        assert_eq!(classify(&participants), "identical");
    }

    #[test]
    fn differing_content_on_one_path_is_divergent() {
        let participants = vec![
            participant("modified", Some("aaa"), None),
            participant("modified", Some("bbb"), None),
        ];
        assert_eq!(classify(&participants), "divergent");
    }

    #[test]
    fn a_deletion_against_an_edit_outranks_content_comparison() {
        let participants = vec![
            participant("deleted", None, None),
            participant("modified", Some("bbb"), None),
        ];
        assert_eq!(classify(&participants), "delete_edit");
    }

    #[test]
    fn an_unhashable_path_is_treated_as_divergent() {
        let participants = vec![
            participant("modified", None, None),
            participant("modified", None, None),
        ];
        assert_eq!(classify(&participants), "divergent");
    }

    #[test]
    fn differing_base_commits_are_reported_separately_from_severity() {
        let same = vec![
            participant("modified", Some("aaa"), Some("c1")),
            participant("modified", Some("bbb"), Some("c1")),
        ];
        let drifted = vec![
            participant("modified", Some("aaa"), Some("c1")),
            participant("modified", Some("bbb"), Some("c2")),
        ];
        assert!(!base_divergent(&same));
        assert!(base_divergent(&drifted));
    }

    #[test]
    fn status_codes_map_to_change_kinds() {
        assert_eq!(ChangeKind::from_status_code("??"), ChangeKind::Added);
        assert_eq!(ChangeKind::from_status_code("A "), ChangeKind::Added);
        assert_eq!(ChangeKind::from_status_code(" D"), ChangeKind::Deleted);
        assert_eq!(ChangeKind::from_status_code("MD"), ChangeKind::Deleted);
        assert_eq!(ChangeKind::from_status_code(" M"), ChangeKind::Modified);
    }
}
