use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use pidmesh::converge::scan_worktree;
use pidmesh::store::{MeshStore, default_database_path, workspace_root};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "pidmesh", version, about)]
struct Cli {
    #[arg(long, global = true, env = "PIDMESH_DB")]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init {
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    Join {
        #[arg(long)]
        name: String,
        #[arg(long)]
        pid: Option<u32>,
        #[arg(long, default_value = "unknown")]
        provider: String,
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    Heartbeat {
        #[arg(long)]
        agent: Option<String>,
    },
    Leave {
        #[arg(long)]
        agent: Option<String>,
    },
    Remember {
        text: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value = "note")]
        kind: String,
        #[arg(long)]
        key: Option<String>,
        #[arg(long, default_value_t = 0.5)]
        importance: f64,
    },
    Recall {
        query: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    Send {
        text: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long = "to", default_value = "*")]
        recipient: String,
        #[arg(long)]
        correlation_id: Option<String>,
    },
    Inbox {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        ack: bool,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    Claim {
        task: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 300)]
        lease_seconds: u64,
        #[arg(long)]
        detail: Option<String>,
    },
    Release {
        task: String,
        #[arg(long)]
        agent: Option<String>,
    },
    Reserve {
        #[arg(required = true)]
        resources: Vec<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        detail: Option<String>,
        #[arg(long, default_value_t = 300)]
        lease_seconds: u64,
    },
    Resources {
        #[arg(long)]
        agent: Option<String>,
    },
    Unreserve {
        #[arg(required = true)]
        resources: Vec<String>,
        #[arg(long)]
        agent: Option<String>,
    },
    /// Observe this checkout and publish what it actually changed.
    Sync {
        #[arg(long)]
        agent: Option<String>,
        /// Integration branch to measure against; defaults to main, then master.
        #[arg(long)]
        base: Option<String>,
        /// Checkout to observe; defaults to the current directory.
        #[arg(long)]
        checkout: Option<PathBuf>,
    },
    /// Report paths contested by more than one agent's observed footprint.
    Collisions {
        #[arg(long)]
        agent: Option<String>,
    },
    Status {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    Events {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    Wait {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 30.0)]
        timeout_seconds: f64,
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    Gc {
        #[arg(long, default_value_t = 30)]
        stale_seconds: u64,
    },
    Run {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "unknown")]
        provider: String,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, default_value_t = 5.0)]
        heartbeat_seconds: f64,
        #[arg(last = true, required = true)]
        child_command: Vec<OsString>,
    },
    Swarm {
        #[arg(long, default_value = "worker")]
        name_prefix: String,
        #[arg(long, default_value = "unknown")]
        provider: String,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, default_value_t = 5)]
        workers: u16,
        #[arg(long, default_value_t = 5.0)]
        heartbeat_seconds: f64,
        #[arg(long, default_value_t = 2.0)]
        shutdown_grace_seconds: f64,
        #[arg(long)]
        fail_fast: bool,
        #[arg(last = true, required = true)]
        child_command: Vec<OsString>,
    },
    #[command(alias = "ui")]
    Dashboard {
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, default_value_t = 4399)]
        port: u16,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("{}", json!({"error": format!("{error:#}")}));
            ExitCode::from(2)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run() -> Result<u8> {
    let cli = Cli::parse();
    let database = match cli.db {
        Some(path) => path,
        None => default_database_path()?,
    };
    let store = MeshStore::new(&database)?;
    match cli.command {
        Commands::Init { workspace } => emit(&json!({
            "database": database,
            "workspace": workspace_root(workspace.as_deref())?
        }))?,
        Commands::Join {
            name,
            pid,
            provider,
            capabilities,
            workspace,
        } => emit(&store.register_agent(
            &name,
            pid.unwrap_or_else(std::process::id),
            workspace.as_deref(),
            &provider,
            &capabilities,
            None,
        )?)?,
        Commands::Heartbeat { agent } => emit(&json!({
            "updated": store.heartbeat(&agent_id(agent.as_deref())?)?
        }))?,
        Commands::Leave { agent } => emit(&json!({
            "stopped": store.stop_agent(&agent_id(agent.as_deref())?)?
        }))?,
        Commands::Remember {
            text,
            agent,
            kind,
            key,
            importance,
        } => emit(&store.remember(
            &agent_id(agent.as_deref())?,
            &text,
            &kind,
            key.as_deref(),
            importance,
        )?)?,
        Commands::Recall {
            query,
            agent,
            limit,
        } => emit(&store.recall(&agent_id(agent.as_deref())?, &query, limit)?)?,
        Commands::Send {
            text,
            agent,
            recipient,
            correlation_id,
        } => emit(&store.send(
            &agent_id(agent.as_deref())?,
            &text,
            &recipient,
            correlation_id.as_deref(),
        )?)?,
        Commands::Inbox {
            agent,
            all,
            ack,
            limit,
        } => {
            let agent_id = agent_id(agent.as_deref())?;
            let messages = store.inbox(&agent_id, !all, limit)?;
            let acknowledged = if ack {
                let ids = messages
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|message| message["id"].as_i64())
                    .collect::<Vec<_>>();
                store.acknowledge(&agent_id, &ids)?
            } else {
                0
            };
            emit(&json!({"messages": messages, "acknowledged": acknowledged}))?;
        }
        Commands::Claim {
            task,
            agent,
            lease_seconds,
            detail,
        } => emit(&store.claim(
            &agent_id(agent.as_deref())?,
            &task,
            lease_seconds,
            detail.as_deref(),
        )?)?,
        Commands::Release { task, agent } => emit(&json!({
            "released": store.release(&agent_id(agent.as_deref())?, &task)?
        }))?,
        Commands::Reserve {
            resources,
            agent,
            task,
            detail,
            lease_seconds,
        } => emit(&store.reserve_resources(
            &agent_id(agent.as_deref())?,
            &resources,
            task.as_deref(),
            detail.as_deref(),
            lease_seconds,
        )?)?,
        Commands::Resources { agent } => {
            emit(&store.resources(&agent_id(agent.as_deref())?)?)?;
        }
        Commands::Unreserve { resources, agent } => emit(&json!({
            "released": store.release_resources(&agent_id(agent.as_deref())?, &resources)?
        }))?,
        Commands::Sync {
            agent,
            base,
            checkout,
        } => {
            let directory = match checkout {
                Some(path) => path,
                None => std::env::current_dir()?,
            };
            let scan = scan_worktree(&directory, base.as_deref())?;
            emit(&store.publish_footprint(&agent_id(agent.as_deref())?, &scan)?)?;
        }
        Commands::Collisions { agent } => {
            emit(&store.collisions(&agent_id(agent.as_deref())?)?)?;
        }
        Commands::Status { agent, workspace } => {
            emit(&store.status(agent.as_deref(), workspace.as_deref())?)?;
        }
        Commands::Events {
            agent,
            after,
            limit,
        } => emit(&store.events(&agent_id(agent.as_deref())?, after, limit)?)?,
        Commands::Wait {
            agent,
            after,
            timeout_seconds,
            limit,
        } => emit(&store.wait_for_events(
            &agent_id(agent.as_deref())?,
            after,
            duration_from_seconds(timeout_seconds)?,
            Duration::from_millis(100),
            limit,
        )?)?,
        Commands::Gc { stale_seconds } => {
            emit(&store.collect_stale(Duration::from_secs(stale_seconds))?)?;
        }
        Commands::Run {
            name,
            provider,
            workspace,
            heartbeat_seconds,
            child_command,
        } => {
            return run_supervised(
                &store,
                &name,
                &provider,
                workspace.as_deref(),
                duration_from_seconds(heartbeat_seconds)?,
                &child_command,
            );
        }
        Commands::Swarm {
            name_prefix,
            provider,
            workspace,
            workers,
            heartbeat_seconds,
            shutdown_grace_seconds,
            fail_fast,
            child_command,
        } => {
            return run_swarm(
                &store,
                &name_prefix,
                &provider,
                workspace.as_deref(),
                workers,
                duration_from_seconds(heartbeat_seconds)?,
                duration_from_seconds(shutdown_grace_seconds)?,
                fail_fast,
                &child_command,
            );
        }
        Commands::Dashboard { workspace, port } => {
            let workspace = PathBuf::from(workspace_root(workspace.as_deref())?);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to start dashboard runtime")?;
            runtime.block_on(pidmesh::dashboard::serve(store, workspace, port))?;
            return Ok(0);
        }
    }
    Ok(0)
}

struct SwarmWorker {
    agent_id: String,
    name: String,
    child: Child,
    exit_code: Option<i32>,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_swarm(
    store: &MeshStore,
    name_prefix: &str,
    provider: &str,
    workspace: Option<&std::path::Path>,
    worker_count: u16,
    heartbeat_interval: Duration,
    shutdown_grace: Duration,
    fail_fast: bool,
    child_command: &[OsString],
) -> Result<u8> {
    if !(1..=64).contains(&worker_count) {
        return Err(anyhow!("workers must be between 1 and 64"));
    }
    if heartbeat_interval < Duration::from_millis(10) {
        return Err(anyhow!(
            "heartbeat interval must be at least 10 milliseconds"
        ));
    }
    if shutdown_grace > Duration::from_secs(60) {
        return Err(anyhow!("shutdown grace must be at most 60 seconds"));
    }

    let root = workspace_root(workspace)?;
    let swarm_id = format!("swarm-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let stopping = Arc::new(AtomicBool::new(false));
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal_stopping = Arc::clone(&stopping);
    let signal_interrupted = Arc::clone(&interrupted);
    ctrlc::set_handler(move || {
        signal_interrupted.store(true, Ordering::Relaxed);
        signal_stopping.store(true, Ordering::Relaxed);
    })
    .context("failed to install swarm shutdown handler")?;

    let mut workers = Vec::with_capacity(usize::from(worker_count));
    for index in 0..worker_count {
        let name = format!("{name_prefix}-{index}");
        let agent_id = format!(
            "{name}-{swarm_id}-{}",
            &Uuid::new_v4().simple().to_string()[..8]
        );
        let registration = store.register_agent(
            &name,
            std::process::id(),
            workspace,
            provider,
            &["swarm-worker".to_owned()],
            Some(&agent_id),
        )?;
        let environment = json!({
            "PIDMESH_AGENT_ID": agent_id,
            "PIDMESH_AGENT_INDEX": index,
            "PIDMESH_AGENT_NAME": name,
            "PIDMESH_DB": store.path(),
            "PIDMESH_PROVIDER": provider,
            "PIDMESH_SWARM_ID": swarm_id,
            "PIDMESH_SWARM_SIZE": worker_count,
            "PIDMESH_WORKSPACE": root
        });
        let mut command = Command::new(&child_command[0]);
        command.args(&child_command[1..]);
        command.process_group(0);
        for (key, value) in environment.as_object().into_iter().flatten() {
            command.env(key, environment_value(value)?);
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                store.stop_agent(&agent_id)?;
                force_stop_workers(store, &mut workers);
                return Err(error).context("failed to start swarm worker");
            }
        };
        let child_pid = child.id();
        workers.push(SwarmWorker {
            agent_id: agent_id.clone(),
            name,
            child,
            exit_code: None,
        });
        let registered = store.update_agent_pid(&agent_id, child_pid).and_then(|_| {
            store.remember(
                &agent_id,
                &json!({
                    "command": child_command,
                    "environment": environment,
                    "registration": registration,
                    "swarm_id": swarm_id
                })
                .to_string(),
                "session",
                Some("process.start"),
                0.2,
            )
        });
        if let Err(error) = registered {
            force_stop_workers(store, &mut workers);
            return Err(error).context("failed to register spawned swarm worker");
        }
    }

    let mut remaining = workers.len();
    let mut next_heartbeat = Instant::now() + heartbeat_interval;
    let mut shutdown_deadline = None;
    let mut first_failure = None;
    while remaining > 0 {
        if stopping.load(Ordering::Relaxed) && shutdown_deadline.is_none() {
            request_worker_shutdown(&mut workers);
            shutdown_deadline = Some(Instant::now() + shutdown_grace);
        }
        if shutdown_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            force_kill_running(&mut workers);
        }
        for worker in &mut workers {
            if worker.exit_code.is_some() {
                continue;
            }
            if let Some(status) = worker
                .child
                .try_wait()
                .context("failed to poll swarm worker")?
            {
                let code = status.code().unwrap_or(1);
                worker.exit_code = Some(code);
                remaining -= 1;
                store.stop_agent(&worker.agent_id)?;
                if code != 0 && first_failure.is_none() {
                    first_failure = Some(code);
                    if fail_fast {
                        stopping.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        if Instant::now() >= next_heartbeat {
            for worker in workers.iter().filter(|worker| worker.exit_code.is_none()) {
                store.heartbeat(&worker.agent_id)?;
            }
            next_heartbeat = Instant::now() + heartbeat_interval;
        }
        if remaining > 0 {
            thread::sleep(Duration::from_millis(25));
        }
    }

    emit(&json!({
        "event": "swarm.stopped",
        "interrupted": interrupted.load(Ordering::Relaxed),
        "swarm_id": swarm_id,
        "workers": workers.iter().map(|worker| json!({
            "agent_id": worker.agent_id,
            "exit_code": worker.exit_code,
            "name": worker.name,
            "pid": worker.child.id()
        })).collect::<Vec<_>>()
    }))?;
    if interrupted.load(Ordering::Relaxed) {
        Ok(130)
    } else {
        Ok(first_failure
            .and_then(|code| code.try_into().ok())
            .unwrap_or(0))
    }
}

fn environment_value(value: &Value) -> Result<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(anyhow!("unsupported child environment value")),
    }
}

fn request_worker_shutdown(workers: &mut [SwarmWorker]) {
    for worker in workers
        .iter_mut()
        .filter(|worker| worker.exit_code.is_none())
    {
        if let Ok(pid) = i32::try_from(worker.child.id()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGTERM);
        }
    }
}

fn force_kill_running(workers: &mut [SwarmWorker]) {
    for worker in workers
        .iter_mut()
        .filter(|worker| worker.exit_code.is_none())
    {
        if let Ok(pid) = i32::try_from(worker.child.id()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

fn force_stop_workers(store: &MeshStore, workers: &mut [SwarmWorker]) {
    force_kill_running(workers);
    for worker in workers {
        let _ = worker.child.wait();
        let _ = store.stop_agent(&worker.agent_id);
    }
}

fn run_supervised(
    store: &MeshStore,
    name: &str,
    provider: &str,
    workspace: Option<&std::path::Path>,
    heartbeat_interval: Duration,
    child_command: &[OsString],
) -> Result<u8> {
    if heartbeat_interval < Duration::from_millis(10) {
        return Err(anyhow!(
            "heartbeat interval must be at least 10 milliseconds"
        ));
    }
    let agent_id = format!("{name}-run-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let root = workspace_root(workspace)?;
    let mut registration = store.register_agent(
        name,
        std::process::id(),
        workspace,
        provider,
        &[],
        Some(&agent_id),
    )?;
    let environment = json!({
        "PIDMESH_AGENT_ID": agent_id,
        "PIDMESH_DB": store.path(),
        "PIDMESH_WORKSPACE": root
    });
    let mut command = Command::new(&child_command[0]);
    command.args(&child_command[1..]);
    for (key, value) in environment
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| value.as_str().map(|value| (key, value)))
    {
        command.env(key, value);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            store.stop_agent(&agent_id)?;
            return Err(error).context("failed to start supervised command");
        }
    };
    store.update_agent_pid(&agent_id, child.id())?;
    store.remember(
        &agent_id,
        &json!({"command": child_command, "environment": environment}).to_string(),
        "session",
        Some("process.start"),
        0.2,
    )?;
    registration["environment"] = environment;
    emit(&registration)?;

    let running = Arc::new(AtomicBool::new(true));
    let heartbeat_running = Arc::clone(&running);
    let heartbeat_store = store.clone();
    let heartbeat_agent = agent_id.clone();
    let heartbeat = thread::spawn(move || {
        while heartbeat_running.load(Ordering::Relaxed) {
            thread::park_timeout(heartbeat_interval);
            if heartbeat_running.load(Ordering::Relaxed) {
                let _ = heartbeat_store.heartbeat(&heartbeat_agent);
            }
        }
    });
    let status = child
        .wait()
        .context("failed to wait for supervised command")?;
    running.store(false, Ordering::Relaxed);
    heartbeat.thread().unpark();
    heartbeat
        .join()
        .map_err(|_| anyhow!("heartbeat thread panicked"))?;
    store.stop_agent(&agent_id)?;
    Ok(status.code().unwrap_or(1).try_into().unwrap_or(1))
}

fn agent_id(explicit: Option<&str>) -> Result<String> {
    explicit
        .map(ToOwned::to_owned)
        .or_else(|| std::env::var("PIDMESH_AGENT_ID").ok())
        .ok_or_else(|| anyhow!("agent id required: pass --agent or set PIDMESH_AGENT_ID"))
}

fn duration_from_seconds(seconds: f64) -> Result<Duration> {
    if !seconds.is_finite() || seconds.is_sign_negative() {
        bail_duration()
    } else {
        Ok(Duration::from_secs_f64(seconds))
    }
}

fn bail_duration<T>() -> Result<T> {
    Err(anyhow!("duration must be a finite non-negative number"))
}

fn emit(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
