use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use pidmesh::converge::scan_worktree;
use pidmesh::store::MeshStore;
use serde_json::Value;
use tempfile::TempDir;

fn git(directory: &Path, arguments: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

struct Fleet {
    _directory: TempDir,
    repository: PathBuf,
    store: MeshStore,
}

impl Fleet {
    /// A repository with one commit and a mesh scoped to its root.
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let repository = directory.path().join("repository");
        std::fs::create_dir(&repository)?;
        git(&repository, &["init", "-b", "main"])?;
        git(
            &repository,
            &["config", "user.email", "pidmesh@example.com"],
        )?;
        git(&repository, &["config", "user.name", "PidMesh Test"])?;
        std::fs::create_dir(repository.join("src"))?;
        std::fs::write(repository.join("src/shared.rs"), "fn shared() {}\n")?;
        std::fs::write(repository.join("src/legacy.rs"), "legacy\n")?;
        git(&repository, &["add", "-A"])?;
        git(&repository, &["commit", "-m", "initial"])?;
        let store = MeshStore::new(directory.path().join("mesh.db"))?;
        Ok(Self {
            _directory: directory,
            repository,
            store,
        })
    }

    /// Add a linked worktree on its own branch and register an agent for it.
    fn agent(&self, name: &str) -> Result<(String, PathBuf)> {
        let worktree = self.repository.join("..").join(format!("wt-{name}"));
        git(
            &self.repository,
            &[
                "worktree",
                "add",
                "-b",
                &format!("feat/{name}"),
                worktree.to_str().context("worktree path")?,
            ],
        )?;
        let registration = self.store.register_agent(
            name,
            std::process::id(),
            Some(&self.repository),
            "test",
            &[],
            None,
        )?;
        let agent_id = registration["agent_id"]
            .as_str()
            .context("missing agent id")?
            .to_owned();
        // The mesh is shared (one repository root) while each agent works in its own checkout,
        // which is what linked-worktree discovery produces for a real fleet.
        self.store
            .update_agent_checkout(&agent_id, &worktree, Some(&format!("feat/{name}")))?;
        Ok((agent_id, worktree))
    }

    /// Observe a checkout and publish the result, exactly as `pidmesh sync` does.
    fn sync(&self, agent_id: &str, worktree: &Path) -> Result<Value> {
        let scan = scan_worktree(worktree, Some("main"))?;
        self.store.publish_footprint(agent_id, &scan)
    }

    fn collisions(&self, agent_id: &str) -> Result<Vec<Value>> {
        Ok(self.store.collisions(agent_id)?["collisions"]
            .as_array()
            .context("missing collisions")?
            .clone())
    }
}

/// Find the report entry for one path.
fn at<'a>(collisions: &'a [Value], path: &str) -> Option<&'a Value> {
    collisions
        .iter()
        .find(|collision| collision["path"] == path)
}

#[test]
fn overlapping_worktrees_collide_without_anyone_reserving_a_path() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;

    fleet.sync(&alpha, &alpha_tree)?;
    let report = fleet.sync(&bravo, &bravo_tree)?;

    // Neither agent called reserve; the overlap is observed, not declared.
    assert_eq!(report["collision_count"], 1);
    let collisions = fleet.collisions(&alpha)?;
    let shared = at(&collisions, "src/shared.rs").context("expected a collision")?;
    assert_eq!(shared["severity"], "divergent");
    assert_eq!(shared["involves_me"], true);
    assert_eq!(shared["participants"].as_array().map(Vec::len), Some(2));
    Ok(())
}

#[test]
fn byte_identical_edits_are_duplicated_effort_rather_than_a_conflict() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(alpha_tree.join("src/shared.rs"), "fn shared() { same() }\n")?;
    std::fs::write(bravo_tree.join("src/shared.rs"), "fn shared() { same() }\n")?;

    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let shared = fleet.collisions(&alpha)?;
    let shared = at(&shared, "src/shared.rs").context("expected a collision")?;
    assert_eq!(shared["severity"], "identical");
    Ok(())
}

#[test]
fn deleting_a_path_another_agent_is_editing_is_flagged() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(
        alpha_tree.join("src/legacy.rs"),
        "legacy but still needed\n",
    )?;
    std::fs::remove_file(bravo_tree.join("src/legacy.rs"))?;

    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&bravo)?;
    let legacy = at(&collisions, "src/legacy.rs").context("expected a collision")?;
    assert_eq!(legacy["severity"], "delete_edit");
    Ok(())
}

#[test]
fn work_on_separate_paths_never_collides() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(alpha_tree.join("src/only-alpha.rs"), "a\n")?;
    std::fs::write(bravo_tree.join("src/only-bravo.rs"), "b\n")?;

    fleet.sync(&alpha, &alpha_tree)?;
    let report = fleet.sync(&bravo, &bravo_tree)?;

    assert_eq!(report["collision_count"], 0);
    assert!(fleet.collisions(&alpha)?.is_empty());
    Ok(())
}

#[test]
fn a_worktree_cut_from_an_older_base_is_reported_as_divergent() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;

    // Someone lands work on main, so every existing worktree is now stale.
    std::fs::write(
        fleet.repository.join("src/shared.rs"),
        "fn shared() { /* landed */ }\n",
    )?;
    git(&fleet.repository, &["commit", "-am", "land upstream"])?;

    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* landed + b */ }\n",
    )?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&bravo)?;
    let shared = at(&collisions, "src/shared.rs").context("expected a collision")?;
    assert_eq!(shared["severity"], "divergent");
    assert_eq!(
        shared["base_divergent"], true,
        "the two checkouts were cut from different commits"
    );
    Ok(())
}

#[test]
fn a_footprint_is_authoritative_so_abandoned_work_clears_its_collision() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    assert_eq!(fleet.collisions(&alpha)?.len(), 1);

    // Alpha drops its change; the next scan withdraws it from the contested path.
    git(&alpha_tree, &["checkout", "--", "src/shared.rs"])?;
    fleet.sync(&alpha, &alpha_tree)?;

    assert!(fleet.collisions(&alpha)?.is_empty());
    let cleared = fleet.store.events(&alpha, 0, 500)?;
    let cleared = cleared
        .as_array()
        .context("missing events")?
        .iter()
        .filter(|event| event["event_type"] == "collision.cleared")
        .count();
    assert_eq!(cleared, 1);
    Ok(())
}

#[test]
fn a_collision_wakes_peers_once_rather_than_on_every_scan() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let detected = |store: &MeshStore| -> Result<usize> {
        Ok(store
            .events(&alpha, 0, 500)?
            .as_array()
            .context("missing events")?
            .iter()
            .filter(|event| event["event_type"] == "collision.detected")
            .count())
    };
    let first = detected(&fleet.store)?;
    assert_eq!(first, 1, "the overlap is announced when it becomes real");

    // A polling fleet re-scans constantly; that must not flood the event stream.
    for _ in 0..5 {
        fleet.sync(&alpha, &alpha_tree)?;
        fleet.sync(&bravo, &bravo_tree)?;
    }
    assert_eq!(
        detected(&fleet.store)?,
        first,
        "unchanged state must not re-announce"
    );
    Ok(())
}

#[test]
fn committed_and_uncommitted_work_both_count_toward_the_footprint() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // Alpha commits its change; bravo leaves its own dirty in the working tree.
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    git(&alpha_tree, &["commit", "-am", "alpha work"])?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;

    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&alpha)?;
    let shared = at(&collisions, "src/shared.rs").context("expected a collision")?;
    assert_eq!(shared["severity"], "divergent");
    Ok(())
}

#[test]
fn two_sessions_sharing_a_checkout_are_one_editor_not_a_collision() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;

    // A worker commonly runs a CLI session and an MCP session against the same worktree.
    let second = fleet.store.register_agent(
        "alpha-mcp",
        std::process::id(),
        Some(&fleet.repository),
        "test",
        &[],
        None,
    )?;
    let second = second["agent_id"]
        .as_str()
        .context("missing agent id")?
        .to_owned();
    fleet
        .store
        .update_agent_checkout(&second, &alpha_tree, Some("feat/alpha"))?;

    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    let report = fleet.sync(&second, &alpha_tree)?;

    assert_eq!(
        report["collision_count"], 0,
        "one checkout cannot collide with itself"
    );
    Ok(())
}
