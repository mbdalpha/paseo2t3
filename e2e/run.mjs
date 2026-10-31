// End-to-end test: import fixture stores into a real, isolated T3 Code server and check
// what T3 persisted. Needs Node >= 22.13 (node:sqlite), git, and the `t3` npm package.
//
//   PASEO2T3_BIN   path to the built paseo2t3 binary (default: target/release/paseo2t3)
//   T3_RUNTIME_HOME  where the t3 launcher caches its runtime (default: e2e/.cache/t3)
//   E2E_KEEP=1     keep the temp directory for inspection
//   T3_SRC         a T3 Code source checkout of the same version with `pnpm install` done;
//                  when set, also runs t3-history/backfill.ts (needs Node 26: T3_NODE)
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { IDS, writeClaudeHome, writeCodexHome, writePaseoHome } from "./fixture.mjs";

const here = import.meta.dirname;
const repoRoot = path.dirname(here);
const exe = process.platform === "win32" ? ".exe" : "";
const bin = path.resolve(process.env.PASEO2T3_BIN || path.join(repoRoot, "target", "release", `paseo2t3${exe}`));
const t3Launcher = path.join(here, "node_modules", "t3", "bin", "t3.mjs");
const runtimeHome = path.resolve(process.env.T3_RUNTIME_HOME || path.join(here, ".cache", "t3"));
const t3Version = JSON.parse(fs.readFileSync(path.join(here, "node_modules", "t3", "package.json"), "utf8")).version;

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "paseo2t3-e2e-"));
const p = (...parts) => path.join(tmp, ...parts);
const demo = p("repos", "demo");
const worktree = p("repos", "demo-wt-feature");
const plain = p("plain");
const t3Data = p("t3data");
const tokenFile = p("token.txt");
const fakeClaude = p("bin", "claude.mjs");
const fakeClaudeLog = p("bin", "fake-claude.log");
// The Claude SDK passes some flags as `--flag=value` and others as two arguments.
const argAfter = (args, flag) => {
  const i = args.indexOf(flag);
  if (i >= 0) return args[i + 1];
  return args.find((a) => a.startsWith(`${flag}=`))?.slice(flag.length + 1);
};
// Turns the stub ran for imported sessions (title generation also calls claude; skip those).
const claudeCalls = () =>
  (fs.existsSync(fakeClaudeLog) ? fs.readFileSync(fakeClaudeLog, "utf8").trim().split("\n").map((l) => JSON.parse(l)) : [])
    .filter((args) => [IDS.claudeNative, IDS.claudeViaPaseo].includes(argAfter(args, "--session-id") ?? argAfter(args, "--resume")));

const log = (...args) => console.log("[e2e]", ...args);
const env = { ...process.env, T3CODE_HOME: runtimeHome };
const t3 = (...args) => execFileSync(process.execPath, [t3Launcher, ...args], { env, encoding: "utf8" });
const git = (...args) => execFileSync("git", ["-c", "user.name=e2e", "-c", "user.email=e2e@example.invalid", ...args], { encoding: "utf8" });

const common = [
  "--paseo-home", p("paseo"), "--claude-home", p("claude"), "--codex-home", p("codex"),
  "--source", "paseo,claude,codex", "--include-archived",
];
const toolEnv = { ...process.env };
delete toolEnv.T3_TOKEN;
const tool = (...args) => {
  try {
    return execFileSync(bin, args, { encoding: "utf8", env: toolEnv });
  } catch (e) {
    throw new Error(`paseo2t3 ${args.join(" ")} failed:\n${e.stdout}\n${e.stderr}`);
  }
};

async function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer().listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
    srv.on("error", reject);
  });
}

// One Effect-RPC call over T3's WebSocket, the way the web client sends commands.
async function rpc(origin, token, method, payload) {
  // A keep-alive socket the server already closed fails once with UND_ERR_SOCKET; retry.
  const getTicket = () => fetch(`${origin}/api/auth/websocket-ticket/rpc`, { method: "POST", headers: { Authorization: `Bearer ${token}` } });
  const { ticket } = await (await getTicket().catch(getTicket)).json();
  const ws = new WebSocket(`${origin.replace(/^http/, "ws")}/ws?orchestrationProtocol=2&wsTicket=${ticket}`);
  return new Promise((resolve, reject) => {
    ws.onerror = reject;
    ws.onopen = () => ws.send(JSON.stringify({ _tag: "Request", id: "1", tag: method, payload, headers: [] }));
    ws.onmessage = (e) => {
      for (const m of [JSON.parse(e.data)].flat()) {
        if (m._tag === "Ping") ws.send(JSON.stringify({ _tag: "Pong" }));
        if (m._tag === "Exit" || m._tag === "Defect") {
          ws.close();
          if (m._tag === "Exit" && m.exit._tag === "Success") resolve(m.exit.value);
          else reject(new Error(`${method}: ${JSON.stringify(m)}`));
        }
      }
    };
  });
}

async function waitFor(what, fn, timeoutMs = 120_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await fn().catch(() => undefined);
    if (value) return value;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 500));
  }
}

let serverPid;
let serverLog = "";
try {
  log(`t3 ${t3Version}, binary ${bin}, temp ${tmp}`);
  fs.mkdirSync(path.dirname(demo), { recursive: true });
  fs.mkdirSync(plain, { recursive: true });
  git("init", "-q", "-b", "main", demo);
  git("-C", demo, "commit", "-q", "--allow-empty", "-m", "init");
  git("-C", demo, "worktree", "add", "-q", "-b", "feature/x", worktree);
  writePaseoHome(p("paseo"), { demo, worktree, plain });
  writeClaudeHome(p("claude"), { cwd: demo });
  writeCodexHome(p("codex"), { cwd: demo });

  // scan is read-only and needs no server.
  const scan = JSON.parse(tool("scan", ...common, "--json"));
  const scanned = new Set(scan.sessions.map((s) => s.native_id));
  for (const id of [IDS.claudeViaPaseo, IDS.codexWorktree, IDS.codexArchived, IDS.codexLegacy, IDS.claudeNative, IDS.codexNative, IDS.claudeMissingDir]) {
    assert.ok(scanned.has(id), `scan should find ${id}`);
  }
  assert.ok(!scanned.has(IDS.codexSubagent), "sub-agent threads are skipped");
  const reasons = scan.skipped.map((s) => s.reason).join("\n");
  for (const r of ["internal Paseo agent", "agent never started", "`copilot`", "unreadable", "sub-agent thread", "no user messages"]) {
    assert.ok(reasons.includes(r), `expected a skip for: ${r}\n${reasons}`);
  }
  const wt = scan.sessions.find((s) => s.native_id === IDS.codexWorktree);
  assert.equal(path.resolve(wt.project_root), path.resolve(demo));
  assert.equal(path.resolve(wt.worktree_path), path.resolve(worktree));
  assert.equal(wt.branch, "feature/x");
  assert.equal(wt.parent_agent_id, "agt_claude");
  const withoutArchived = JSON.parse(tool("scan", ...common.filter((a) => a !== "--include-archived"), "--json"));
  const kept = withoutArchived.sessions.map((s) => s.native_id);
  assert.ok(kept.includes(IDS.codexArchived), "an archived sub-agent of a live agent is kept");
  log("scan ok");

  // T3 runs Claude threads through a stub, so linking is checked without model calls.
  fs.mkdirSync(path.dirname(fakeClaude), { recursive: true });
  fs.copyFileSync(path.join(here, "fake-claude.mjs"), fakeClaude);
  fs.mkdirSync(path.join(t3Data, "userdata"), { recursive: true });
  fs.writeFileSync(
    path.join(t3Data, "userdata", "settings.json"),
    JSON.stringify({ providerInstances: { claudeAgent: { driver: "claudeAgent", config: { binaryPath: fakeClaude } } } }),
  );

  const port = await freePort();
  log(`starting t3 serve on ${port} (first run downloads the runtime)`);
  const server = spawn(process.execPath, [t3Launcher, "serve", "--base-dir", t3Data, "--host", "127.0.0.1", "--port", String(port)], { env, stdio: ["ignore", "pipe", "pipe"] });
  server.stdout.on("data", (d) => (serverLog += d));
  server.stderr.on("data", (d) => (serverLog += d));
  const runtimeFile = path.join(t3Data, "userdata", "server-runtime.json");
  const origin = await waitFor("t3 server", async () => {
    if (server.exitCode !== null) throw new Error(`t3 exited early (${server.exitCode})`);
    const rt = JSON.parse(fs.readFileSync(runtimeFile, "utf8"));
    serverPid = rt.pid;
    const res = await fetch(`${rt.origin}/api/projects`);
    return res.status > 0 ? rt.origin : undefined;
  }, 300_000);
  log(`t3 up at ${origin} (pid ${serverPid})`);

  fs.writeFileSync(tokenFile, t3("auth", "session", "issue", "--base-dir", t3Data, "--token-only", "--ttl", "30m", "--label", "e2e").trim());
  const target = ["--t3-home", t3Data, "--token-file", tokenFile];

  const dry = tool("import", ...common, ...target);
  assert.match(dry, /Dry run: nothing was written/);
  const before = await (await fetch(`${origin}/api/projects`, { headers: { Authorization: `Bearer ${fs.readFileSync(tokenFile, "utf8")}` } })).json();
  assert.equal(before.projects.length, 0, "dry run must not create projects");
  log("dry run ok");

  const first = tool("import", ...common, ...target, "--apply");
  assert.match(first, /Done: 6 imported, 0 already present, 0 failure/, first);
  assert.match(first, /does not exist here/, "missing project root is reported");
  // The native Claude session has a transcript in the Claude home, so its thread is linked:
  // one throwaway turn that asks for a new session under the id and is refused.
  assert.match(first, new RegExp(`linked: the next message resumes session ${IDS.claudeNative.slice(0, 8)}`), first);
  // The Paseo Claude agent has no transcript there, so a first turn would start a new session.
  assert.match(first, /not linked: no transcript at/, first);
  const linkCalls = claudeCalls();
  assert.equal(linkCalls.length, 1, `one link turn: ${JSON.stringify(linkCalls)}`);
  assert.equal(argAfter(linkCalls[0], "--session-id"), IDS.claudeNative);
  log("import ok");

  const db = new DatabaseSync(path.join(t3Data, "userdata", "statev2.sqlite"), { readOnly: true });
  const refs = new Map(
    db.prepare("SELECT payload_json FROM orchestration_v2_projection_provider_threads").all()
      .map((r) => JSON.parse(r.payload_json))
      .map((pt) => [pt.nativeThreadRef?.nativeId, { driver: pt.nativeThreadRef?.driver, threadId: pt.appThreadId }]),
  );
  const expected = {
    [IDS.claudeViaPaseo]: "claudeAgent", [IDS.claudeNative]: "claudeAgent",
    [IDS.codexWorktree]: "codex", [IDS.codexArchived]: "codex", [IDS.codexLegacy]: "codex", [IDS.codexNative]: "codex",
  };
  assert.equal(refs.size, Object.keys(expected).length, `provider threads: ${[...refs.keys()]}`);
  for (const [id, driver] of Object.entries(expected)) assert.equal(refs.get(id)?.driver, driver, `native ref for ${id}`);

  const archived = db.prepare("SELECT thread_id, archived_at FROM orchestration_v2_projection_threads").all();
  const archivedIds = archived.filter((t) => t.archived_at).map((t) => t.thread_id);
  assert.deepEqual(archivedIds, [refs.get(IDS.codexArchived).threadId], "only the archived agent is archived");
  assert.equal(db.prepare("SELECT count(*) AS n FROM projection_projects").get().n, 2, "demo and plain projects");

  const shown = JSON.parse(t3("thread", "show", refs.get(IDS.codexWorktree).threadId, "--base-dir", t3Data, "--json"));
  assert.equal(shown.thread.branch, "feature/x");
  assert.equal(path.resolve(shown.thread.worktreePath), path.resolve(worktree));
  const titles = JSON.parse(t3("thread", "list", "--base-dir", t3Data, "--project", demo, "--json")).map((t) => t.title);
  for (const title of ["Streaming parser refactor", "Uploader retries", "Legacy record"]) assert.ok(titles.includes(title), `title ${title} in ${titles}`);
  db.close();
  log("persisted state ok");

  // After linking, the user's first message resumes the original session.
  const token = fs.readFileSync(tokenFile, "utf8").trim();
  const linked = refs.get(IDS.claudeNative).threadId;
  try {
    t3("thread", "send", linked, "continue", "--base-dir", t3Data, "--wait");
  } catch {} // the stub fails every turn; only its arguments matter here
  const sendCall = claudeCalls()[1];
  assert.ok(sendCall, "the first message reached claude");
  assert.equal(argAfter(sendCall, "--resume"), IDS.claudeNative, `first message resumes: ${JSON.stringify(sendCall)}`);
  assert.equal(argAfter(sendCall, "--session-id"), undefined);
  log("first message resumes the linked session");

  // An unlinked thread's first web message fails and T3 retitles the thread from it.
  const unlinked = refs.get(IDS.claudeViaPaseo).threadId;
  await rpc(origin, token, "orchestration.dispatchCommand", {
    type: "message.dispatch", commandId: crypto.randomUUID(), threadId: unlinked, messageId: crypto.randomUUID(),
    text: "resume", attachments: [], titleSeed: "resume", createdBy: "user", creationSource: "web",
    dispatchMode: { type: "start_immediately" },
  });
  const showTitle = (id) => JSON.parse(t3("thread", "show", id, "--base-dir", t3Data, "--json")).thread;
  await waitFor("the failed first turn", async () => {
    const t = showTitle(unlinked);
    return t.title !== "Claude via Paseo" && claudeCalls().length === 3;
  });
  log(`unlinked thread retitled to "${showTitle(unlinked).title}"`);

  const second = tool("import", ...common, ...target, "--apply");
  assert.match(second, /Done: 0 imported, 6 already present, 0 failure/, second);
  assert.match(second, /title restored from/, second);
  assert.equal(showTitle(unlinked).title, "Claude via Paseo", "re-import restores the imported title");
  assert.equal(claudeCalls().length, 3, "re-import starts no turns");
  const db2 = new DatabaseSync(path.join(t3Data, "userdata", "statev2.sqlite"), { readOnly: true });
  assert.equal(db2.prepare("SELECT count(*) AS n FROM orchestration_v2_projection_threads").get().n, 6, "no duplicate threads");
  db2.close();
  log("idempotent re-run ok");

  if (process.env.T3_SRC) {
    const scanFile = p("scan.json");
    fs.writeFileSync(scanFile, tool("scan", ...common, "--json"));
    const script = path.join(process.env.T3_SRC, "apps", "server", "src", "paseo2t3-backfill.ts");
    fs.copyFileSync(path.join(repoRoot, "t3-history", "backfill.ts"), script);
    const backfill = (...extra) => {
      const out = execFileSync(process.env.T3_NODE || process.execPath, [script, "--db", path.join(t3Data, "userdata", "statev2.sqlite"), "--scan", scanFile, "--claude-home", p("claude"), "--codex-home", p("codex"), ...extra], { cwd: path.dirname(script), encoding: "utf8" });
      return JSON.parse(out.slice(out.indexOf("{")));
    };
    try {
      const applied = backfill("--apply");
      assert.equal(applied.history, 2, `history for the native Claude and Codex sessions: ${JSON.stringify(applied)}`);
      assert.equal(applied.nested, 2, `both sub-agents nested: ${JSON.stringify(applied)}`);
      assert.equal(backfill().threads, 0, "a second backfill has nothing to do");
    } finally {
      fs.rmSync(script, { force: true });
    }
    // The running server serves both without a restart.
    const claudeThread = JSON.parse(t3("thread", "show", refs.get(IDS.claudeNative).threadId, "--base-dir", t3Data, "--messages", "10", "--json"));
    const texts = JSON.stringify(claudeThread);
    assert.ok(texts.includes("Refactor the parser") && texts.includes("Done."), `history shown: ${texts.slice(0, 500)}`);
    const child = await rpc(origin, token, "orchestration.getThreadProjection", { threadId: refs.get(IDS.codexWorktree).threadId });
    assert.deepEqual(child.thread.lineage, { parentThreadId: unlinked, relationshipToParent: "subagent", rootThreadId: unlinked });
    log("history backfill and sub-agent nesting ok");
  }

  log("PASS");
} catch (e) {
  console.error(e);
  // CI logs are public: drop the pairing QR code and credentials before printing.
  const safeLog = serverLog
    .split("\n")
    .filter((l) => !/[█▀▄]/.test(l) && !/token|pairing|app link|connection string/i.test(l))
    .join("\n");
  if (safeLog) console.error(`--- t3 server output (tail, credentials removed) ---\n${safeLog.slice(-4000)}`);
  process.exitCode = 1;
} finally {
  // Stop only the server this run started, by the pid it published for our base dir.
  if (serverPid) {
    try {
      process.kill(serverPid);
    } catch {}
  }
  if (process.env.E2E_KEEP === "1") log(`kept ${tmp}`);
  else fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 5, retryDelay: 500 });
  // Without this, open pipes from the killed server can keep Node alive.
  setTimeout(() => process.exit(process.exitCode ?? 0), 1000).unref();
}
