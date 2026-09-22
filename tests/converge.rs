use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// A repository driven through the CLI, with one linked worktree and joined agent per worker.
///
/// Agents join from inside their checkout so linked-worktree discovery unifies the mesh, which is
/// what a real fleet does and what an explicit `--workspace` would bypass.
struct CliFleet {
    repository: PathBuf,
    database: PathBuf,
    workers: Vec<(String, PathBuf)>,
}

impl CliFleet {
    fn new(root: &Path, workers: usize, pid: Option<u32>) -> Result<Self> {
        let repository = root.join("repository");
        let database = root.join("fleet.db");
        std::fs::create_dir(&repository)?;
        git(&repository, &["init", "-b", "main"])?;
        git(
            &repository,
            &["config", "user.email", "pidmesh@example.com"],
        )?;
        git(&repository, &["config", "user.name", "PidMesh Test"])?;
        std::fs::create_dir(repository.join("src"))?;
        std::fs::write(repository.join("src/shared.rs"), "fn shared() {}\n")?;
        git(&repository, &["add", "-A"])?;
        git(&repository, &["commit", "-m", "initial"])?;

        let mut joined = Vec::new();
        for worker in 0..workers {
            let worktree = root.join(format!("wt-{worker}"));
            git(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &format!("feat/{worker}"),
                    worktree.to_str().context("worktree path")?,
                ],
            )?;
            // Every worktree edits the same path differently, plus one path of its own.
            std::fs::write(
                worktree.join("src/shared.rs"),
                format!("fn shared() {{ /* {worker} */ }}\n"),
            )?;
            std::fs::write(worktree.join(format!("src/only-{worker}.rs")), "private\n")?;
            let mut arguments = vec![
                "--db".to_owned(),
                database.to_string_lossy().into_owned(),
                "join".to_owned(),
                "--name".to_owned(),
                format!("worker-{worker}"),
            ];
            if let Some(pid) = pid {
                arguments.push("--pid".to_owned());
                arguments.push(pid.to_string());
            }
            let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
                .current_dir(&worktree)
                .env_remove("PIDMESH_WORKSPACE")
                .args(&arguments)
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let registration: Value = serde_json::from_slice(&output.stdout)?;
            joined.push((
                registration["agent_id"]
                    .as_str()
                    .context("agent id")?
                    .to_owned(),
                worktree,
            ));
        }
        Ok(Self {
            repository,
            database,
            workers: joined,
        })
    }

    /// Run a pidmesh subcommand from a directory and parse its JSON.
    fn run(&self, directory: &Path, arguments: &[&str]) -> Result<Value> {
        let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
            .current_dir(directory)
            .env_remove("PIDMESH_WORKSPACE")
            .args(["--db", self.database.to_str().context("database path")?])
            .args(arguments)
            .output()?;
        assert!(
            output.status.success(),
            "pidmesh {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

/// Contributing guide: transaction changes need multiprocess coverage.
///
/// Eight independent processes scan eight worktrees and publish footprints concurrently against one
/// `SQLite` database. Every write must land, and the contested path must end up with exactly eight
/// participants rather than a torn or partially-overwritten set.
#[test]
fn eight_processes_publish_footprints_concurrently() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let fleet = CliFleet::new(directory.path(), 8, None)?;
    let (database, workers) = (&fleet.database, &fleet.workers);

    let mut children = Vec::new();
    for (agent_id, worktree) in workers {
        children.push(
            Command::new(env!("CARGO_BIN_EXE_pidmesh"))
                .current_dir(worktree)
                .env_remove("PIDMESH_WORKSPACE")
                .args([
                    "--db",
                    database.to_str().context("database path")?,
                    "sync",
                    "--agent",
                    agent_id,
                    "--base",
                    "main",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
    }
    for child in children {
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "concurrent sync failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let report = fleet.run(
        &fleet.repository,
        &["collisions", "--agent", workers[0].0.as_str()],
    )?;
    let collisions = report["collisions"].as_array().context("collisions")?;
    assert_eq!(
        collisions.len(),
        1,
        "only the shared path is contested: {report}"
    );
    assert_eq!(collisions[0]["path"], "src/shared.rs");
    assert_eq!(
        collisions[0]["participants"].as_array().map(Vec::len),
        Some(8),
        "every concurrent write must survive"
    );
    assert_eq!(collisions[0]["severity"], "divergent");
    Ok(())
}

/// Regression: a large change set used to deadlock.
///
/// Hashing shelled out to `git hash-object --stdin-paths` and wrote every path to its stdin
/// before reading any stdout. Once the child's unread output filled the pipe it stopped reading,
/// the parent filled the stdin pipe, and both blocked forever with no timeout. `pidmesh sync`
/// hung unkillable, and a `watch` sweep froze convergence for the whole fleet.
#[test]
fn a_large_change_set_does_not_hang_the_scan() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let bulk = alpha_tree.join("bulk");
    std::fs::create_dir(&bulk)?;
    // Comfortably past the observed stall threshold in both bytes and count.
    for index in 0..9_000 {
        std::fs::write(
            bulk.join(format!("file-with-a-fairly-long-name-{index:06}.txt")),
            "x".repeat(40),
        )?;
    }
    let report = fleet.sync(&alpha, &alpha_tree)?;
    assert_eq!(report["paths"], 9_000);
    Ok(())
}

/// Regression: one unhashable path used to abort the whole scan.
///
/// `git hash-object` dies on the first path it cannot read, and that failure was turned into a
/// scan-wide error. A staged-then-deleted file, a dirty submodule, or an untracked nested
/// repository was enough to make a checkout permanently unscannable.
#[test]
fn an_unreadable_path_degrades_to_no_digest_instead_of_failing() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;

    // Staged, then removed from the worktree: git reports `AD`, the file is not there.
    std::fs::write(alpha_tree.join("src/staged.rs"), "staged\n")?;
    git(&alpha_tree, &["add", "src/staged.rs"])?;
    std::fs::remove_file(alpha_tree.join("src/staged.rs"))?;

    // An untracked nested repository, which `-uall` reports as a single directory entry.
    let nested = alpha_tree.join("vendor");
    std::fs::create_dir(&nested)?;
    git(&nested, &["init", "-b", "main"])?;
    std::fs::write(nested.join("inner.txt"), "inner\n")?;

    // A real edit alongside them must still be observed.
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;

    let scan = scan_worktree(&alpha_tree, Some("main"))?;
    let shared = scan
        .entries
        .iter()
        .find(|entry| entry.path == "src/shared.rs")
        .context("the ordinary edit must survive")?;
    assert!(shared.digest.is_some(), "a readable file is still hashed");

    let staged = scan
        .entries
        .iter()
        .find(|entry| entry.path == "src/staged.rs")
        .context("the staged-then-deleted path is still reported")?;
    assert_eq!(
        staged.change_kind.as_str(),
        "deleted",
        "the filesystem decides, not the git status letter"
    );
    assert!(staged.digest.is_none());

    fleet.sync(&alpha, &alpha_tree)?;
    Ok(())
}

/// Regression: a filename starting with a quote used to take another file's digest.
///
/// `git hash-object --stdin-paths` C-unquotes any line beginning with `"`, so the footprint for
/// `"quoted.txt"` silently recorded the digest of `quoted.txt`. Two checkouts holding different
/// bytes could then be reported `identical`, which reads as safe to merge.
#[test]
fn a_quoted_filename_does_not_borrow_another_files_digest() -> Result<()> {
    let fleet = Fleet::new()?;
    let (_alpha, alpha_tree) = fleet.agent("alpha")?;
    std::fs::write(alpha_tree.join("quoted.txt"), "plain contents\n")?;
    std::fs::write(alpha_tree.join("\"quoted.txt\""), "different contents\n")?;

    let scan = scan_worktree(&alpha_tree, Some("main"))?;
    let plain = scan
        .entries
        .iter()
        .find(|entry| entry.path == "quoted.txt")
        .and_then(|entry| entry.digest.clone());
    let quoted = scan
        .entries
        .iter()
        .find(|entry| entry.path == "\"quoted.txt\"")
        .and_then(|entry| entry.digest.clone());
    assert!(plain.is_some() && quoted.is_some(), "both are hashed");
    assert_ne!(plain, quoted, "different bytes must never share a digest");
    Ok(())
}

/// Regression: a stale footprint from a co-located session used to argue with its own checkout.
///
/// The contested-path test collapsed agents by checkout but the participant query did not, so a
/// second session in the same worktree contributed an independent opinion. Two byte-identical
/// checkouts were reported `divergent`, the severity that means work is being overwritten.
#[test]
fn a_stale_session_in_one_checkout_does_not_fabricate_divergence() -> Result<()> {
    let fleet = Fleet::new()?;
    let (first, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

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

    // The first session records one version, then the file changes underneath it.
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* v1 */ }\n",
    )?;
    fleet.sync(&first, &alpha_tree)?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* v2 */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* v2 */ }\n",
    )?;
    fleet.sync(&second, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&bravo)?;
    let shared = at(&collisions, "src/shared.rs").context("expected a collision")?;
    assert_eq!(
        shared["severity"], "identical",
        "both worktrees hold the same bytes: {shared}"
    );
    assert_eq!(
        shared["participants"].as_array().map(Vec::len),
        Some(2),
        "one opinion per checkout, not per session"
    );
    Ok(())
}

/// Regression: two agents deleting the same file were reported as divergent.
///
/// Deletions carry no digest, so delete/delete fell past the `delete_edit` branch and past the
/// content comparison into `divergent`. Removing the same dead file on two branches merges
/// cleanly; flagging it as the top severity is a false positive in ordinary cleanup work.
#[test]
fn two_agents_deleting_the_same_file_do_not_conflict() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::remove_file(alpha_tree.join("src/legacy.rs"))?;
    std::fs::remove_file(bravo_tree.join("src/legacy.rs"))?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&alpha)?;
    let legacy = at(&collisions, "src/legacy.rs").context("expected a collision")?;
    assert_eq!(legacy["severity"], "identical", "{legacy}");
    Ok(())
}
