// Backfills what T3 Code's public API cannot set on threads paseo2t3 imported:
//
//  - the conversation so far, as the user and assistant messages T3's own session importer
//    writes (first message plus the latest 199, parsed by T3's transcript parser), and
//  - Paseo's sub-agent tree, as T3 lineage, so a sub-agent's thread shows under its parent.
//
// It writes events through T3's own event store and projection code, so it must run from
// apps/server/src of a T3 Code source checkout matching the server version (see README).
// Dry run unless --apply. Safe to re-run: every event id is derived from the thread.
//
//   node paseo2t3-backfill.ts --db <t3 home>/userdata/statev2.sqlite --scan scan.json [--apply]
//
// scan.json is `paseo2t3 scan --json` with the same --source and filters used for the import.
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";
import util from "node:util";
import { parseArgs } from "node:util";

import { EventId, MessageId, ProviderInstanceId, ThreadId, TurnItemId } from "@t3tools/contracts";
import * as NodeSqliteClient from "@t3tools/shared/nodeSqliteClient";
import * as DateTime from "effect/DateTime";
import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Schedule from "effect/Schedule";
import * as SqlClient from "effect/sql/SqlClient";

import * as EventStore from "./orchestration-v2/EventStore.ts";
import * as ProjectionStore from "./orchestration-v2/ProjectionStore.ts";
import { parseAgentSessionTranscript } from "./project/AgentSessionScanner.ts";

const { values: args } = parseArgs({
  options: {
    db: { type: "string" },
    scan: { type: "string" },
    "claude-home": { type: "string", default: process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), ".claude") },
    "codex-home": { type: "string", default: process.env.CODEX_HOME || path.join(os.homedir(), ".codex") },
    apply: { type: "boolean", default: false },
  },
});
if (!args.db || !args.scan) throw new Error("usage: --db <statev2.sqlite> --scan <scan.json> [--apply]");

interface ScanSession {
  driver: string;
  native_id: string;
  cwd: string;
  archived: boolean;
  agent_id?: string;
  parent_agent_id?: string;
}
const sessions: ScanSession[] = JSON.parse(fs.readFileSync(args.scan, "utf8")).sessions;

/** Same ids as paseo2t3's `stable_id`. */
function stableId(kind: string, key: string): string {
  const b = crypto.createHash("sha256").update(`paseo2t3\0${kind}\0${key}`).digest().subarray(0, 16);
  b[6] = (b[6]! & 0x0f) | 0x50;
  b[8] = (b[8]! & 0x3f) | 0x80;
  const x = b.toString("hex");
  return `${x.slice(0, 8)}-${x.slice(8, 12)}-${x.slice(12, 16)}-${x.slice(16, 20)}-${x.slice(20)}`;
}
const threadIdOf = (s: ScanSession) => stableId("thread", `${s.driver}:${s.native_id}`);

// Claude Code: projects/<cwd with non-alphanumerics as '-'>/<id>.jsonl (long names get a hash suffix).
function claudeTranscript(cwd: string, id: string): string | null {
  const projects = path.join(args["claude-home"]!, "projects");
  const dir = cwd.replace(/[^a-zA-Z0-9]/g, "-");
  const direct = path.join(projects, dir, `${id}.jsonl`);
  if (fs.existsSync(direct)) return direct;
  if (dir.length <= 200 || !fs.existsSync(projects)) return null;
  for (const entry of fs.readdirSync(projects)) {
    const file = path.join(projects, entry, `${id}.jsonl`);
    if (entry.startsWith(dir.slice(0, 200)) && fs.existsSync(file)) return file;
  }
  return null;
}

// Codex: sessions/YYYY/MM/DD/rollout-<timestamp>-<id>.jsonl, or archived_sessions/.
const codexRollouts = new Map<string, string>();
for (const sub of ["sessions", "archived_sessions"]) {
  const walk = (dir: string) => {
    if (!fs.existsSync(dir)) return;
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (entry.name.startsWith("rollout-") && entry.name.endsWith(".jsonl")) {
        codexRollouts.set(entry.name.slice(-"00000000-0000-0000-0000-000000000000.jsonl".length, -6), full);
      }
    }
  };
  walk(path.join(args["codex-home"]!, sub));
}

const transcriptOf = (s: ScanSession): string | null =>
  s.driver === "claudeAgent" ? claudeTranscript(s.cwd, s.native_id) : s.driver === "codex" ? (codexRollouts.get(s.native_id) ?? null) : null;

const isText = (b: any) => b && typeof b === "object" && ["text", "input_text", "output_text"].includes(b.type);
const textOnly = (content: any) => (Array.isArray(content) ? content.filter(isText) : content);

/**
 * The transcript reduced to the records and fields T3's parser reads. Rollouts with
 * screenshots and tool output reach hundreds of MB, more than one string can hold.
 * Messages from `cutoffMs` on are dropped: the session was continued in T3 by then, and
 * T3 already shows those turns.
 */
async function slimTranscript(file: string, source: string, cutoffMs: number): Promise<string> {
  const out: string[] = [];
  const lines = readline.createInterface({ input: fs.createReadStream(file), crlfDelay: Infinity });
  for await (const line of lines) {
    if (!line.trim()) continue;
    let r: any;
    try {
      r = JSON.parse(line);
    } catch {
      continue;
    }
    if (typeof r.timestamp === "string" && Date.parse(r.timestamp) >= cutoffMs) continue;
    if (source === "claudeAgent") {
      if (r.type === "user" || r.type === "assistant") {
        const { type, isSidechain, isMeta, isCompactSummary, timestamp, sessionId, cwd } = r;
        const m = r.message ?? {};
        const content = textOnly(m.content);
        if (typeof content !== "string" && !(content?.length > 0) && !m.model) continue;
        out.push(JSON.stringify({ type, isSidechain, isMeta, isCompactSummary, timestamp, sessionId, cwd, message: { role: m.role, model: m.model, content } }));
      } else if (r.aiTitle !== undefined || r.customTitle !== undefined) out.push(line);
      continue;
    }
    const p = r.payload ?? {};
    if (r.type === "session_meta") {
      const { id, session_id, cwd, source: origin, parent_thread_id } = p;
      out.push(JSON.stringify({ type: r.type, timestamp: r.timestamp, payload: { id, session_id, cwd, source: origin, parent_thread_id } }));
    } else if (r.type === "turn_context") {
      out.push(JSON.stringify({ type: r.type, timestamp: r.timestamp, payload: { model: p.model, cwd: p.cwd } }));
    } else if (r.type === "event_msg" && p.type === "user_message") {
      out.push(JSON.stringify({ type: r.type, timestamp: r.timestamp, payload: { type: p.type, message: p.message } }));
    } else if (r.type === "response_item" && p.type === "message") {
      const { role, internal_chat_message_metadata_passthrough } = p;
      out.push(JSON.stringify({ type: r.type, timestamp: r.timestamp, payload: { type: p.type, role, internal_chat_message_metadata_passthrough, content: textOnly(p.content) } }));
    }
  }
  return out.join("\n");
}

// Paseo's tree: an agent's parent is the agent named by its paseo.parent-agent-id label.
const byAgent = new Map(sessions.filter((s) => s.agent_id).map((s) => [s.agent_id!, s]));
const parentOf = (s: ScanSession) => (s.parent_agent_id ? byAgent.get(s.parent_agent_id) : undefined);

const EVENT_PREFIX = "paseo2t3-backfill:v1";

const program = Effect.gen(function* () {
  const sql = yield* SqlClient.SqlClient;
  const eventStore = yield* EventStore.EventStoreV2;
  const projections = yield* ProjectionStore.ProjectionStoreV2;

  const existing = new Map(
    (yield* sql<{ thread_id: string; deleted_at: string | null }>`
      SELECT thread_id, deleted_at FROM orchestration_v2_projection_threads
    `).map((r) => [r.thread_id, r.deleted_at === null]),
  );
  const live = (id: string) => existing.get(id) === true;
  // The topmost ancestor that is a T3 thread, for lineage.rootThreadId.
  const rootOf = (s: ScanSession): string => {
    const seen = new Set<ScanSession>();
    let top = s;
    for (let p = parentOf(top); p && !seen.has(p) && live(threadIdOf(p)); p = parentOf(top)) {
      seen.add(top);
      top = p;
    }
    return threadIdOf(top);
  };

  const totals = { threads: 0, history: 0, messages: 0, nested: 0, noTranscript: 0, unparsable: 0, failed: 0, missing: 0 };
  for (const s of sessions) {
    const threadId = ThreadId.make(threadIdOf(s));
    if (!live(threadId)) {
      totals.missing += 1;
      continue;
    }
    const { thread } = yield* projections.getThreadRecords(threadId, []);
    const events: Array<any> = [];

    const firstEventId = `${EVENT_PREFIX}:message:${threadId}:000000`;
    const done = yield* sql`SELECT 1 FROM orchestration_events WHERE event_id = ${firstEventId} LIMIT 1`;
    const file = done.length > 0 ? null : transcriptOf(s);
    if (done.length === 0 && file === null) totals.noTranscript += 1;
    if (file !== null) {
      // Where the thread's own T3 history starts.
      const [first] = yield* sql<{ at: string | null }>`
        SELECT min(at) AS at FROM (
          SELECT min(created_at) AS at FROM orchestration_v2_projection_messages
          WHERE thread_id = ${threadId} AND message_id NOT LIKE ${`${threadId}:paseo2t3:%`}
          UNION ALL
          SELECT min(requested_at) FROM orchestration_v2_projection_runs WHERE thread_id = ${threadId}
        )
      `;
      const cutoffMs = first?.at ? Date.parse(first.at) : Infinity;
      const history = yield* Effect.promise(() => readHistory(threadId, s, thread.providerInstanceId, file, cutoffMs));
      if (history.kind === "failed") {
        totals.failed += 1;
        console.error(`! ${threadId} ${file}: ${history.error}`);
      } else if (history.kind === "unparsable") totals.unparsable += 1;
      else {
        events.push(...history.events);
        if (history.messages > 0) totals.history += 1;
        totals.messages += history.messages;
      }
    }

    const parent = parentOf(s);
    if (parent && live(threadIdOf(parent)) && thread.lineage.parentThreadId === null) {
      events.push({
        id: EventId.make(`${EVENT_PREFIX}:lineage:${threadId}`),
        type: "thread.metadata-updated",
        threadId,
        providerInstanceId: thread.providerInstanceId,
        occurredAt: yield* DateTime.now,
        payload: {
          ...thread,
          lineage: {
            parentThreadId: ThreadId.make(threadIdOf(parent)),
            relationshipToParent: "subagent",
            rootThreadId: ThreadId.make(rootOf(s)),
          },
        },
      });
      totals.nested += 1;
    }

    if (events.length === 0) continue;
    totals.threads += 1;
    if (!args.apply) continue;
    yield* sql.withTransaction(
      Effect.gen(function* () {
        const stored = yield* eventStore.append({ events });
        // What EventSink does after an append and ProjectionMaintenance does on rebuild.
        for (const { event } of stored) {
          yield* projections.apply(event);
          if (event.type === "turn-item.updated") {
            yield* sql`
              INSERT INTO orchestration_v2_turn_item_positions (thread_id, turn_item_id, ordinal)
              VALUES (${event.threadId}, ${event.payload.id}, ${event.payload.ordinal})
              ON CONFLICT(thread_id, turn_item_id) DO UPDATE SET ordinal = excluded.ordinal
            `;
          }
        }
        yield* sql`
          UPDATE orchestration_v2_projection_metadata
          SET last_sequence = ${stored.at(-1)!.sequence}, updated_at = ${new Date().toISOString()}
          WHERE projection_name = 'thread-projections' AND last_sequence < ${stored.at(-1)!.sequence}
        `;
      }),
    ).pipe(
      // A running T3 server writes too; SQLite refuses a write that would race its commit.
      Effect.retry({ while: isLocked, schedule: Schedule.spaced("250 millis"), times: 40 }),
    );
  }
  console.log(JSON.stringify({ apply: args.apply, ...totals }, null, 2));
  if (totals.failed > 0) process.exitCode = 1;
});

/** One thread's history as events. Never rejects, so one bad transcript skips one thread. */
async function readHistory(threadId: ThreadId, s: ScanSession, providerInstanceId: ProviderInstanceId, file: string, cutoffMs: number) {
  try {
    const parsed = parseAgentSessionTranscript({
      source: s.driver as "claudeAgent" | "codex",
      providerInstanceId,
      fallbackSessionId: s.native_id,
      lastActiveAtMs: Math.min(fs.statSync(file).mtimeMs, cutoffMs),
      contents: await slimTranscript(file, s.driver, cutoffMs),
    });
    if (parsed === null) return { kind: "unparsable" } as const;
    const events = parsed.messages.flatMap((message, index) => messageEvents(threadId, index, message));
    return { kind: "events", events, messages: parsed.messages.length } as const;
  } catch (error) {
    return { kind: "failed", error } as const;
  }
}

/** SQLITE_BUSY, however deep T3's error types wrap it. */
const isLocked = (e: unknown) => util.inspect(e, { depth: 12 }).includes("database is locked");

/** The events T3's AgentSessionImporter writes per message, with this tool's event ids. */
function messageEvents(threadId: ThreadId, index: number, message: { role: "user" | "assistant"; text: string; createdAt: string }) {
  const suffix = String(index).padStart(6, "0");
  const messageId = MessageId.make(`${threadId}:paseo2t3:${suffix}`);
  const turnItemId = TurnItemId.make(`${EVENT_PREFIX}:turn-item:${threadId}:${suffix}`);
  const at = DateTime.makeUnsafe(message.createdAt);
  const common = {
    id: turnItemId,
    threadId,
    runId: null,
    nodeId: null,
    providerThreadId: null,
    providerTurnId: null,
    nativeItemRef: null,
    parentItemId: null,
    ordinal: index + 1,
    status: "completed" as const,
    title: null,
    startedAt: at,
    completedAt: at,
    updatedAt: at,
  };
  return [
    {
      id: EventId.make(`${EVENT_PREFIX}:message:${threadId}:${suffix}`),
      type: "message.updated",
      threadId,
      occurredAt: at,
      payload: {
        createdBy: message.role === "user" ? "user" : "agent",
        creationSource: "server",
        id: messageId,
        threadId,
        runId: null,
        nodeId: null,
        role: message.role,
        text: message.text,
        attachments: [],
        streaming: false,
        createdAt: at,
        updatedAt: at,
      },
    },
    {
      id: EventId.make(`${EVENT_PREFIX}:turn-item:${threadId}:${suffix}`),
      type: "turn-item.updated",
      threadId,
      occurredAt: at,
      payload:
        message.role === "user"
          ? { ...common, createdBy: "user", creationSource: "server", type: "user_message", messageId, inputIntent: "turn_start", text: message.text, attachments: [] }
          : { ...common, type: "assistant_message", messageId, text: message.text, streaming: false },
    },
  ];
}

const layer = Layer.mergeAll(EventStore.layer, ProjectionStore.layer).pipe(
  Layer.provideMerge(NodeSqliteClient.layer({ filename: args.db })),
);
await Effect.runPromise(program.pipe(Effect.provide(layer)));
