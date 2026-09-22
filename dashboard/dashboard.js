"use strict";

const state = {
  activeTab: "overview",
  confirmAction: null,
  currentDirectory: ".",
  currentFile: null,
  providers: [],
  review: null,
  selectedSessionId: null,
  sessions: [],
  snapshot: null,
  terminal: null,
  terminalReconnect: null,
  token: null,
  view: "ide",
  websocket: null,
  workspaceFilter: "",
};

const byId = (id) => document.getElementById(id);

function node(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

function empty(element, message, className = "pane-empty") {
  element.replaceChildren(node("p", className, message));
}

function selectedSession() {
  return state.sessions.find((session) => session.id === state.selectedSessionId) || null;
}

function normalizeStatus(status) {
  return String(status || "unknown").replaceAll("_", "-");
}

function statusLabel(status) {
  return normalizeStatus(status).replaceAll("-", " ");
}

function relativeTime(timestamp) {
  if (!timestamp) return "—";
  const seconds = Math.max(0, Math.floor((Date.now() - timestamp) / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}

function basename(path) {
  const pieces = String(path || "").split("/").filter(Boolean);
  return pieces.at(-1) || path || "workspace";
}

function loadToken() {
  const parameters = new URLSearchParams(window.location.hash.slice(1));
  const fragmentToken = parameters.get("token");
  if (fragmentToken) {
    sessionStorage.setItem("pidmesh-token", fragmentToken);
    history.replaceState(null, "", `${location.pathname}${location.search}`);
  }
  state.token = fragmentToken || sessionStorage.getItem("pidmesh-token");
  if (!state.token) throw new Error("Dashboard token is missing. Restart `pidmesh dashboard`.");
}

async function api(path, options = {}) {
  const headers = new Headers(options.headers || {});
  headers.set("Authorization", `Bearer ${state.token}`);
  if (options.body && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
  const response = await fetch(path, { ...options, headers });
  const payload = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(payload.error || `Request failed (${response.status})`);
  return payload;
}

function setStatus(message, error = false) {
  byId("status-message").textContent = message;
  byId("status-message").classList.toggle("error", error);
}

function setConnection(connected) {
  const container = byId("connection-state");
  container.classList.toggle("disconnected", !connected);
  container.querySelector("span").textContent = connected ? "Local mesh" : "Disconnected";
}

async function refresh() {
  try {
    const [snapshot, sessions] = await Promise.all([
      api("/api/v1/snapshot?limit=100"),
      api("/api/v1/ide/sessions"),
    ]);
    state.snapshot = snapshot;
    state.sessions = sessions;
    if (state.selectedSessionId && !selectedSession()) state.selectedSessionId = null;
    if (!state.selectedSessionId && sessions.length) state.selectedSessionId = sessions[0].id;
    renderAll();
    setConnection(true);
    setStatus(`Updated ${new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}`);
  } catch (error) {
    setConnection(false);
    setStatus(error.message, true);
  }
}

function renderAll() {
  renderProject();
  renderSessions();
  renderAttention();
  renderSelectedSession();
  renderOperations();
}

function renderProject() {
  if (!state.snapshot) return;
  const workspace = state.snapshot.workspace || "local";
  const project = basename(workspace);
  byId("project-name").textContent = project;
  byId("rail-project-name").textContent = project;
  byId("project-branch").textContent = selectedSession()?.workstream || "local mesh";
}

function workstreamName(session) {
  return String(session.workstream || "Unassigned");
}

function needsAttention(session) {
  return ["ready_for_review", "failed", "rejected", "conflict"].includes(session.status) ||
    (session.out_of_scope || []).length > 0;
}

function workstreamGroups() {
  const groups = new Map();
  for (const session of state.sessions) {
    const searchable = [session.name, session.provider, session.branch, session.task, workstreamName(session)]
      .join(" ")
      .toLowerCase();
    if (state.workspaceFilter && !searchable.includes(state.workspaceFilter)) continue;
    const name = workstreamName(session);
    if (!groups.has(name)) groups.set(name, []);
    groups.get(name).push(session);
  }
  return [...groups.entries()]
    .map(([name, sessions]) => [name, sessions.sort((left, right) => right.created_at - left.created_at)])
    .sort((left, right) => {
      const leftAttention = left[1].some(needsAttention);
      const rightAttention = right[1].some(needsAttention);
      if (leftAttention !== rightAttention) return rightAttention - leftAttention;
      return right[1][0].created_at - left[1][0].created_at;
    });
}

function renderSessions() {
  const container = byId("session-groups");
  const groups = [];
  for (const [label, sessions] of workstreamGroups()) {
    const section = node("section", "session-group");
    const heading = node("div", "session-group-heading");
    heading.append(node("h2", "", label), node("span", "", String(sessions.length)));
    section.append(heading);
    const list = node("div", "session-group-list");
    for (const session of sessions) {
      const button = node("button", "session-item");
      button.type = "button";
      button.classList.toggle("active", session.id === state.selectedSessionId);
      button.addEventListener("click", () => openSession(session.id));
      const dot = node("i", `status-dot status-${normalizeStatus(session.status)}`);
      const copy = node("span", "session-item-copy");
      copy.append(node("strong", "", session.name));
      copy.append(node("span", "", `${session.provider} · ${session.branch}`));
      button.append(dot, copy, node("time", "session-age", relativeTime(session.created_at)));
      list.append(button);
    }
    section.append(list);
    groups.push(section);
  }
  if (!groups.length) {
    const message = state.workspaceFilter
      ? `No workspace matches “${byId("workspace-filter").value.trim()}”.`
      : "No managed runs yet. Create one to start an isolated agent.";
    empty(container, message, "attention-empty");
  }
  else container.replaceChildren(...groups);
}

function attentionSessions() {
  return state.sessions.filter(needsAttention);
}

function renderAttention() {
  const sessions = attentionSessions();
  byId("attention-count").textContent = String(sessions.length);
  const container = byId("attention-list");
  if (!sessions.length) {
    empty(container, "Nothing is waiting for operator action.", "attention-empty");
    return;
  }
  container.replaceChildren(
    ...sessions.map((session) => {
      const button = node("button", `attention-item ${session.status === "failed" ? "critical" : ""}`);
      button.type = "button";
      button.addEventListener("click", () => {
        openSession(session.id);
        setWorkbenchTab(session.status === "ready_for_review" ? "diff" : "overview");
      });
      const icon = node("span", "attention-icon", session.status === "failed" ? "!" : "↗");
      const copy = node("span", "attention-copy");
      copy.append(node("strong", "", session.name));
      const detail = (session.out_of_scope || []).length
        ? `${session.out_of_scope.length} out-of-scope change(s)`
        : statusLabel(session.status);
      copy.append(node("span", "", detail));
      button.append(icon, copy);
      return button;
    }),
  );
}

async function selectSession(sessionId) {
  const changed = sessionId !== state.selectedSessionId;
  state.selectedSessionId = sessionId;
  state.review = null;
  state.currentDirectory = ".";
  state.currentFile = null;
  renderAll();
  closeDrawers();
  if (state.view === "ide") {
    if (changed || !state.terminal) await connectTerminal();
    await Promise.allSettled([loadReview(), loadFiles(".")]);
  }
}

async function openSession(sessionId) {
  setWorkbenchTab("terminal");
  await selectSession(sessionId);
  setAppView("ide");
}

function renderSelectedSession() {
  const session = selectedSession();
  byId("workspace-empty").classList.toggle("hidden", Boolean(session));
  byId("run-overview").classList.toggle("hidden", !session);
  byId("inspector-empty").classList.toggle("hidden", Boolean(session));
  byId("inspector-content").classList.toggle("hidden", !session);
  if (!session) {
    byId("active-run-title").textContent = "Choose or create a workspace";
    byId("active-run-subtitle").textContent = "Each agent gets an isolated branch and worktree.";
    byId("active-status").textContent = "No active run";
    byId("active-status-dot").className = "status-dot";
    byId("activity-diff-count").textContent = "";
    byId("open-editor-button").disabled = true;
    byId("status-worktree").textContent = "No worktree selected";
    byId("status-process").textContent = "PID —";
    byId("status-changes").textContent = "0 changed files";
    byId("stop-button").disabled = true;
    return;
  }
  const status = normalizeStatus(session.status);
  byId("open-editor-button").disabled = false;
  byId("active-run-title").textContent = session.name;
  byId("active-run-subtitle").textContent = `${workstreamName(session)} · ${session.branch}`;
  byId("active-status").textContent = statusLabel(session.status);
  byId("active-status-dot").className = `status-dot status-${status}`;
  byId("task-prompt").textContent = session.prompt;
  renderDetails(byId("execution-details"), [
    ["Workstream", workstreamName(session)],
    ["Provider", session.provider],
    ["Process", `PID ${session.pid}`],
    ["Status", statusLabel(session.status)],
    ["Exit code", session.exit_code ?? "—"],
  ]);
  const scopes = (session.scopes || []).map((scope) => node("span", "scope-chip", scope));
  byId("scope-list").replaceChildren(...scopes);
  renderActivity(session);
  renderInspector(session);
  byId("file-count").textContent = String((session.changed_files || []).length || "");
  byId("diff-count").textContent = String((session.changed_files || []).length || "");
  byId("activity-diff-count").textContent = String((session.changed_files || []).length || "");
  byId("status-worktree").textContent = session.worktree;
  byId("status-process").textContent = `PID ${session.pid}`;
  byId("status-changes").textContent = `${(session.changed_files || []).length} changed files`;
  byId("console-state").textContent = statusLabel(session.status);
  const running = session.status === "running";
  byId("console-input").disabled = !running;
  byId("console-input-form").querySelector("button").disabled = !running;
  byId("stop-button").disabled = !running;
  const reviewable = ["ready_for_review", "failed", "rejected"].includes(session.status);
  byId("reject-button").disabled = !reviewable;
  byId("approve-button").disabled = !reviewable || (session.out_of_scope || []).length > 0;
}

function renderDetails(container, values) {
  container.replaceChildren(
    ...values.map(([term, description]) => {
      const row = node("div");
      row.append(node("dt", "", term), node("dd", "", String(description ?? "—")));
      return row;
    }),
  );
}

function renderActivity(session) {
  const events = [
    ["Workstream assigned", workstreamName(session), session.created_at],
    ["Worktree created", session.worktree, session.created_at],
    ["Provider launched", `${session.provider} · PID ${session.pid}`, session.created_at],
  ];
  if (session.finished_at) events.push(["Agent exited", `Exit ${session.exit_code ?? "—"}`, session.finished_at]);
  if (session.merged_at) events.push(["Changes merged", session.base_branch, session.merged_at]);
  byId("activity-feed").replaceChildren(
    ...events.map(([title, detail, timestamp]) => {
      const item = node("div", "activity-item");
      const copy = node("div");
      copy.append(node("strong", "", title), node("span", "", detail));
      item.append(node("i"), copy, node("time", "", relativeTime(timestamp)));
      return item;
    }),
  );
}

function renderInspector(session) {
  renderDetails(byId("task-details"), [
    ["Workstream", workstreamName(session)],
    ["Task key", session.task],
    ["Agent", session.agent_id],
    ["Provider", session.provider],
    ["Created", relativeTime(session.created_at)],
  ]);
  renderDetails(byId("worktree-details"), [
    ["Base", session.base_branch],
    ["Base SHA", String(session.base_commit).slice(0, 12)],
    ["Branch", session.branch],
    ["Checkout", session.worktree],
  ]);
  const outside = session.out_of_scope || [];
  const reviewState = byId("review-state");
  const heading = outside.length
    ? `${outside.length} scope violation(s)`
    : session.status === "ready_for_review"
      ? "Ready for review"
      : statusLabel(session.status);
  const description = outside.length
    ? outside.join(", ")
    : "Approval validates scope, commits the worktree, and merges into the base branch.";
  reviewState.replaceChildren(node("span", "", heading), node("p", "", description));
}

async function connectTerminal() {
  disconnectTerminal();
  const session = selectedSession();
  const container = byId("console-output");
  container.replaceChildren();
  if (!session || !window.PidMeshTerminal) {
    if (!session) container.textContent = "Select a run to inspect its terminal.";
    return;
  }
  let terminal;
  terminal = window.PidMeshTerminal.create(container, {
    onData(data) {
      if (state.websocket?.readyState === WebSocket.OPEN) {
        state.websocket.send(new TextEncoder().encode(data));
      }
    },
    onResize(cols, rows) {
      if (state.websocket?.readyState === WebSocket.OPEN) {
        state.websocket.send(JSON.stringify({ type: "resize", cols, rows }));
      }
    },
  });
  state.terminal = terminal;
  terminal.write(`\u001b[2mConnecting to ${session.name}…\u001b[0m\r\n`);
  try {
    const { ticket } = await api(`/api/v1/ide/sessions/${encodeURIComponent(session.id)}/attach-ticket`, {
      method: "POST",
    });
    const protocol = location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${protocol}//${location.host}/api/v1/ide/sessions/${encodeURIComponent(session.id)}/terminal?ticket=${encodeURIComponent(ticket)}`;
    const socket = new WebSocket(url);
    socket.binaryType = "arraybuffer";
    socket.addEventListener("open", () => {
      byId("console-state").textContent = "Attached";
      terminal.focus();
    });
    socket.addEventListener("message", (event) => {
      if (event.data instanceof ArrayBuffer) terminal.write(new Uint8Array(event.data));
    });
    socket.addEventListener("close", () => {
      byId("console-state").textContent = "Detached";
      if (selectedSession()?.id === session.id && selectedSession()?.status === "running") {
        state.terminalReconnect = window.setTimeout(connectTerminal, 1800);
      }
    });
    socket.addEventListener("error", () => setStatus("Terminal connection failed", true));
    state.websocket = socket;
  } catch (error) {
    terminal.write(`\r\n\u001b[31m${error.message}\u001b[0m\r\n`);
  }
}

function disconnectTerminal() {
  if (state.terminalReconnect) window.clearTimeout(state.terminalReconnect);
  state.terminalReconnect = null;
  if (state.websocket) state.websocket.close();
  state.websocket = null;
  if (state.terminal) state.terminal.dispose();
  state.terminal = null;
}

async function loadFiles(path = ".") {
  const session = selectedSession();
  if (!session) return;
  try {
    const query = new URLSearchParams({ session: session.id, path });
    const result = await api(`/api/v1/ide/files?${query}`);
    state.currentDirectory = path;
    renderFileTree(result.entries || []);
  } catch (error) {
    empty(byId("file-tree"), error.message, "tree-empty");
  }
}

function renderFileTree(entries) {
  const container = byId("file-tree");
  const buttons = [];
  if (state.currentDirectory !== ".") {
    const parent = state.currentDirectory.split("/").slice(0, -1).join("/") || ".";
    const back = node("button", "tree-item directory", "↰  ..");
    back.type = "button";
    back.addEventListener("click", () => loadFiles(parent));
    buttons.push(back);
  }
  for (const entry of entries) {
    const button = node("button", `tree-item ${entry.type}`, `${entry.type === "directory" ? "▸" : "·"}  ${entry.name}`);
    button.type = "button";
    button.addEventListener("click", () => {
      if (entry.type === "directory") loadFiles(entry.path);
      else loadFile(entry.path);
    });
    buttons.push(button);
  }
  if (!buttons.length) empty(container, "This directory is empty.", "tree-empty");
  else container.replaceChildren(...buttons);
}

async function loadFile(path) {
  const session = selectedSession();
  if (!session) return;
  const query = new URLSearchParams({ session: session.id, path });
  try {
    const result = await api(`/api/v1/ide/file?${query}`);
    state.currentFile = result;
    byId("file-path").textContent = result.path;
    byId("file-content").textContent = result.content;
    byId("copy-file").disabled = false;
  } catch (error) {
    byId("file-path").textContent = path;
    byId("file-content").textContent = error.message;
    byId("copy-file").disabled = true;
  }
}

async function loadReview() {
  const session = selectedSession();
  if (!session) return;
  try {
    state.review = await api(`/api/v1/ide/sessions/${encodeURIComponent(session.id)}/review`);
    renderReview();
  } catch (error) {
    state.review = null;
    empty(byId("change-list"), error.message);
  }
}

function renderReview() {
  const review = state.review;
  if (!review) return;
  const changed = review.changed_files || [];
  const outside = new Set(review.out_of_scope || []);
  byId("change-summary").textContent = `${changed.length} files`;
  byId("diff-count").textContent = String(changed.length || "");
  const list = byId("change-list");
  if (!changed.length) empty(list, "No worktree changes yet.");
  else {
    list.replaceChildren(
      ...changed.map((path) => {
        const button = node("button", `change-item ${outside.has(path) ? "outside-scope" : ""}`);
        button.type = "button";
        button.append(node("span", "change-status", outside.has(path) ? "!" : "M"), node("span", "", path));
        button.addEventListener("click", () => showFileDiff(path));
        return button;
      }),
    );
  }
  byId("diff-path").textContent = changed.length ? "All changes" : "No changes selected";
  byId("diff-stats").textContent = outside.size ? `${outside.size} outside scope` : "Scope valid";
  byId("diff-content").textContent = review.patch || "No textual diff available.";
}

function showFileDiff(path) {
  const patch = state.review?.patch || "";
  const marker = `diff --git a/${path} b/${path}`;
  const start = patch.indexOf(marker);
  let section = patch;
  if (start >= 0) {
    const next = patch.indexOf("\ndiff --git ", start + marker.length);
    section = patch.slice(start, next < 0 ? undefined : next);
  }
  byId("diff-path").textContent = path;
  byId("diff-content").textContent = section || "No textual diff available for this file.";
}

function renderOperations() {
  const snapshot = state.snapshot;
  if (!snapshot) return;
  const stats = snapshot.stats || {};
  const values = [
    ["Running agents", (snapshot.agents || []).filter((agent) => agent.status === "running").length],
    ["Task claims", stats.claims || 0],
    ["Resources", stats.resources || 0],
    ["Memories", stats.memories || 0],
  ];
  byId("operation-metrics").replaceChildren(
    ...values.map(([label, value]) => {
      const card = node("article", "metric-card");
      card.append(node("span", "", label), node("strong", "", String(value)));
      return card;
    }),
  );
  renderOperationRows(
    byId("operation-agents"),
    snapshot.agents || [],
    (agent) => [agent.name, `${agent.provider} · PID ${agent.pid}`, statusLabel(agent.status)],
  );
  const ownership = [
    ...(snapshot.claims || []).map((claim) => ({ title: claim.task_key, detail: `claim · ${claim.agent_name}`, state: "task" })),
    ...(snapshot.resources || []).map((resource) => ({ title: `${resource.resource_type}:${resource.resource_key}`, detail: resource.agent_name, state: "scope" })),
  ];
  renderOperationRows(byId("operation-ownership"), ownership, (item) => [item.title, item.detail, item.state]);
  renderOperationRows(
    byId("operation-events"),
    snapshot.events || [],
    (event) => [event.event_type, event.subject || event.agent_name || "mesh", relativeTime(event.created_at)],
  );
}

function renderOperationRows(container, items, mapper) {
  if (!items.length) {
    empty(container, "No activity to show.");
    return;
  }
  container.replaceChildren(
    ...items.map((item) => {
      const [title, detail, trailing] = mapper(item);
      const row = node("div", "operation-row");
      const copy = node("div", "operation-row-copy");
      copy.append(node("strong", "", title), node("span", "", detail));
      row.append(copy, node("time", "", trailing));
      return row;
    }),
  );
}

function setWorkbenchTab(tab) {
  state.activeTab = tab;
  document.querySelectorAll("[data-workbench-view]").forEach((button) => {
    const active = button.dataset.workbenchView === tab;
    button.classList.toggle("active", active);
    button.setAttribute("aria-selected", String(active));
  });
  for (const name of ["terminal", "overview", "files", "diff"]) {
    byId(`${name}-panel`).classList.toggle("hidden", name !== tab);
  }
  if (tab === "diff") loadReview();
  if (tab === "files") loadFiles(state.currentDirectory);
  const session = selectedSession();
  byId("window-title-label").textContent = session ? `${session.name} — ${tab}` : "Local agent workspace";
}

function setAppView(view) {
  const previousView = state.view;
  state.view = view;
  byId("ide-view").classList.toggle("hidden", view !== "ide");
  byId("operations-view").classList.toggle("hidden", view !== "operations");
  byId("mobile-view-select").value = view;
  document.querySelectorAll("[data-app-view]").forEach((button) => {
    const active = button.dataset.appView === view;
    button.classList.toggle("active", active);
    button.setAttribute("aria-pressed", String(active));
  });
  if (view === "ide" && previousView !== "ide" && selectedSession()) connectTerminal();
  if (view !== "ide" && previousView === "ide") disconnectTerminal();
  if (view === "operations") byId("window-title-label").textContent = "Local agent fleet";
  renderProject();
}

async function copyWorktreePath() {
  const session = selectedSession();
  if (!session) {
    setStatus("Select a workspace first", true);
    return;
  }
  try {
    await navigator.clipboard.writeText(session.worktree);
    setStatus("Worktree path copied");
  } catch {
    setStatus(`Worktree: ${session.worktree}`, true);
  }
}

function taskSlug(value) {
  return value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 72);
}

function suggestTaskLabels() {
  const slug = taskSlug(byId("task-prompt-input").value);
  for (const id of ["task-name", "task-key"]) {
    const input = byId(id);
    if (input.dataset.edited !== "true") input.value = slug;
  }
}

function openNewTask(workstream = "") {
  byId("new-task-form").reset();
  byId("task-name").dataset.edited = "false";
  byId("task-key").dataset.edited = "false";
  byId("task-provider").value = state.providers.find((provider) => provider.available)?.id || "";
  byId("new-task-error").textContent = "";
  byId("task-workstream").value = workstream;
  const options = [...new Set(state.sessions.map(workstreamName))].sort().map((name) => {
    const option = document.createElement("option");
    option.value = name;
    return option;
  });
  byId("workstream-options").replaceChildren(...options);
  updateLaunchPreview();
  byId("new-task-dialog").showModal();
  byId("task-prompt-input").focus();
}

function closeDialog(dialog) {
  if (dialog.open) dialog.close();
}

async function createTask(event) {
  event.preventDefault();
  const submit = byId("create-task-submit");
  const scopes = byId("task-scopes").value.split(/\r?\n/).map((value) => value.trim()).filter(Boolean);
  const request = {
    name: byId("task-name").value.trim(),
    provider: byId("task-provider").value,
    workstream: byId("task-workstream").value.trim(),
    task: byId("task-key").value.trim(),
    prompt: byId("task-prompt-input").value.trim(),
    scopes,
  };
  submit.disabled = true;
  submit.textContent = "Creating worktree…";
  byId("new-task-error").textContent = "";
  try {
    const session = await api("/api/v1/ide/sessions", { method: "POST", body: JSON.stringify(request) });
    closeDialog(byId("new-task-dialog"));
    byId("new-task-form").reset();
    await refresh();
    await openSession(session.id);
    setStatus(`${session.name} launched in an isolated worktree`);
  } catch (error) {
    byId("new-task-error").textContent = error.message;
  } finally {
    submit.disabled = false;
    submit.textContent = "Launch workspace";
  }
}

function updateLaunchPreview() {
  const provider = state.providers.find((item) => item.id === byId("task-provider").value);
  const workstream = byId("task-workstream").value.trim();
  const scopes = byId("task-scopes").value.split(/\r?\n/).filter((value) => value.trim()).length;
  byId("launch-preview").textContent = provider
    ? `${workstream || "Unnamed workstream"} · ${provider.name} · isolated worktree · ${scopes || 0} reserved path(s)`
    : "Choose a provider and describe the task.";
}

function openConfirm({ overline, title, description, label, danger = true, action }) {
  state.confirmAction = action;
  byId("confirm-overline").textContent = overline;
  byId("confirm-title").textContent = title;
  byId("confirm-description").textContent = description;
  byId("confirm-submit").textContent = label;
  byId("confirm-submit").classList.toggle("danger", danger);
  byId("confirm-submit").classList.toggle("primary", !danger);
  byId("confirm-error").textContent = "";
  byId("confirm-dialog").showModal();
}

async function runConfirm(event) {
  event.preventDefault();
  if (!state.confirmAction) return;
  const submit = byId("confirm-submit");
  submit.disabled = true;
  try {
    await state.confirmAction();
    closeDialog(byId("confirm-dialog"));
    state.confirmAction = null;
    await refresh();
  } catch (error) {
    byId("confirm-error").textContent = error.message;
  } finally {
    submit.disabled = false;
  }
}

function confirmStop() {
  const session = selectedSession();
  if (!session) return;
  openConfirm({
    overline: "Stop process",
    title: `Stop ${session.name}?`,
    description: "PidMesh will terminate the managed process group. The worktree and all changes will be preserved for review.",
    label: "Stop agent",
    action: () => api(`/api/v1/ide/sessions/${encodeURIComponent(session.id)}`, { method: "DELETE" }),
  });
}

function confirmReject() {
  const session = selectedSession();
  if (!session) return;
  openConfirm({
    overline: "Review decision",
    title: `Reject ${session.name}?`,
    description: "The worktree will be preserved. No commit or merge will be performed.",
    label: "Reject changes",
    action: () => api(`/api/v1/ide/sessions/${encodeURIComponent(session.id)}/reject`, { method: "POST" }),
  });
}

function confirmApprove() {
  const session = selectedSession();
  if (!session) return;
  const commitMessage = byId("commit-message").value.trim();
  if (!commitMessage) {
    setStatus("Enter a commit message before approving", true);
    byId("commit-message").focus();
    return;
  }
  openConfirm({
    overline: "Approval gate",
    title: `Merge into ${session.base_branch}?`,
    description: "PidMesh will revalidate scope and the primary checkout, commit the worktree, then perform a no-fast-forward merge.",
    label: "Approve & merge",
    danger: false,
    action: () =>
      api(`/api/v1/ide/sessions/${encodeURIComponent(session.id)}/approve`, {
        method: "POST",
        body: JSON.stringify({ commit_message: commitMessage }),
      }),
  });
}

function openRail() {
  document.body.classList.add("rail-open");
  byId("run-rail-toggle").setAttribute("aria-expanded", "true");
}

function openInspector() {
  document.body.classList.add("inspector-open");
  byId("inspector-toggle").setAttribute("aria-expanded", "true");
}

function closeDrawers() {
  document.body.classList.remove("rail-open", "inspector-open");
  byId("run-rail-toggle").setAttribute("aria-expanded", "false");
  byId("inspector-toggle").setAttribute("aria-expanded", "false");
}

function bindEvents() {
  ["new-task-button", "new-task-rail", "empty-new-task", "rail-add-button"].forEach((id) => byId(id).addEventListener("click", () => openNewTask()));
  document.querySelector(".wordmark").addEventListener("click", (event) => {
    event.preventDefault();
    setAppView("ide");
  });
  byId("new-task-close").addEventListener("click", () => closeDialog(byId("new-task-dialog")));
  byId("new-task-cancel").addEventListener("click", () => closeDialog(byId("new-task-dialog")));
  byId("new-task-form").addEventListener("submit", createTask);
  byId("task-provider").addEventListener("change", updateLaunchPreview);
  byId("task-prompt-input").addEventListener("input", suggestTaskLabels);
  for (const id of ["task-name", "task-key"]) {
    byId(id).addEventListener("input", () => {
      byId(id).dataset.edited = "true";
    });
  }
  byId("task-workstream").addEventListener("input", updateLaunchPreview);
  byId("task-scopes").addEventListener("input", updateLaunchPreview);
  document.querySelectorAll("[data-workbench-view]").forEach((button) =>
    button.addEventListener("click", () => setWorkbenchTab(button.dataset.workbenchView)),
  );
  document.querySelectorAll("[data-app-view]").forEach((button) =>
    button.addEventListener("click", () => setAppView(button.dataset.appView)),
  );
  byId("mobile-view-select").addEventListener("change", (event) => setAppView(event.target.value));
  byId("refresh-button").addEventListener("click", refresh);
  byId("activity-details").addEventListener("click", openInspector);
  byId("new-terminal-button").addEventListener("click", () => setWorkbenchTab("terminal"));
  byId("open-editor-button").addEventListener("click", copyWorktreePath);
  byId("workspace-filter").addEventListener("input", (event) => {
    state.workspaceFilter = event.target.value.trim().toLowerCase();
    renderSessions();
  });
  byId("operations-refresh").addEventListener("click", refresh);
  byId("files-refresh").addEventListener("click", () => loadFiles(state.currentDirectory));
  byId("copy-file").addEventListener("click", async () => {
    if (state.currentFile) await navigator.clipboard.writeText(state.currentFile.content);
  });
  byId("clear-console").addEventListener("click", () => state.terminal?.reset());
  byId("console-input-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const input = byId("console-input");
    const text = input.value;
    if (!text) return;
    if (state.websocket?.readyState === WebSocket.OPEN) state.websocket.send(new TextEncoder().encode(`${text}\r`));
    else {
      const session = selectedSession();
      if (session) await api(`/api/v1/ide/sessions/${session.id}/input`, { method: "POST", body: JSON.stringify({ text: `${text}\n` }) });
    }
    input.value = "";
  });
  byId("stop-button").addEventListener("click", confirmStop);
  byId("reject-button").addEventListener("click", confirmReject);
  byId("approve-button").addEventListener("click", confirmApprove);
  byId("confirm-form").addEventListener("submit", runConfirm);
  byId("confirm-close").addEventListener("click", () => closeDialog(byId("confirm-dialog")));
  byId("confirm-cancel").addEventListener("click", () => closeDialog(byId("confirm-dialog")));
  byId("run-rail-toggle").addEventListener("click", openRail);
  byId("run-rail-close").addEventListener("click", closeDrawers);
  byId("inspector-toggle").addEventListener("click", openInspector);
  byId("inspector-close").addEventListener("click", closeDrawers);
  byId("scrim").addEventListener("click", closeDrawers);
  document.addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "n") {
      event.preventDefault();
      openNewTask();
    }
    if ((event.metaKey || event.ctrlKey) && ["1", "2"].includes(event.key)) {
      event.preventDefault();
      setAppView({ "1": "ide", "2": "operations" }[event.key]);
    }
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
      event.preventDefault();
      byId("workspace-filter").focus();
      byId("workspace-filter").select();
    }
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "b") {
      event.preventDefault();
      if (window.matchMedia("(max-width: 860px)").matches) openRail();
      else document.body.classList.toggle("rail-collapsed");
    }
    if ((event.metaKey || event.ctrlKey) && event.shiftKey && event.key.toLowerCase() === "c") {
      event.preventDefault();
      copyWorktreePath();
    }
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "l") {
      event.preventDefault();
      setAppView("ide");
      setWorkbenchTab("diff");
    }
    if (event.key === "Escape") closeDrawers();
  });
  window.addEventListener("beforeunload", disconnectTerminal);
}

async function initialize() {
  try {
    loadToken();
    bindEvents();
    state.providers = await api("/api/v1/ide/providers");
    const select = byId("task-provider");
    select.replaceChildren(
      ...state.providers.map((provider) => {
        const option = node("option", "", `${provider.name}${provider.available ? "" : " — not installed"}`);
        option.value = provider.id;
        option.disabled = !provider.available;
        return option;
      }),
    );
    const available = state.providers.find((provider) => provider.available);
    if (available) select.value = available.id;
    else select.insertBefore(Object.assign(node("option", "", "No supported agent CLI found"), { value: "" }), select.firstChild);
    updateLaunchPreview();
    await refresh();
    if (state.selectedSessionId && state.view === "ide") {
      setWorkbenchTab("terminal");
      await selectSession(state.selectedSessionId);
    }
    scheduleRefresh();
  } catch (error) {
    setConnection(false);
    setStatus(error.message, true);
  }
}

function scheduleRefresh() {
  window.setTimeout(async () => {
    if (!document.hidden) await refresh();
    scheduleRefresh();
  }, 2500);
}

initialize();
