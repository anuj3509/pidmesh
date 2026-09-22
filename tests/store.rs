use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use pidmesh::store::MeshStore;
use serde_json::Value;
use tempfile::TempDir;

fn setup() -> Result<(TempDir, MeshStore)> {
    let directory = tempfile::tempdir()?;
    let store = MeshStore::new(directory.path().join("mesh.db"))?;
    Ok((directory, store))
}

fn register(store: &MeshStore, name: &str, workspace: &std::path::Path) -> Result<String> {
    let value =
        store.register_agent(name, std::process::id(), Some(workspace), "test", &[], None)?;
    Ok(value["agent_id"]
        .as_str()
        .context("missing agent id")?
        .to_owned())
}

fn git(directory: &std::path::Path, arguments: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn memory_is_shared_and_workspace_scoped() -> Result<()> {
    let (directory, store) = setup()?;
    let alpha = directory.path().join("alpha");
    let beta = directory.path().join("beta");
    std::fs::create_dir_all(&alpha)?;
    std::fs::create_dir_all(&beta)?;
    let codex = register(&store, "codex", &alpha)?;
    let claude = register(&store, "claude", &alpha)?;
    let outsider = register(&store, "other", &beta)?;
    store.remember(
        &codex,
        "Use SQLite WAL for concurrent writers",
        "decision",
        Some("storage"),
        0.9,
    )?;
    store.remember(&outsider, "Use a hosted database", "decision", None, 0.5)?;

    let results = store.recall(&claude, "SQLite concurrent", 10)?;
    let memories = results
        .as_array()
        .context("recall did not return an array")?;
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0]["agent_name"], "codex");
    Ok(())
}

#[test]
fn implicit_workspace_unifies_linked_worktrees_and_reports_checkout_context() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let repository = directory.path().join("repository");
    let worktree = directory.path().join("feature-checkout");
    std::fs::create_dir(&repository)?;
    git(&repository, &["init"])?;
    git(
        &repository,
        &["config", "user.email", "pidmesh@example.com"],
    )?;
    git(&repository, &["config", "user.name", "PidMesh Test"])?;
    std::fs::write(repository.join("README.md"), "mesh\n")?;
    git(&repository, &["add", "README.md"])?;
    git(&repository, &["commit", "-m", "initial"])?;
    git(
        &repository,
        &[
            "worktree",
            "add",
            "-b",
            "feature/resource-leases",
            worktree.to_str().context("worktree path")?,
        ],
    )?;

    let database = directory.path().join("mesh.db");
    let binary = env!("CARGO_BIN_EXE_pidmesh");
    let implicit = Command::new(binary)
        .current_dir(&worktree)
        .env_remove("PIDMESH_WORKSPACE")
        .args([
            "--db",
            database.to_str().context("database path")?,
            "join",
            "--name",
            "linked-worker",
        ])
        .output()?;
    assert!(implicit.status.success());
    let registration: Value = serde_json::from_slice(&implicit.stdout)?;
    assert_eq!(
        registration["workspace"],
        repository.canonicalize()?.to_string_lossy().as_ref()
    );
    assert_eq!(
        registration["checkout_path"],
        worktree.canonicalize()?.to_string_lossy().as_ref()
    );
    assert_eq!(registration["git_branch"], "feature/resource-leases");

    let status = Command::new(binary)
        .current_dir(&worktree)
        .env_remove("PIDMESH_WORKSPACE")
        .args([
            "--db",
            database.to_str().context("database path")?,
            "status",
        ])
        .output()?;
    let mesh: Value = serde_json::from_slice(&status.stdout)?;
    assert_eq!(mesh["workspace"], registration["workspace"]);
    assert_eq!(
        mesh["agents"][0]["checkout_path"],
        registration["checkout_path"]
    );
    assert_eq!(mesh["agents"][0]["git_branch"], registration["git_branch"]);

    let explicit = Command::new(binary)
        .current_dir(&worktree)
        .args([
            "--db",
            database.to_str().context("database path")?,
            "join",
            "--name",
            "explicit-worker",
            "--workspace",
            worktree.to_str().context("worktree path")?,
        ])
        .output()?;
    let explicit_registration: Value = serde_json::from_slice(&explicit.stdout)?;
    assert_eq!(
        explicit_registration["workspace"],
        worktree.canonicalize()?.to_string_lossy().as_ref()
    );
    Ok(())
}

#[test]
fn existing_agent_schema_gains_checkout_columns_without_data_loss() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("legacy.db");
    let connection = rusqlite::Connection::open(&database)?;
    connection.execute_batch(
        "CREATE TABLE workspaces (
            id TEXT PRIMARY KEY,
            root TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE agents (
            id TEXT PRIMARY KEY,
            workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            pid INTEGER NOT NULL,
            parent_pid INTEGER,
            provider TEXT NOT NULL,
            capabilities_json TEXT NOT NULL DEFAULT '[]',
            started_at INTEGER NOT NULL,
            heartbeat_at INTEGER NOT NULL,
            stopped_at INTEGER,
            status TEXT NOT NULL DEFAULT 'running'
        );",
    )?;
    drop(connection);

    let store = MeshStore::new(&database)?;
    let agent = register(&store, "migrated", directory.path())?;
    let status = store.status(Some(&agent), None)?;
    assert!(status["agents"][0]["checkout_path"].is_string());
    assert!(status["agents"][0]["git_branch"].is_null());
    Ok(())
}

#[test]
fn dashboard_snapshot_combines_only_the_selected_workspace() -> Result<()> {
    let (directory, store) = setup()?;
    let primary = directory.path().join("primary");
    let other = directory.path().join("other");
    std::fs::create_dir_all(&primary)?;
    std::fs::create_dir_all(&other)?;
    let planner = register(&store, "planner", &primary)?;
    let worker = register(&store, "worker", &primary)?;
    let outsider = register(&store, "outsider", &other)?;
    store.remember(&planner, "primary decision", "decision", None, 0.8)?;
    store.remember(&outsider, "other decision", "decision", None, 0.8)?;
    store.send(&planner, "start implementation", "worker", None)?;
    store.claim(&worker, "src/store.rs", 300, None)?;

    let snapshot = store.dashboard_snapshot(&primary, 100)?;
    assert_eq!(snapshot["stats"]["total_agents"], 2);
    assert_eq!(snapshot["stats"]["memories"], 1);
    assert_eq!(snapshot["stats"]["messages"], 1);
    assert_eq!(snapshot["stats"]["claims"], 1);
    assert_eq!(snapshot["memories"][0]["content"], "primary decision");
    assert_eq!(snapshot["messages"][0]["recipient_name"], "worker");
    assert!(
        snapshot["agents"]
            .as_array()
            .context("agents")?
            .iter()
            .all(|agent| agent["name"] != "outsider")
    );
    Ok(())
}

#[test]
fn messages_have_independent_receipts() -> Result<()> {
    let (directory, store) = setup()?;
    let sender = register(&store, "planner", directory.path())?;
    let worker = register(&store, "worker", directory.path())?;
    let observer = register(&store, "observer", directory.path())?;
    let direct = store.send(&sender, "implement parser", "worker", None)?;
    let broadcast = store.send(&sender, "tests are green", "*", None)?;

    let worker_inbox = store.inbox(&worker, true, 50)?;
    let observer_inbox = store.inbox(&observer, true, 50)?;
    assert_eq!(worker_inbox.as_array().context("worker inbox")?.len(), 2);
    assert_eq!(
        observer_inbox.as_array().context("observer inbox")?.len(),
        1
    );
    let ids = [
        direct["message_id"].as_i64().context("direct id")?,
        broadcast["message_id"].as_i64().context("broadcast id")?,
    ];
    assert_eq!(store.acknowledge(&worker, &ids)?, 2);
    assert_eq!(store.inbox(&worker, true, 50)?, serde_json::json!([]));
    assert_eq!(
        store
            .inbox(&observer, true, 50)?
            .as_array()
            .context("observer inbox")?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn claims_are_exclusive_and_recover_after_expiry() -> Result<()> {
    let (directory, store) = setup()?;
    let first = register(&store, "first", directory.path())?;
    let second = register(&store, "second", directory.path())?;
    assert_eq!(store.claim(&first, "task", 1, None)?["acquired"], true);
    assert_eq!(store.claim(&second, "task", 10, None)?["acquired"], false);
    thread::sleep(Duration::from_millis(1_050));
    let takeover = store.claim(&second, "task", 10, None)?;
    assert_eq!(takeover["acquired"], true);
    assert_eq!(takeover["agent_id"], second);
    Ok(())
}

#[test]
fn resource_leases_detect_hierarchical_conflicts_atomically() -> Result<()> {
    let (directory, store) = setup()?;
    let planner = register(&store, "planner", directory.path())?;
    let worker = register(&store, "worker", directory.path())?;
    let initial = store.reserve_resources(
        &planner,
        &["path:src".to_owned()],
        Some("refactor"),
        None,
        60,
    )?;
    assert_eq!(initial["acquired"], true);

    let blocked = store.reserve_resources(
        &worker,
        &["path:src/store.rs".to_owned(), "port:4399".to_owned()],
        Some("dashboard"),
        None,
        60,
    )?;
    assert_eq!(blocked["acquired"], false);
    assert_eq!(blocked["conflicts"][0]["agent_id"], planner);
    assert!(
        store
            .resources(&worker)?
            .as_array()
            .context("resources")?
            .iter()
            .all(|resource| resource["resource_key"] != "4399")
    );

    let boundary =
        store.reserve_resources(&worker, &["path:src2/store.rs".to_owned()], None, None, 60)?;
    assert_eq!(boundary["acquired"], true);
    assert_eq!(
        store.release_resources(&worker, &["path:src2/store.rs".to_owned()])?,
        1
    );
    assert!(store.stop_agent(&planner)?);
    assert_eq!(
        store.reserve_resources(&worker, &["path:src/store.rs".to_owned()], None, None, 60,)?["acquired"],
        true
    );
    Ok(())
}

#[test]
fn resource_lease_expiry_allows_takeover() -> Result<()> {
    let (directory, store) = setup()?;
    let first = register(&store, "first", directory.path())?;
    let second = register(&store, "second", directory.path())?;
    assert_eq!(
        store.reserve_resources(&first, &["service:indexer".to_owned()], None, None, 1)?["acquired"],
        true
    );
    thread::sleep(Duration::from_millis(1_050));
    assert_eq!(
        store.reserve_resources(&second, &["service:indexer".to_owned()], None, None, 60)?["acquired"],
        true
    );
    assert_eq!(store.resources(&second)?[0]["agent_id"], second);
    Ok(())
}

#[test]
fn dead_agent_collection_releases_resources() -> Result<()> {
    let (directory, store) = setup()?;
    let dead = store.register_agent(
        "dead-worker",
        u32::MAX,
        Some(directory.path()),
        "test",
        &[],
        None,
    )?;
    let dead_id = dead["agent_id"].as_str().context("dead agent id")?;
    store.reserve_resources(dead_id, &["port:4399".to_owned()], None, None, 60)?;
    thread::sleep(Duration::from_millis(2));
    let collected = store.collect_stale(Duration::ZERO)?;
    assert_eq!(collected["dead_agents"], 1);
    assert_eq!(store.resources(dead_id)?, serde_json::json!([]));
    Ok(())
}

#[test]
fn wait_wakes_when_another_agent_sends() -> Result<()> {
    let (directory, store) = setup()?;
    let receiver = register(&store, "receiver", directory.path())?;
    let sender = register(&store, "sender", directory.path())?;
    let existing = store.events(&receiver, 0, 100)?;
    let after = existing
        .as_array()
        .and_then(|events| events.last())
        .and_then(|event| event["sequence"].as_i64())
        .context("missing sequence")?;
    let sending_store = store.clone();
    let sending_sender = sender.clone();
    let handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        sending_store.send(&sending_sender, "wake up", "receiver", None)
    });
    let events = store.wait_for_events(
        &receiver,
        after,
        Duration::from_secs(1),
        Duration::from_millis(10),
        100,
    )?;
    handle.join().expect("sender panicked")?;
    assert_eq!(events[0]["event_type"], "message.sent");
    Ok(())
}

#[test]
fn eight_threads_write_without_data_loss() -> Result<()> {
    let (directory, store) = setup()?;
    let workspace = directory.path().to_path_buf();
    let mut handles = Vec::new();
    for worker in 0..8 {
        let worker_store = store.clone();
        let worker_workspace = workspace.clone();
        handles.push(thread::spawn(move || -> Result<()> {
            let agent = register(
                &worker_store,
                &format!("worker-{worker}"),
                &worker_workspace,
            )?;
            for unit in 0..100 {
                worker_store.remember(
                    &agent,
                    &format!("worker {worker} completed native unit {unit}"),
                    "progress",
                    Some(&format!("{worker}:{unit}")),
                    0.5,
                )?;
            }
            Ok(())
        }));
    }
    for handle in handles {
        handle.join().expect("writer panicked")?;
    }
    let reader = register(&store, "reader", &workspace)?;
    let memories = store.recall(&reader, "native unit", 1_000)?;
    assert_eq!(memories.as_array().context("recall array")?.len(), 800);
    Ok(())
}

#[test]
fn eight_processes_produce_one_claim_winner() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("claims.db");
    let binary = env!("CARGO_BIN_EXE_pidmesh");
    let mut agent_ids = Vec::new();
    for worker in 0..8 {
        let output = Command::new(binary)
            .args([
                "--db",
                database.to_str().context("database path")?,
                "join",
                "--name",
                &format!("worker-{worker}"),
                "--workspace",
                directory.path().to_str().context("workspace path")?,
            ])
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let registration: Value = serde_json::from_slice(&output.stdout)?;
        agent_ids.push(
            registration["agent_id"]
                .as_str()
                .context("agent id")?
                .to_owned(),
        );
    }
    let mut children = Vec::new();
    for agent_id in agent_ids {
        children.push(
            Command::new(binary)
                .args([
                    "--db",
                    database.to_str().context("database path")?,
                    "claim",
                    "exclusive-task",
                    "--agent",
                    &agent_id,
                    "--lease-seconds",
                    "60",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
    }
    let mut winners = 0;
    for child in children {
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let claim: Value = serde_json::from_slice(&output.stdout)?;
        winners += usize::from(claim["acquired"] == true);
    }
    assert_eq!(winners, 1);
    Ok(())
}

#[test]
fn eight_processes_produce_one_overlapping_resource_winner() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("resources.db");
    let binary = env!("CARGO_BIN_EXE_pidmesh");
    let mut agent_ids = Vec::new();
    for worker in 0..8 {
        let output = Command::new(binary)
            .args([
                "--db",
                database.to_str().context("database path")?,
                "join",
                "--name",
                &format!("worker-{worker}"),
                "--workspace",
                directory.path().to_str().context("workspace path")?,
            ])
            .output()?;
        let registration: Value = serde_json::from_slice(&output.stdout)?;
        agent_ids.push(
            registration["agent_id"]
                .as_str()
                .context("agent id")?
                .to_owned(),
        );
    }
    let mut children = Vec::new();
    for (worker, agent_id) in agent_ids.iter().enumerate() {
        let resource = if worker % 2 == 0 {
            "path:src"
        } else {
            "path:src/store.rs"
        };
        children.push(
            Command::new(binary)
                .args([
                    "--db",
                    database.to_str().context("database path")?,
                    "reserve",
                    resource,
                    "--agent",
                    agent_id,
                    "--lease-seconds",
                    "60",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
    }
    let mut winners = 0;
    for child in children {
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reservation: Value = serde_json::from_slice(&output.stdout)?;
        winners += usize::from(reservation["acquired"] == true);
    }
    assert_eq!(winners, 1);
    Ok(())
}

#[test]
fn eight_processes_write_to_one_database() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("writes.db");
    let binary = env!("CARGO_BIN_EXE_pidmesh");
    let mut agent_ids = Vec::new();
    for worker in 0..8 {
        let output = Command::new(binary)
            .args([
                "--db",
                database.to_str().context("database path")?,
                "join",
                "--name",
                &format!("writer-{worker}"),
                "--workspace",
                directory.path().to_str().context("workspace path")?,
            ])
            .output()?;
        let registration: Value = serde_json::from_slice(&output.stdout)?;
        agent_ids.push(
            registration["agent_id"]
                .as_str()
                .context("agent id")?
                .to_owned(),
        );
    }
    let mut children = Vec::new();
    for (worker, agent_id) in agent_ids.iter().enumerate() {
        children.push(
            Command::new(binary)
                .args([
                    "--db",
                    database.to_str().context("database path")?,
                    "remember",
                    &format!("native process write {worker}"),
                    "--agent",
                    agent_id,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
    }
    for child in children {
        assert!(child.wait_with_output()?.status.success());
    }
    let store = MeshStore::new(&database)?;
    let reader = register(&store, "reader", directory.path())?;
    assert_eq!(
        store
            .recall(&reader, "native process write", 100)?
            .as_array()
            .context("recall")?
            .len(),
        8
    );
    Ok(())
}

#[test]
fn supervised_command_receives_identity_and_stops_cleanly() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("run.db");
    let marker = directory.path().join("agent-id.txt");
    let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .args([
            "--db",
            database.to_str().context("database path")?,
            "run",
            "--name",
            "child",
            "--workspace",
            directory.path().to_str().context("workspace path")?,
            "--heartbeat-seconds",
            "0.01",
            "--",
            "/bin/sh",
            "-c",
            &format!("printf %s \"$PIDMESH_AGENT_ID\" > {}", marker.display()),
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let registration: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(std::fs::read_to_string(marker)?, registration["agent_id"]);
    let store = MeshStore::new(database)?;
    let status = store.status(None, Some(directory.path()))?;
    assert_eq!(status["agents"][0]["status"], "stopped");
    Ok(())
}

#[test]
fn swarm_launches_six_distinct_processes_and_stops_them_cleanly() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("swarm.db");
    let marker = directory.path().join("workers.txt");
    let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .args([
            "--db",
            database.to_str().context("database path")?,
            "swarm",
            "--workers",
            "6",
            "--name-prefix",
            "researcher",
            "--provider",
            "test",
            "--workspace",
            directory.path().to_str().context("workspace path")?,
            "--heartbeat-seconds",
            "0.01",
            "--",
            "/bin/sh",
            "-c",
            &format!(
                "printf '%s,%s,%s,%s\\n' \"$PIDMESH_AGENT_ID\" \"$PIDMESH_AGENT_INDEX\" \"$PIDMESH_SWARM_SIZE\" \"$$\" >> {}",
                marker.display()
            ),
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(summary["event"], "swarm.stopped");
    assert_eq!(summary["workers"].as_array().context("workers")?.len(), 6);

    let records = std::fs::read_to_string(marker)?;
    let mut identities = records
        .lines()
        .map(|line| line.split(',').map(str::to_owned).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    identities.sort_by_key(|record| record[1].parse::<usize>().unwrap_or_default());
    assert_eq!(identities.len(), 6);
    for (index, record) in identities.iter().enumerate() {
        assert_eq!(record.len(), 4);
        assert_eq!(record[1], index.to_string());
        assert_eq!(record[2], "6");
        assert_ne!(record[3], std::process::id().to_string());
    }
    let unique_ids = identities
        .iter()
        .map(|record| &record[0])
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(unique_ids.len(), 6);

    let store = MeshStore::new(database)?;
    let status = store.status(None, Some(directory.path()))?;
    assert_eq!(status["agents"].as_array().context("agents")?.len(), 6);
    assert!(
        status["agents"]
            .as_array()
            .context("agents")?
            .iter()
            .all(|agent| agent["status"] == "stopped")
    );
    Ok(())
}

#[test]
fn swarm_fail_fast_propagates_failure_and_stops_other_workers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("fail-fast.db");
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .args([
            "--db",
            database.to_str().context("database path")?,
            "swarm",
            "--workers",
            "4",
            "--fail-fast",
            "--shutdown-grace-seconds",
            "0.05",
            "--workspace",
            directory.path().to_str().context("workspace path")?,
            "--",
            "/bin/sh",
            "-c",
            "if [ \"$PIDMESH_AGENT_INDEX\" = 2 ]; then exit 7; fi; sleep 10",
        ])
        .output()?;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(output.status.code(), Some(7));
    let summary: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(summary["interrupted"], false);
    assert_eq!(
        summary["workers"]
            .as_array()
            .context("workers")?
            .iter()
            .filter(|worker| worker["exit_code"] == 7)
            .count(),
        1
    );
    let store = MeshStore::new(database)?;
    let status = store.status(None, Some(directory.path()))?;
    assert!(
        status["agents"]
            .as_array()
            .context("agents")?
            .iter()
            .all(|agent| agent["status"] == "stopped")
    );
    Ok(())
}

#[test]
fn swarm_interrupt_terminates_every_worker_and_releases_sessions() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("interrupt.db");
    let child = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .args([
            "--db",
            database.to_str().context("database path")?,
            "swarm",
            "--workers",
            "3",
            "--shutdown-grace-seconds",
            "0.1",
            "--workspace",
            directory.path().to_str().context("workspace path")?,
            "--",
            "/bin/sh",
            "-c",
            "trap 'exit 0' TERM; while :; do sleep 10; done",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let store = MeshStore::new(&database)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = store.status(None, Some(directory.path()))?;
        if status["agents"]
            .as_array()
            .is_some_and(|agents| agents.len() == 3)
        {
            break;
        }
        assert!(Instant::now() < deadline, "swarm workers did not register");
        thread::sleep(Duration::from_millis(20));
    }

    kill(Pid::from_raw(i32::try_from(child.id())?), Signal::SIGINT)?;
    let interrupted_at = Instant::now();
    let output = child.wait_with_output()?;
    assert!(interrupted_at.elapsed() < Duration::from_secs(2));
    assert_eq!(output.status.code(), Some(130));
    let summary: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(summary["interrupted"], true);

    let status = store.status(None, Some(directory.path()))?;
    assert!(
        status["agents"]
            .as_array()
            .context("agents")?
            .iter()
            .all(|agent| agent["status"] == "stopped")
    );
    Ok(())
}

#[test]
fn dashboard_api_is_token_protected_and_cleans_up_its_session() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("dashboard.db");
    let mut child = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .args([
            "--db",
            database.to_str().context("database path")?,
            "dashboard",
            "--port",
            "0",
            "--workspace",
            directory.path().to_str().context("workspace path")?,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut startup = String::new();
    BufReader::new(child.stdout.take().context("dashboard stdout")?).read_line(&mut startup)?;
    let startup: Value = serde_json::from_str(&startup)?;
    let address = startup["listening"].as_str().context("listening address")?;
    let token = startup["dashboard"]
        .as_str()
        .and_then(|url| url.split("#token=").nth(1))
        .context("dashboard token")?;

    let health = http_request(address, "GET", "/healthz", &[], "")?;
    assert_eq!(health.0, 200);
    assert!(
        health
            .1
            .to_ascii_lowercase()
            .contains("content-security-policy:")
    );
    let unauthorized = http_request(address, "GET", "/api/v1/snapshot", &[], "")?;
    assert_eq!(unauthorized.0, 401);
    let authorization = format!("Bearer {token}");
    assert_ide_api_security(address, &authorization)?;
    let foreign = http_request(
        address,
        "GET",
        "/api/v1/snapshot",
        &[
            ("Authorization", authorization.as_str()),
            ("Origin", "https://attacker.example"),
        ],
        "",
    )?;
    assert_eq!(foreign.0, 403);
    let remembered = http_request(
        address,
        "POST",
        "/api/v1/memories",
        &[("Authorization", authorization.as_str())],
        r#"{"text":"dashboard memory","kind":"decision","importance":0.9}"#,
    )?;
    assert_eq!(remembered.0, 200, "{}", remembered.2);
    let recalled = http_request(
        address,
        "GET",
        "/api/v1/memories?q=dashboard&limit=10",
        &[("Authorization", authorization.as_str())],
        "",
    )?;
    assert_eq!(recalled.0, 200);
    assert!(recalled.2.contains("dashboard memory"));
    let reserved = http_request(
        address,
        "POST",
        "/api/v1/resources",
        &[("Authorization", authorization.as_str())],
        r#"{"resources":["path:dashboard"],"task":"cockpit","lease_seconds":900}"#,
    )?;
    assert_eq!(reserved.0, 200, "{}", reserved.2);
    assert!(reserved.2.contains(r#""acquired":true"#));
    let resources = http_request(
        address,
        "GET",
        "/api/v1/resources",
        &[("Authorization", authorization.as_str())],
        "",
    )?;
    assert_eq!(resources.0, 200, "{}", resources.2);
    assert!(resources.2.contains("dashboard"));
    let snapshot = http_request(
        address,
        "GET",
        "/api/v1/snapshot",
        &[("Authorization", authorization.as_str())],
        "",
    )?;
    assert_eq!(snapshot.0, 200, "{}", snapshot.2);
    assert!(snapshot.2.contains(r#""resources":1"#));

    kill(Pid::from_raw(i32::try_from(child.id())?), Signal::SIGINT)?;
    assert!(child.wait()?.success());
    let store = MeshStore::new(database)?;
    let status = store.status(None, Some(directory.path()))?;
    assert_eq!(status["agents"][0]["status"], "stopped");
    Ok(())
}

fn assert_ide_api_security(address: &str, authorization: &str) -> Result<()> {
    let providers = http_request(
        address,
        "GET",
        "/api/v1/ide/providers",
        &[("Authorization", authorization)],
        "",
    )?;
    assert_eq!(providers.0, 200);
    assert!(providers.2.contains("claude"));
    let launch_without_origin = http_request(
        address,
        "POST",
        "/api/v1/ide/sessions",
        &[("Authorization", authorization)],
        r#"{"name":"blocked","provider":"claude","workstream":"runtime","task":"blocked","prompt":"blocked","scopes":["src"]}"#,
    )?;
    assert_eq!(launch_without_origin.0, 403);
    Ok(())
}

fn http_request(
    address: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<(u16, String, String)> {
    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    )?;
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    if !body.is_empty() {
        write!(stream, "Content-Type: application/json\r\n")?;
    }
    write!(stream, "\r\n{body}")?;
    stream.flush()?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response.split_once("\r\n\r\n").context("HTTP response")?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("HTTP status")?
        .parse()?;
    Ok((status, head.to_owned(), body.to_owned()))
}
