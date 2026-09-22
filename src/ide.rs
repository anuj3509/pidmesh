use std::collections::{BTreeMap, HashMap, VecDeque};
use std::env;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::store::MeshStore;

const MAX_LIVE_SESSIONS: usize = 8;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_CHUNK: usize = 16 * 1024;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_PATCH_BYTES: usize = 1024 * 1024;
const MAX_FILE_BYTES: u64 = 512 * 1024;
const TICKET_TTL_MS: i64 = 30_000;

#[derive(Clone, Deserialize)]
pub struct LaunchRequest {
    pub name: String,
    pub provider: String,
    pub workstream: String,
    pub task: String,
    pub prompt: String,
    pub scopes: Vec<String>,
}

#[derive(Clone, Deserialize)]
pub struct ApproveRequest {
    pub commit_message: String,
}

#[derive(Clone, Serialize)]
pub struct TerminalChunk {
    pub data: Vec<u8>,
    pub sequence: u64,
}

pub struct TerminalAttachment {
    pub backlog: Vec<TerminalChunk>,
    pub receiver: broadcast::Receiver<TerminalChunk>,
    pub session: Value,
}

#[derive(Clone)]
pub struct IdeManager {
    inner: Arc<IdeInner>,
}

struct IdeInner {
    sessions: Mutex<BTreeMap<String, Arc<ManagedSession>>>,
    store: MeshStore,
    tickets: Mutex<HashMap<String, AttachGrant>>,
    workspace: PathBuf,
}

struct AttachGrant {
    expires_at: i64,
    session_id: String,
}

struct ManagedSession {
    agent_id: String,
    base_branch: String,
    base_commit: String,
    branch: String,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    created_at: i64,
    id: String,
    master: Mutex<Box<dyn MasterPty + Send>>,
    name: String,
    output: Mutex<OutputBuffer>,
    prompt: String,
    provider: String,
    runtime: Mutex<RuntimeState>,
    scopes: Vec<String>,
    sender: broadcast::Sender<TerminalChunk>,
    task: String,
    workstream: String,
    worktree: PathBuf,
    writer: Mutex<Box<dyn Write + Send>>,
}

struct SpawnedProvider {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    pid: u32,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

struct RuntimeState {
    approved_at: Option<i64>,
    exit_code: Option<u32>,
    finished_at: Option<i64>,
    merged_at: Option<i64>,
    pid: u32,
    status: String,
}

#[derive(Default)]
struct OutputBuffer {
    bytes: usize,
    chunks: VecDeque<TerminalChunk>,
    next_sequence: u64,
}

impl IdeManager {
    #[must_use]
    pub fn new(store: MeshStore, workspace: PathBuf) -> Self {
        Self {
            inner: Arc::new(IdeInner {
                sessions: Mutex::new(BTreeMap::new()),
                store,
                tickets: Mutex::new(HashMap::new()),
                workspace,
            }),
        }
    }

    pub fn providers(&self) -> Result<Value> {
        let providers = [
            provider_view("codex", "Codex", "codex")?,
            provider_view("claude", "Claude Code", "claude")?,
        ];
        Ok(json!(providers))
    }

    pub fn launch(&self, request: LaunchRequest) -> Result<Value> {
        let executable = resolve_provider(&request.provider)?;
        self.launch_resolved(request, &executable)
    }

    fn launch_resolved(&self, request: LaunchRequest, executable: &Path) -> Result<Value> {
        validate_launch(&request)?;
        ensure!(
            self.live_session_count()? < MAX_LIVE_SESSIONS,
            "at most {MAX_LIVE_SESSIONS} agent sessions may run at once"
        );
        let repository = repository_root(&self.inner.workspace)?;
        let base_branch = git_text(&repository, ["symbolic-ref", "--short", "HEAD"])?;
        let base_commit = git_text(&repository, ["rev-parse", "HEAD"])?;
        let id = Uuid::new_v4().simple().to_string();
        let slug = slugify(&request.name);
        let branch = format!("pidmesh/{slug}-{}", &id[..8]);
        let worktree = managed_worktree_path(&repository, &id)?;
        create_worktree(&repository, &worktree, &branch, &base_commit)?;

        let agent_id = format!("{}-ide-{}", slug, &id[..8]);
        let registration = self.inner.store.register_agent(
            &request.name,
            std::process::id(),
            Some(&repository),
            &request.provider,
            &["ide".to_owned(), "terminal".to_owned()],
            Some(&agent_id),
        );
        if let Err(error) = registration {
            rollback_worktree(&repository, &worktree, &branch);
            return Err(error).context("failed to register managed agent");
        }
        self.inner
            .store
            .update_agent_checkout(&agent_id, &worktree, Some(&branch))?;

        if let Err(error) = self.acquire_scope(&agent_id, &request) {
            let _ = self.inner.store.stop_agent(&agent_id);
            rollback_worktree(&repository, &worktree, &branch);
            return Err(error);
        }

        let spawned = match spawn_provider(
            &request,
            executable,
            &worktree,
            &repository,
            &agent_id,
            self.inner.store.path(),
        ) {
            Ok(spawned) => spawned,
            Err(error) => {
                let _ = self.inner.store.stop_agent(&agent_id);
                rollback_worktree(&repository, &worktree, &branch);
                return Err(error);
            }
        };
        let pid = spawned.pid;
        self.inner.store.update_agent_pid(&agent_id, pid)?;
        let (sender, _) = broadcast::channel(256);
        let session = Arc::new(ManagedSession {
            agent_id,
            base_branch,
            base_commit,
            branch,
            child: Mutex::new(spawned.child),
            created_at: now_ms()?,
            id: id.clone(),
            master: Mutex::new(spawned.master),
            name: request.name,
            output: Mutex::new(OutputBuffer::default()),
            prompt: request.prompt,
            provider: request.provider,
            runtime: Mutex::new(RuntimeState {
                approved_at: None,
                exit_code: None,
                finished_at: None,
                merged_at: None,
                pid,
                status: "running".to_owned(),
            }),
            scopes: request.scopes,
            sender,
            task: request.task,
            workstream: request.workstream,
            worktree,
            writer: Mutex::new(spawned.writer),
        });
        self.session_map()?.insert(id, Arc::clone(&session));
        start_output_pump(Arc::clone(&session), spawned.reader);
        start_monitor(Arc::clone(&session), self.inner.store.clone());
        session_view(&session)
    }

    pub fn sessions(&self) -> Result<Value> {
        let sessions = self
            .session_map()?
            .values()
            .map(|session| session_view(session))
            .collect::<Result<Vec<_>>>()?;
        Ok(json!(sessions))
    }

    pub fn output(&self, session_id: &str, after: u64) -> Result<Value> {
        let session = self.session(session_id)?;
        let output = lock(&session.output)?;
        let chunks = output
            .chunks
            .iter()
            .filter(|chunk| chunk.sequence > after)
            .map(|chunk| {
                json!({
                    "sequence": chunk.sequence,
                    "text": String::from_utf8_lossy(&chunk.data)
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"chunks": chunks, "next_sequence": output.next_sequence}))
    }

    pub fn input(&self, session_id: &str, text: &str) -> Result<()> {
        ensure!(!text.is_empty(), "terminal input cannot be empty");
        ensure!(
            text.len() <= MAX_INPUT_BYTES,
            "terminal input exceeds {MAX_INPUT_BYTES} bytes"
        );
        let session = self.session(session_id)?;
        ensure!(
            lock(&session.runtime)?.status == "running",
            "agent session is not running"
        );
        let mut writer = lock(&session.writer)?;
        writer.write_all(text.as_bytes())?;
        writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<()> {
        ensure!(
            (40..=400).contains(&cols),
            "terminal columns must be 40-400"
        );
        ensure!((10..=160).contains(&rows), "terminal rows must be 10-160");
        self.session(session_id)?
            .master
            .lock()
            .map_err(|_| anyhow!("terminal lock is poisoned"))?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        Ok(())
    }

    pub fn stop(&self, session_id: &str) -> Result<Value> {
        let session = self.session(session_id)?;
        {
            let mut runtime = lock(&session.runtime)?;
            if runtime.status != "running" {
                return session_view(&session);
            }
            "stopping".clone_into(&mut runtime.status);
        }
        signal_session(&session, Signal::SIGTERM)?;
        session_view(&session)
    }

    pub fn session_is_live(&self, session_id: &str) -> Result<bool> {
        Ok(matches!(
            lock(&self.session(session_id)?.runtime)?.status.as_str(),
            "running" | "stopping"
        ))
    }

    pub fn stop_all(&self) {
        let sessions = self
            .session_map()
            .map(|sessions| sessions.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for session in &sessions {
            let _ = self.stop(&session.id);
        }
        thread::sleep(Duration::from_secs(2));
        for session in sessions {
            if lock(&session.runtime)
                .is_ok_and(|runtime| matches!(runtime.status.as_str(), "running" | "stopping"))
            {
                let _ = signal_session(&session, Signal::SIGKILL);
                let exit = session
                    .child
                    .lock()
                    .ok()
                    .and_then(|mut child| child.wait().ok());
                if let Ok(mut runtime) = session.runtime.lock() {
                    runtime.exit_code = exit.map(|status| status.exit_code());
                    runtime.finished_at = now_ms().ok();
                    "stopped".clone_into(&mut runtime.status);
                }
                let _ = self.inner.store.stop_agent(&session.agent_id);
            }
        }
    }

    pub fn issue_ticket(&self, session_id: &str) -> Result<Value> {
        self.session(session_id)?;
        let ticket = Uuid::new_v4().simple().to_string();
        let expires_at = now_ms()? + TICKET_TTL_MS;
        lock(&self.inner.tickets)?.insert(
            ticket.clone(),
            AttachGrant {
                expires_at,
                session_id: session_id.to_owned(),
            },
        );
        Ok(json!({"ticket": ticket, "expires_at": expires_at}))
    }

    pub fn attach(&self, session_id: &str, ticket: &str) -> Result<TerminalAttachment> {
        let grant = lock(&self.inner.tickets)?
            .remove(ticket)
            .context("terminal ticket is invalid or already used")?;
        ensure!(
            grant.session_id == session_id,
            "terminal ticket is for another session"
        );
        ensure!(grant.expires_at >= now_ms()?, "terminal ticket expired");
        let session = self.session(session_id)?;
        let backlog = lock(&session.output)?.chunks.iter().cloned().collect();
        Ok(TerminalAttachment {
            backlog,
            receiver: session.sender.subscribe(),
            session: session_view(&session)?,
        })
    }

    pub fn review(&self, session_id: &str) -> Result<Value> {
        let session = self.session(session_id)?;
        review_value(&session)
    }

    pub fn approve(&self, session_id: &str, request: &ApproveRequest) -> Result<Value> {
        ensure!(
            !request.commit_message.trim().is_empty() && request.commit_message.len() <= 256,
            "commit message must contain 1-256 bytes"
        );
        let session = self.session(session_id)?;
        ensure!(
            lock(&session.runtime)?.status != "running",
            "stop the agent before approving its changes"
        );
        let review = review_value(&session)?;
        let changed = review["changed_files"]
            .as_array()
            .context("review did not contain changed files")?;
        ensure!(
            !changed.is_empty(),
            "agent worktree has no changes to approve"
        );
        let outside = review["out_of_scope"]
            .as_array()
            .context("review did not contain scope results")?;
        ensure!(
            outside.is_empty(),
            "out-of-scope changes must be resolved before approval"
        );
        ensure!(
            git_text(&self.inner.workspace, ["status", "--porcelain"])?.is_empty(),
            "primary checkout is dirty; commit or stash it before merging"
        );
        ensure!(
            git_text(&self.inner.workspace, ["branch", "--show-current"])? == session.base_branch,
            "primary checkout is no longer on {}",
            session.base_branch
        );

        git_status(&session.worktree, [OsStr::new("add"), OsStr::new("-A")])?;
        git_status(
            &session.worktree,
            [
                OsStr::new("commit"),
                OsStr::new("-m"),
                OsStr::new(request.commit_message.trim()),
            ],
        )?;
        git_status(
            &self.inner.workspace,
            [
                OsStr::new("merge"),
                OsStr::new("--no-ff"),
                OsStr::new(&session.branch),
                OsStr::new("-m"),
                OsStr::new(request.commit_message.trim()),
            ],
        )?;
        let mut runtime = lock(&session.runtime)?;
        let now = now_ms()?;
        runtime.approved_at = Some(now);
        runtime.merged_at = Some(now);
        "merged".clone_into(&mut runtime.status);
        drop(runtime);
        session_view(&session)
    }

    pub fn reject(&self, session_id: &str) -> Result<Value> {
        let session = self.session(session_id)?;
        ensure!(
            lock(&session.runtime)?.status != "running",
            "stop the agent before rejecting its changes"
        );
        "rejected".clone_into(&mut lock(&session.runtime)?.status);
        session_view(&session)
    }

    pub fn files(&self, session_id: Option<&str>, relative: &str) -> Result<Value> {
        let root = self.checkout_root(session_id)?;
        let directory = safe_path(&root, relative)?;
        ensure!(directory.is_dir(), "requested path is not a directory");
        let mut entries = fs::read_dir(&directory)?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != ".git")
            .map(|entry| {
                let path = entry.path();
                let metadata = entry.metadata()?;
                let relative = path
                    .strip_prefix(&root)
                    .context("file escaped checkout")?
                    .to_string_lossy()
                    .to_string();
                Ok(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "path": relative,
                    "type": if metadata.is_dir() { "directory" } else { "file" },
                    "size": metadata.len()
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        entries.sort_by(|left, right| {
            left["type"]
                .as_str()
                .cmp(&right["type"].as_str())
                .then_with(|| left["name"].as_str().cmp(&right["name"].as_str()))
        });
        Ok(json!({"path": relative, "entries": entries}))
    }

    pub fn file(&self, session_id: Option<&str>, relative: &str) -> Result<Value> {
        let root = self.checkout_root(session_id)?;
        let path = safe_path(&root, relative)?;
        let metadata = path.metadata()?;
        ensure!(metadata.is_file(), "requested path is not a file");
        ensure!(
            metadata.len() <= MAX_FILE_BYTES,
            "file is larger than {MAX_FILE_BYTES} bytes"
        );
        let bytes = fs::read(path)?;
        ensure!(!bytes.contains(&0), "binary files cannot be previewed");
        Ok(json!({
            "path": relative,
            "content": String::from_utf8(bytes).context("file is not UTF-8 text")?
        }))
    }

    fn acquire_scope(&self, agent_id: &str, request: &LaunchRequest) -> Result<()> {
        let task_key = format!("{}:{}", slugify(&request.workstream), request.task);
        let claim =
            self.inner
                .store
                .claim(agent_id, &task_key, 86_400, Some("managed IDE session"))?;
        ensure!(
            claim["acquired"] == true,
            "task is already claimed by another agent"
        );
        let resources = request
            .scopes
            .iter()
            .map(|scope| format!("path:{scope}"))
            .collect::<Vec<_>>();
        let reservation = self.inner.store.reserve_resources(
            agent_id,
            &resources,
            Some(&task_key),
            Some("managed IDE scope"),
            86_400,
        )?;
        if reservation["acquired"] != true {
            let _ = self.inner.store.release(agent_id, &task_key);
            bail!("one or more requested paths are reserved by another agent");
        }
        Ok(())
    }

    fn checkout_root(&self, session_id: Option<&str>) -> Result<PathBuf> {
        session_id.map_or_else(
            || Ok(self.inner.workspace.clone()),
            |session_id| Ok(self.session(session_id)?.worktree.clone()),
        )
    }

    fn live_session_count(&self) -> Result<usize> {
        self.session_map()?.values().try_fold(0, |count, session| {
            Ok(count + usize::from(lock(&session.runtime)?.status == "running"))
        })
    }

    fn session(&self, session_id: &str) -> Result<Arc<ManagedSession>> {
        self.session_map()?
            .get(session_id)
            .cloned()
            .context("agent session was not found")
    }

    fn session_map(&self) -> Result<MutexGuard<'_, BTreeMap<String, Arc<ManagedSession>>>> {
        lock(&self.inner.sessions)
    }
}

fn session_view(session: &ManagedSession) -> Result<Value> {
    let runtime = lock(&session.runtime)?;
    let changed_files = changed_files(&session.worktree).unwrap_or_default();
    let out_of_scope = outside_scope(&changed_files, &session.scopes);
    Ok(json!({
        "id": session.id,
        "agent_id": session.agent_id,
        "name": session.name,
        "provider": session.provider,
        "workstream": session.workstream,
        "task": session.task,
        "prompt": session.prompt,
        "scopes": session.scopes,
        "base_branch": session.base_branch,
        "base_commit": session.base_commit,
        "branch": session.branch,
        "worktree": session.worktree,
        "pid": runtime.pid,
        "status": runtime.status,
        "exit_code": runtime.exit_code,
        "created_at": session.created_at,
        "finished_at": runtime.finished_at,
        "approved_at": runtime.approved_at,
        "merged_at": runtime.merged_at,
        "changed_files": changed_files,
        "out_of_scope": out_of_scope
    }))
}

fn spawn_provider(
    request: &LaunchRequest,
    executable: &Path,
    worktree: &Path,
    repository: &Path,
    agent_id: &str,
    database: &Path,
) -> Result<SpawnedProvider> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open an agent terminal")?;
    let prompt = managed_prompt(request);
    let mut command = provider_command(&request.provider, executable, &request.name, &prompt);
    command.cwd(worktree);
    command.env("PIDMESH_AGENT_ID", agent_id);
    command.env("PIDMESH_AGENT_NAME", &request.name);
    command.env("PIDMESH_PROVIDER", &request.provider);
    command.env("PIDMESH_WORKSTREAM", &request.workstream);
    command.env("PIDMESH_DB", database);
    command.env("PIDMESH_WORKSPACE", repository);
    let mut child = pair
        .slave
        .spawn_command(command)
        .context("failed to launch agent provider")?;
    let pid = child
        .process_id()
        .context("agent provider did not expose a process id")?;
    let reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("failed to attach terminal output");
        }
    };
    let writer = match pair.master.take_writer() {
        Ok(writer) => writer,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("failed to attach terminal input");
        }
    };
    Ok(SpawnedProvider {
        child,
        master: pair.master,
        pid,
        reader,
        writer,
    })
}

fn provider_view(id: &str, name: &str, executable: &str) -> Result<Value> {
    let resolved = find_executable(executable);
    Ok(json!({
        "id": id,
        "name": name,
        "available": resolved.is_some(),
        "executable": resolved.map(|path| path.to_string_lossy().to_string())
    }))
}

fn resolve_provider(provider: &str) -> Result<PathBuf> {
    let executable = match provider {
        "claude" => "claude",
        "codex" => "codex",
        _ => bail!("unknown agent provider"),
    };
    find_executable(executable)
        .with_context(|| format!("{executable} is not installed or not on PATH"))
}

fn find_executable(name: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
            .and_then(|candidate| candidate.canonicalize().ok())
    })
}

fn provider_command(provider: &str, executable: &Path, name: &str, prompt: &str) -> CommandBuilder {
    let mut command = CommandBuilder::new(executable);
    match provider {
        "claude" => command.args(["--permission-mode", "acceptEdits", "--name", name, prompt]),
        "codex" => command.arg(prompt),
        _ => unreachable!("provider was validated"),
    }
    command
}

fn managed_prompt(request: &LaunchRequest) -> String {
    format!(
        "You are running inside a PidMesh-managed worktree.\nWorkstream: {}\nTask: {}\nAllowed paths: {}\nUse PidMesh for shared context and handoffs. Do not change files outside the allowed paths. Finish by summarizing changes and tests.\n\n{}",
        request.workstream,
        request.task,
        request.scopes.join(", "),
        request.prompt.trim()
    )
}

fn validate_launch(request: &LaunchRequest) -> Result<()> {
    validate_short("name", &request.name)?;
    validate_short("workstream", &request.workstream)?;
    validate_short("task", &request.task)?;
    ensure!(
        !request.prompt.trim().is_empty() && request.prompt.len() <= 64 * 1024,
        "prompt must contain 1-65536 bytes"
    );
    ensure!(
        !request.scopes.is_empty() && request.scopes.len() <= 64,
        "scope must contain 1-64 relative paths"
    );
    for scope in &request.scopes {
        validate_relative(scope)?;
    }
    ensure!(
        matches!(request.provider.as_str(), "codex" | "claude"),
        "unknown agent provider"
    );
    Ok(())
}

fn validate_short(label: &str, value: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= 256,
        "{label} must contain 1-256 bytes"
    );
    Ok(())
}

fn validate_relative(value: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(!path.is_absolute(), "scope paths must be relative");
    ensure!(
        !value.trim().is_empty() && value.len() <= 1024,
        "scope paths must contain 1-1024 bytes"
    );
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir)),
        "scope paths cannot contain parent traversal"
    );
    Ok(())
}

fn repository_root(workspace: &Path) -> Result<PathBuf> {
    let root = git_text(workspace, ["rev-parse", "--show-toplevel"])?;
    PathBuf::from(root)
        .canonicalize()
        .context("failed to resolve repository root")
}

fn managed_worktree_path(repository: &Path, session_id: &str) -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    let repository_id = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        repository.as_os_str().as_encoded_bytes(),
    );
    Ok(PathBuf::from(home)
        .join(".pidmesh")
        .join("worktrees")
        .join(&repository_id.simple().to_string()[..12])
        .join(session_id))
}

fn create_worktree(
    repository: &Path,
    worktree: &Path,
    branch: &str,
    base_commit: &str,
) -> Result<()> {
    ensure!(!worktree.exists(), "managed worktree path already exists");
    if let Some(parent) = worktree.parent() {
        fs::create_dir_all(parent)?;
    }
    git_status(
        repository,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--no-track"),
            OsStr::new("-b"),
            OsStr::new(branch),
            worktree.as_os_str(),
            OsStr::new(base_commit),
        ],
    )
}

fn rollback_worktree(repository: &Path, worktree: &Path, branch: &str) {
    let _ = git_status(
        repository,
        [
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            worktree.as_os_str(),
        ],
    );
    let _ = git_status(
        repository,
        [OsStr::new("branch"), OsStr::new("-D"), OsStr::new(branch)],
    );
}

fn git_text<const N: usize>(directory: &Path, arguments: [&str; N]) -> Result<String> {
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
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn git_status<I, S>(directory: &Path, arguments: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
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
    Ok(())
}

fn changed_files(worktree: &Path) -> Result<Vec<String>> {
    let output = git_text(
        worktree,
        ["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    Ok(output
        .lines()
        .filter_map(|line| line.get(3..))
        .map(|path| path.rsplit_once(" -> ").map_or(path, |(_, target)| target))
        .map(|path| path.trim_matches('"').to_owned())
        .collect())
}

fn outside_scope(changed: &[String], scopes: &[String]) -> Vec<String> {
    changed
        .iter()
        .filter(|path| {
            !scopes.iter().any(|scope| {
                scope == "."
                    || path.as_str() == scope
                    || path
                        .strip_prefix(scope)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
        })
        .cloned()
        .collect()
}

fn review_value(session: &ManagedSession) -> Result<Value> {
    let changed_files = changed_files(&session.worktree)?;
    let out_of_scope = outside_scope(&changed_files, &session.scopes);
    let mut patch = git_text(
        &session.worktree,
        ["diff", "--no-ext-diff", "--no-color", "HEAD"],
    )?;
    for path in changed_files
        .iter()
        .filter(|path| session.worktree.join(path).is_file())
    {
        if git_text(&session.worktree, ["ls-files", "--error-unmatch", path]).is_err() {
            let file_path = session.worktree.join(path);
            if file_path.metadata()?.len() <= MAX_FILE_BYTES {
                let bytes = fs::read(&file_path)?;
                if !bytes.contains(&0) {
                    write!(
                        patch,
                        "\ndiff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n"
                    )?;
                    for line in String::from_utf8_lossy(&bytes).lines() {
                        patch.push('+');
                        patch.push_str(line);
                        patch.push('\n');
                    }
                }
            }
        }
        if patch.len() >= MAX_PATCH_BYTES {
            patch.truncate(MAX_PATCH_BYTES);
            patch.push_str("\n[diff truncated by PidMesh]\n");
            break;
        }
    }
    Ok(json!({
        "changed_files": changed_files,
        "out_of_scope": out_of_scope,
        "patch": patch,
        "scope_valid": out_of_scope.is_empty()
    }))
}

fn safe_path(root: &Path, relative: &str) -> Result<PathBuf> {
    if !relative.is_empty() && relative != "." {
        validate_relative(relative)?;
    }
    let canonical_root = root.canonicalize()?;
    let selected = if relative.is_empty() || relative == "." {
        canonical_root.clone()
    } else {
        canonical_root.join(relative).canonicalize()?
    };
    ensure!(
        selected.starts_with(&canonical_root),
        "path escaped managed checkout"
    );
    Ok(selected)
}

fn start_output_pump(session: Arc<ManagedSession>, mut reader: Box<dyn Read + Send>) {
    thread::spawn(move || {
        let mut buffer = vec![0_u8; MAX_OUTPUT_CHUNK];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if let Ok(mut output) = session.output.lock() {
                        output.next_sequence += 1;
                        let chunk = TerminalChunk {
                            data: buffer[..read].to_vec(),
                            sequence: output.next_sequence,
                        };
                        output.bytes += chunk.data.len();
                        output.chunks.push_back(chunk.clone());
                        while output.bytes > MAX_OUTPUT_BYTES {
                            if let Some(removed) = output.chunks.pop_front() {
                                output.bytes = output.bytes.saturating_sub(removed.data.len());
                            } else {
                                break;
                            }
                        }
                        drop(output);
                        let _ = session.sender.send(chunk);
                    }
                }
            }
        }
    });
}

fn start_monitor(session: Arc<ManagedSession>, store: MeshStore) {
    thread::spawn(move || {
        loop {
            let result = session
                .child
                .lock()
                .ok()
                .and_then(|mut child| child.try_wait().ok())
                .flatten();
            if let Some(exit) = result {
                if let Ok(mut runtime) = session.runtime.lock() {
                    let stopped = runtime.status == "stopping";
                    runtime.exit_code = Some(exit.exit_code());
                    runtime.finished_at = now_ms().ok();
                    runtime.status = if stopped {
                        "stopped".to_owned()
                    } else if exit.success() {
                        "ready_for_review".to_owned()
                    } else {
                        "failed".to_owned()
                    };
                }
                let _ = store.stop_agent(&session.agent_id);
                break;
            }
            let _ = store.heartbeat(&session.agent_id);
            thread::sleep(Duration::from_secs(1));
        }
    });
}

fn signal_session(session: &ManagedSession, signal: Signal) -> Result<()> {
    let pid = i32::try_from(lock(&session.runtime)?.pid)?;
    killpg(Pid::from_raw(pid), signal).context("failed to signal agent process group")
}

fn slugify(value: &str) -> String {
    let mut result = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while result.contains("--") {
        result = result.replace("--", "-");
    }
    let result = result.trim_matches('-');
    if result.is_empty() {
        "agent".to_owned()
    } else {
        result.chars().take(48).collect()
    }
}

fn now_ms() -> Result<i64> {
    Ok(i64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| anyhow!("IDE state lock is poisoned"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::thread;
    use std::time::Duration;

    use anyhow::{Context, Result};

    use super::{
        ApproveRequest, IdeManager, LaunchRequest, git_status, outside_scope, rollback_worktree,
        session_view, slugify, validate_relative,
    };
    use crate::store::MeshStore;

    fn git(directory: &Path, arguments: &[&str]) -> Result<()> {
        git_status(directory, arguments)
    }

    #[test]
    fn scope_boundaries_are_hierarchical() {
        let changed = vec![
            "src/lib.rs".to_owned(),
            "src2/lib.rs".to_owned(),
            "README.md".to_owned(),
        ];
        assert_eq!(
            outside_scope(&changed, &["src".to_owned(), "README.md".to_owned()]),
            vec!["src2/lib.rs"]
        );
    }

    #[test]
    fn generated_slug_is_safe_for_git_branches() {
        assert_eq!(slugify(" Billing / API!!! "), "billing-api");
        assert_eq!(slugify("***"), "agent");
    }

    #[test]
    fn traversal_is_not_a_valid_scope() {
        assert!(validate_relative("src/api").is_ok());
        assert!(validate_relative("../secrets").is_err());
        assert!(validate_relative("/tmp/secrets").is_err());
    }

    #[test]
    fn managed_run_streams_pty_output_and_blocks_scope_violations() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let repository = directory.path().join("repository");
        fs::create_dir_all(repository.join("src"))?;
        git(&repository, &["init", "-b", "main"])?;
        git(
            &repository,
            &["config", "user.email", "pidmesh@example.com"],
        )?;
        git(&repository, &["config", "user.name", "PidMesh Test"])?;
        fs::write(repository.join("src/lib.rs"), "pub fn existing() {}\n")?;
        git(&repository, &["add", "."])?;
        git(&repository, &["commit", "-m", "initial"])?;

        let provider = directory.path().join("fake-claude");
        fs::write(
            &provider,
            "#!/bin/sh\nprintf 'fake terminal ready\\r\\n'\nprintf 'allowed\\n' > src/allowed.txt\nprintf 'outside\\n' > README.generated\n",
        )?;
        let mut permissions = provider.metadata()?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&provider, permissions)?;

        let store = MeshStore::new(directory.path().join("mesh.db"))?;
        let manager = IdeManager::new(store, repository.clone());
        let launched = manager.launch_resolved(
            LaunchRequest {
                name: "scope-check".to_owned(),
                provider: "claude".to_owned(),
                workstream: "runtime".to_owned(),
                task: "scope-check".to_owned(),
                prompt: "Create one allowed and one disallowed file".to_owned(),
                scopes: vec!["src".to_owned()],
            },
            &provider,
        )?;
        assert_eq!(launched["workstream"], "runtime");
        let session_id = launched["id"].as_str().context("missing session id")?;
        for _ in 0..40 {
            let active = manager.session(session_id)?;
            if session_view(active.as_ref())?["status"] != "running" {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let output = manager.output(session_id, 0)?;
        assert!(output.to_string().contains("fake terminal ready"));
        let review = manager.review(session_id)?;
        assert_eq!(review["scope_valid"], false);
        assert_eq!(review["out_of_scope"][0], "README.generated");
        assert!(
            manager
                .approve(
                    session_id,
                    &ApproveRequest {
                        commit_message: "test: managed change".to_owned(),
                    },
                )
                .is_err()
        );

        let session = manager.session(session_id)?;
        rollback_worktree(&repository, &session.worktree, &session.branch);
        if let Some(parent) = session.worktree.parent() {
            let _ = fs::remove_dir(parent);
        }
        Ok(())
    }
}
