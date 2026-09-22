//! Observed worktree convergence.
//!
//! Resource reservations describe what an agent *intends* to touch. This module describes what a
//! checkout has *actually* changed, so the mesh can detect collisions that nobody declared. A scan
//! is pure git observation: it never mutates the checkout and never needs the agent's cooperation
//! beyond being pointed at a directory.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Result, anyhow, bail, ensure};
use uuid::Uuid;

/// Branches consulted, in order, when the caller does not name an integration base.
const DEFAULT_BASE_REFS: [&str; 2] = ["main", "master"];

/// Files above this size are not fingerprinted; they are reported as divergent instead, which is
/// the conservative direction.
const MAX_HASHED_BYTES: u64 = 64 * 1024 * 1024;

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
    /// Regions of the *base* file this checkout rewrote, as `start-end` pairs joined by commas.
    ///
    /// Base coordinates, not working-tree coordinates, because a three-way merge conflicts when
    /// two sides rewrote overlapping regions of the common ancestor. `None` means the regions
    /// are unknown and the paths must be assumed to overlap.
    pub ranges: Option<String>,
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
    pub ranges: Option<String>,
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
    if deleting {
        // Every checkout removed the path. Deletions carry no digest, so without this the
        // content comparison below would fall through and call a clean merge divergent.
        return "identical";
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
    // Different content in the same file is not automatically a conflict. A shared router or
    // module index is edited by everyone, and git merges those edits cleanly as long as they
    // rewrote different regions of the common ancestor.
    if disjoint_regions(participants) {
        return "adjacent";
    }
    "divergent"
}

/// Whether every pair of participants rewrote non-overlapping regions of the base file.
///
/// Unknown regions are treated as overlapping, so this only ever downgrades a collision when it
/// can prove the edits are separable.
#[must_use]
fn disjoint_regions(participants: &[Participant]) -> bool {
    let mut parsed = Vec::new();
    for participant in participants {
        let Some(ranges) = participant.ranges.as_deref() else {
            return false;
        };
        parsed.push(parse_ranges(ranges));
    }
    for (index, left) in parsed.iter().enumerate() {
        for right in parsed.iter().skip(index + 1) {
            if left.iter().any(|outer| {
                right
                    .iter()
                    .any(|inner| outer.0 <= inner.1 && inner.0 <= outer.1)
            }) {
                return false;
            }
        }
    }
    true
}

/// Parse a stored `start-end,start-end` range list.
fn parse_ranges(ranges: &str) -> Vec<(u32, u32)> {
    ranges
        .split(',')
        .filter_map(|span| {
            let (start, end) = span.split_once('-')?;
            Some((start.parse().ok()?, end.parse().ok()?))
        })
        .collect()
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
    let base_commit = git_text(checkout, &["merge-base", &base_ref, "HEAD"]).map_err(|_| {
        anyhow!("no merge base between HEAD and {base_ref}; a checkout with no commits cannot be scanned")
    })?;

    // Later sources overwrite earlier ones, so uncommitted state wins over committed state.
    let mut kinds: BTreeMap<String, ChangeKind> = BTreeMap::new();
    for (path, kind) in committed_changes(checkout, &base_commit)? {
        kinds.insert(path, kind);
    }
    for (path, kind) in uncommitted_changes(checkout)? {
        kinds.insert(path, kind);
    }

    let mut regions = changed_regions(checkout, &base_commit).unwrap_or_default();

    let entries = kinds
        .into_iter()
        .map(|(path, reported)| {
            // A lossily-decoded name does not address a real file, so trust neither the
            // filesystem lookup nor the digest for it.
            let addressable = !path.contains('\u{fffd}');
            let absolute = checkout.join(&path);
            let present = addressable && fs::symlink_metadata(&absolute).is_ok();
            // git reports a path as added or modified that can already be gone: a staged file
            // deleted from the worktree, or one removed since the status call. The filesystem is
            // the authority on which it is.
            let change_kind = if addressable && !present {
                ChangeKind::Deleted
            } else {
                reported
            };
            let digest = if change_kind == ChangeKind::Deleted || !addressable {
                None
            } else {
                digest_of(&absolute)
            };
            let ranges = regions.remove(&path);
            FootprintEntry {
                path,
                change_kind,
                digest,
                ranges,
            }
        })
        .collect();
    Ok(WorktreeScan {
        base_ref,
        base_commit,
        branch,
        entries,
    })
}

/// Resolve the integration branch's current head commit.
///
/// Merge readiness compares this against the commit a worktree was cut from, so it has to be the
/// branch tip rather than the merge base.
pub fn integration_head(checkout: &Path, base_ref: Option<&str>) -> Result<(String, String)> {
    let base_ref = resolve_base_ref(checkout, base_ref)?;
    let head = git_text(checkout, &["rev-parse", &base_ref])?;
    Ok((base_ref, head))
}

/// Pick the integration branch to measure against.
fn resolve_base_ref(checkout: &Path, requested: Option<&str>) -> Result<String> {
    if let Some(requested) = requested {
        // Reject a leading dash rather than passing `--`, which would make git read the ref as
        // a pathspec instead of a revision.
        ensure!(
            !requested.starts_with('-'),
            "base ref must not start with a dash: {requested}"
        );
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

/// Regions of the base file each tracked path rewrote, in base-file line numbers.
///
/// One `git diff` covers every tracked path, committed and uncommitted alike, because diffing a
/// commit against the working tree already unions both. Untracked files have no base side and so
/// appear here at all.
fn changed_regions(checkout: &Path, base_commit: &str) -> Result<BTreeMap<String, String>> {
    let raw = git_bytes(
        checkout,
        &[
            "diff",
            "--unified=0",
            "--no-renames",
            "--no-ext-diff",
            "--no-color",
            base_commit,
        ],
    )?;
    let text = String::from_utf8_lossy(&raw);
    let mut regions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            // `--- a/path`, or `--- /dev/null` for an addition, which has no base side.
            current = rest.strip_prefix("a/").map(ToOwned::to_owned);
        } else if let Some(rest) = line.strip_prefix("@@ ")
            && let Some(path) = current.as_ref()
            && let Some(span) = base_span(rest)
        {
            regions.entry(path.clone()).or_default().push(span);
        }
    }
    Ok(regions
        .into_iter()
        .map(|(path, spans)| (path, spans.join(",")))
        .collect())
}

/// Turn the `-start,count` half of a hunk header into an inclusive `start-end` span.
fn base_span(header: &str) -> Option<String> {
    let old = header.split_whitespace().next()?.strip_prefix('-')?;
    let (start, count) = old
        .split_once(',')
        .map_or((old, "1"), |(start, count)| (start, count));
    let start: u32 = start.parse().ok()?;
    let count: u32 = count.parse().ok()?;
    // A pure insertion has count 0 and sits between two base lines; treat it as the single
    // boundary line so two insertions at the same point still register as overlapping.
    let end = start.saturating_add(count.saturating_sub(1)).max(start);
    Some(format!("{start}-{end}"))
}

/// Fingerprint a path's current bytes.
///
/// This is a mesh-internal digest, not a git object id: it only ever has to answer whether two
/// checkouts hold the same content. Hashing in process rather than shelling out to
/// `git hash-object --stdin-paths` matters for three reasons. That subprocess deadlocks once the
/// change set is large enough for its unread stdout to fill the pipe while the parent is still
/// writing paths to its stdin; it aborts the entire scan when any single path cannot be read,
/// which an ordinary staged-then-deleted file or a dirty submodule is enough to cause; and it
/// C-unquotes a leading quote, silently returning another file's digest.
///
/// Anything without comparable content — a directory, a submodule, a nested repository, a
/// symlink, an unreadable or oversized file — yields `None`, which classifies as divergent.
fn digest_of(path: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_HASHED_BYTES {
        return None;
    }
    let content = fs::read(path).ok()?;
    Some(
        Uuid::new_v5(&Uuid::NAMESPACE_OID, &content)
            .simple()
            .to_string(),
    )
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
        ranged(change_kind, digest, base, None)
    }

    fn ranged(
        change_kind: &str,
        digest: Option<&str>,
        base: Option<&str>,
        ranges: Option<&str>,
    ) -> Participant {
        Participant {
            agent_id: "agent".to_owned(),
            agent_name: "agent".to_owned(),
            status: "running".to_owned(),
            change_kind: change_kind.to_owned(),
            digest: digest.map(ToOwned::to_owned),
            base_commit: base.map(ToOwned::to_owned),
            branch: None,
            ranges: ranges.map(ToOwned::to_owned),
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
    fn separable_regions_of_one_file_are_adjacent_not_divergent() {
        let participants = vec![
            ranged("modified", Some("aaa"), None, Some("10-12")),
            ranged("modified", Some("bbb"), None, Some("80-84")),
        ];
        assert_eq!(classify(&participants), "adjacent");
    }

    #[test]
    fn overlapping_regions_remain_divergent() {
        let participants = vec![
            ranged("modified", Some("aaa"), None, Some("10-20")),
            ranged("modified", Some("bbb"), None, Some("18-24")),
        ];
        assert_eq!(classify(&participants), "divergent");
    }

    #[test]
    fn touching_regions_count_as_overlapping() {
        let participants = vec![
            ranged("modified", Some("aaa"), None, Some("10-20")),
            ranged("modified", Some("bbb"), None, Some("20-30")),
        ];
        assert_eq!(classify(&participants), "divergent");
    }

    #[test]
    fn an_unknown_region_is_assumed_to_overlap() {
        let participants = vec![
            ranged("modified", Some("aaa"), None, Some("10-12")),
            ranged("modified", Some("bbb"), None, None),
        ];
        assert_eq!(
            classify(&participants),
            "divergent",
            "a downgrade must be provable"
        );
    }

    #[test]
    fn multiple_disjoint_hunks_stay_adjacent() {
        let participants = vec![
            ranged("modified", Some("aaa"), None, Some("1-5,40-42")),
            ranged("modified", Some("bbb"), None, Some("10-12,80-90")),
        ];
        assert_eq!(classify(&participants), "adjacent");
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
