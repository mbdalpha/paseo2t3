// Writes a Paseo 0.11.3-shaped home under trial/out/paseo-home for local trials.
// Expects trial/out/repos/demo (git repo) and trial/out/repos/demo-wt-feature (its worktree).
// Usage: node trial/make-fixture.js <claude-session-id> <codex-id-1> <codex-id-2> <codex-id-3>
const fs = require("fs");
const path = require("path");

const [claudeId, codexWt, codexArchived, codexLegacy] = process.argv.slice(2);
if (!codexLegacy) throw new Error("need 1 claude and 3 codex session ids");

const OUT = path.join(__dirname, "out");
const H = path.join(OUT, "paseo-home");
const DEMO = path.join(OUT, "repos", "demo");
const WT = path.join(OUT, "repos", "demo-wt-feature");
const HOMEDIR = require("os").homedir();
const now = "2026-10-30T12:00:00.000Z";
// Paseo's projectDir: drop the drive colon/slashes, then join path segments with "-".
const projDir = (cwd) => cwd.replace(/^[A-Za-z]:[\\/]/, (m) => m[0]).replace(/^\//, "").replace(/[\\/]+/g, "-");

const projects = [
  { projectId: "prj_0000000000000001", rootPath: DEMO, kind: "git", displayName: "demo", createdAt: now, updatedAt: now, archivedAt: null },
  { projectId: "prj_0000000000000002", rootPath: HOMEDIR, kind: "non_git", displayName: "home", createdAt: now, updatedAt: now, archivedAt: null },
];
const workspaces = [
  { workspaceId: "wks_demo", projectId: "prj_0000000000000001", cwd: DEMO, kind: "local_checkout", displayName: "demo", branch: "master", worktreeRoot: null, isPaseoOwnedWorktree: false, mainRepoRoot: null, createdAt: now, updatedAt: now, archivedAt: null },
  { workspaceId: "wks_feature", projectId: "prj_0000000000000001", cwd: WT, kind: "worktree", displayName: "feature/x", branch: "feature/x", worktreeRoot: WT, baseBranch: "master", isPaseoOwnedWorktree: true, mainRepoRoot: DEMO, createdAt: now, updatedAt: now, archivedAt: null },
  { workspaceId: "wks_home", projectId: "prj_0000000000000002", cwd: HOMEDIR, kind: "directory", displayName: "home", worktreeRoot: null, isPaseoOwnedWorktree: false, mainRepoRoot: null, createdAt: now, updatedAt: now, archivedAt: null },
];

const agent = (id, provider, cwd, extra) => ({
  id, provider, cwd, createdAt: now, updatedAt: now, lastActivityAt: now, title: null, labels: {},
  lastStatus: "idle", config: { provider, cwd, model: null }, ...extra,
});
const persist = (provider, sessionId) => ({ persistence: { provider, sessionId, nativeHandle: sessionId } });

const agents = [
  agent("agt_claude_home", "claude", HOMEDIR, { workspaceId: "wks_home", title: "Storage devices (via Paseo)", config: { provider: "claude", cwd: HOMEDIR, model: "claude-opus-5-5" }, ...persist("claude", claudeId) }),
  agent("agt_codex_wt", "codex", WT, { workspaceId: "wks_feature", title: "Feature work in worktree", config: { provider: "codex", cwd: WT, model: "gpt-6.1-sol" }, ...persist("codex", codexWt) }),
  agent("agt_codex_archived", "codex", DEMO, { workspaceId: "wks_demo", title: "Old archived task", archivedAt: now, ...persist("codex", codexArchived) }),
  agent("agt_copilot", "copilot", DEMO, { workspaceId: "wks_demo", title: "Copilot agent", ...persist("copilot", "copilot-123") }),
  agent("agt_never_started", "claude", DEMO, { workspaceId: "wks_demo", title: "Never ran", persistence: null }),
  agent("agt_internal", "claude", DEMO, { internal: true, ...persist("claude", "internal-1") }),
  agent("agt_missing_dir", "claude", path.join(OUT, "gone"), { title: "Dir gone", ...persist("claude", "11111111-2222-3333-4444-555555555555") }),
];

fs.rmSync(H, { recursive: true, force: true });
for (const a of agents) {
  const dir = path.join(H, "agents", projDir(a.cwd));
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, `${a.id}.json`), JSON.stringify(a, null, 2));
}
// Legacy layout: directly under agents/ and without workspaceId (pre-backfill).
fs.writeFileSync(path.join(H, "agents", "agt_legacy.json"), JSON.stringify(agent("agt_legacy", "codex", DEMO, { title: "Legacy record", ...persist("codex", codexLegacy) }), null, 2));
fs.writeFileSync(path.join(H, "agents", "corrupt.json"), "{ not json");
fs.mkdirSync(path.join(H, "projects"), { recursive: true });
fs.writeFileSync(path.join(H, "projects", "projects.json"), JSON.stringify(projects, null, 2));
fs.writeFileSync(path.join(H, "projects", "workspaces.json"), JSON.stringify(workspaces, null, 2));
fs.writeFileSync(path.join(H, "config.json"), JSON.stringify({ version: 1 }));
console.log(`wrote ${H}`);
