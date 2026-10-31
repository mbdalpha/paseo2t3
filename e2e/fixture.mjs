// Fixture stores shaped like Paseo 0.11.3, Claude Code and Codex, for the E2E run.
import fs from "node:fs";
import path from "node:path";

const NOW = "2026-10-30T12:00:00.000Z";

export const IDS = {
  claudeViaPaseo: "0d6c1c3e-5b1e-4f0a-9c39-5a0f3c2e7a11",
  codexWorktree: "01a10000-0000-7000-8000-000000000001",
  codexArchived: "01a10000-0000-7000-8000-000000000002",
  codexLegacy: "01a10000-0000-7000-8000-000000000003",
  claudeNative: "6f1f3a52-9b0e-4c55-8d0d-2c7e7d3b9f22",
  codexNative: "01a10000-0000-7000-8000-000000000004",
  codexSubagent: "01a10000-0000-7000-8000-000000000005",
  claudeMissingDir: "11111111-2222-3333-4444-555555555555",
};

// Paseo's projectDir: drop the drive colon/slashes, then join path segments with "-".
const projectDir = (cwd) => cwd.replace(/^[A-Za-z]:[\\/]/, (m) => m[0]).replace(/^\//, "").replace(/[\\/]+/g, "-");
// Claude Code's project dir: every non-alphanumeric character becomes "-".
const claudeDir = (cwd) => cwd.replace(/[^a-zA-Z0-9]/g, "-");

const writeJson = (file, value) => {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(value, null, 2));
};
const writeJsonl = (file, records) => {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, records.map((r) => JSON.stringify(r)).join("\n") + "\n");
};

/** `demo` is a git repo, `worktree` its linked worktree on `feature/x`. */
export function writePaseoHome(home, { demo, worktree, plain }) {
  fs.rmSync(home, { recursive: true, force: true });
  writeJson(path.join(home, "projects", "projects.json"), [
    { projectId: "prj_0000000000000001", rootPath: demo, kind: "git", displayName: "demo", createdAt: NOW, updatedAt: NOW, archivedAt: null },
    { projectId: "prj_0000000000000002", rootPath: plain, kind: "non_git", displayName: "plain", createdAt: NOW, updatedAt: NOW, archivedAt: null },
  ]);
  writeJson(path.join(home, "projects", "workspaces.json"), [
    { workspaceId: "wks_demo", projectId: "prj_0000000000000001", cwd: demo, kind: "local_checkout", displayName: "demo", branch: "main", worktreeRoot: null, isPaseoOwnedWorktree: false, mainRepoRoot: null, createdAt: NOW, updatedAt: NOW, archivedAt: null },
    { workspaceId: "wks_feature", projectId: "prj_0000000000000001", cwd: worktree, kind: "worktree", displayName: "feature/x", branch: "feature/x", worktreeRoot: worktree, baseBranch: "main", isPaseoOwnedWorktree: true, mainRepoRoot: demo, createdAt: NOW, updatedAt: NOW, archivedAt: null },
    { workspaceId: "wks_plain", projectId: "prj_0000000000000002", cwd: plain, kind: "directory", displayName: "plain", worktreeRoot: null, isPaseoOwnedWorktree: false, mainRepoRoot: null, createdAt: NOW, updatedAt: NOW, archivedAt: null },
  ]);
  writeJson(path.join(home, "config.json"), { version: 1 });

  const agent = (id, provider, cwd, extra) => ({
    id, provider, cwd, createdAt: NOW, updatedAt: NOW, lastActivityAt: NOW, title: null, labels: {},
    lastStatus: "idle", config: { provider, cwd, model: null }, ...extra,
  });
  const persist = (provider, sessionId) => ({ persistence: { provider, sessionId, nativeHandle: sessionId } });
  const agents = [
    agent("agt_claude", "claude", plain, { workspaceId: "wks_plain", title: "Claude via Paseo", config: { provider: "claude", cwd: plain, model: "claude-haiku-5-5" }, ...persist("claude", IDS.claudeViaPaseo) }),
    // Sub-agents of agt_claude: one live, one archived (kept even without --include-archived).
    agent("agt_codex_wt", "codex", worktree, { workspaceId: "wks_feature", title: "Feature work in worktree", labels: { "paseo.parent-agent-id": "agt_claude" }, config: { provider: "codex", cwd: worktree, model: "gpt-6.1-sol" }, ...persist("codex", IDS.codexWorktree) }),
    agent("agt_codex_archived", "codex", demo, { workspaceId: "wks_demo", title: "Old archived task", labels: { "paseo.parent-agent-id": "agt_claude" }, archivedAt: NOW, ...persist("codex", IDS.codexArchived) }),
    agent("agt_copilot", "copilot", demo, { workspaceId: "wks_demo", title: "Copilot agent", ...persist("copilot", "copilot-123") }),
    agent("agt_never_started", "claude", demo, { workspaceId: "wks_demo", title: "Never ran", persistence: null }),
    agent("agt_internal", "claude", demo, { internal: true, ...persist("claude", "internal-1") }),
    agent("agt_missing_dir", "claude", path.join(plain, "gone"), { title: "Dir gone", ...persist("claude", IDS.claudeMissingDir) }),
  ];
  for (const a of agents) writeJson(path.join(home, "agents", projectDir(a.cwd), `${a.id}.json`), a);
  // Legacy layout: directly under agents/ and without workspaceId (pre-backfill).
  writeJson(path.join(home, "agents", "agt_legacy.json"), agent("agt_legacy", "codex", demo, { title: "Legacy record", ...persist("codex", IDS.codexLegacy) }));
  fs.writeFileSync(path.join(home, "agents", "corrupt.json"), "{ not json");
}

export function writeClaudeHome(home, { cwd }) {
  fs.rmSync(home, { recursive: true, force: true });
  const base = { sessionId: IDS.claudeNative, cwd, gitBranch: "main", isSidechain: false };
  writeJsonl(path.join(home, "projects", claudeDir(cwd), `${IDS.claudeNative}.jsonl`), [
    { type: "user", ...base, timestamp: "2026-10-29T10:00:00.000Z", message: { role: "user", content: "Refactor the parser\nso it streams" } },
    { type: "assistant", ...base, timestamp: "2026-10-29T10:00:05.000Z", message: { role: "assistant", model: "claude-haiku-5-5", content: [{ type: "text", text: "Done." }] } },
    { type: "ai-title", aiTitle: "Streaming parser refactor", sessionId: IDS.claudeNative },
  ]);
  // A metadata-only transcript (no user turn) is not a session worth importing.
  writeJsonl(path.join(home, "projects", claudeDir(cwd), "22222222-0000-4000-8000-000000000000.jsonl"), [
    { type: "permission-mode", permissionMode: "default", sessionId: "22222222-0000-4000-8000-000000000000" },
  ]);
}

export function writeCodexHome(home, { cwd }) {
  fs.rmSync(home, { recursive: true, force: true });
  const day = path.join(home, "sessions", "2026", "10", "29");
  writeJsonl(path.join(day, `rollout-2026-10-29T09-00-00-${IDS.codexNative}.jsonl`), [
    { timestamp: "2026-10-29T09:00:00.000Z", type: "session_meta", payload: { id: IDS.codexNative, cwd, timestamp: "2026-10-29T09:00:00.000Z", source: "cli", parent_thread_id: null } },
    { timestamp: "2026-10-29T09:00:01.000Z", type: "turn_context", payload: { cwd, model: "gpt-6.1-sol" } },
    { timestamp: "2026-10-29T09:00:01.000Z", type: "event_msg", payload: { type: "user_message", message: "Add retries to the uploader" } },
    { timestamp: "2026-10-29T09:05:00.000Z", type: "event_msg", payload: { type: "task_complete" } },
  ]);
  writeJsonl(path.join(day, `rollout-2026-10-29T09-01-00-${IDS.codexSubagent}.jsonl`), [
    { timestamp: "2026-10-29T09:01:00.000Z", type: "session_meta", payload: { id: IDS.codexSubagent, cwd, source: { subagent: { other: "guardian" } }, parent_thread_id: IDS.codexNative } },
    { timestamp: "2026-10-29T09:01:01.000Z", type: "event_msg", payload: { type: "user_message", message: "review" } },
  ]);
  writeJsonl(path.join(home, "session_index.jsonl"), [
    { id: IDS.codexNative, thread_name: "Uploader retries", updated_at: "2026-10-29T09:05:00.000Z" },
  ]);
}
