# paseo2t3

[![CI](https://github.com/mbdalpha/paseo2t3/actions/workflows/ci.yml/badge.svg)](https://github.com/mbdalpha/paseo2t3/actions/workflows/ci.yml)
[![Nightly](https://github.com/mbdalpha/paseo2t3/actions/workflows/nightly.yml/badge.svg)](https://github.com/mbdalpha/paseo2t3/releases/tag/nightly)

Ports Paseo agents into T3 Code. Each agent becomes a T3 thread bound to the agent's
provider-native session (Claude Code session id, Codex thread id, OpenCode session id), so
the next message sent in T3 resumes the original conversation.

It can also import native Claude Code and Codex sessions directly (`--source claude,codex`),
which matters when Paseo never persisted agent records: Paseo 0.11.x only writes
`~/.paseo/agents/` once an agent exists, and the agents themselves are thin pointers to
those native sessions anyway.

Built against Paseo 0.11.3 and T3 Code 0.1.5.

## What carries over

| Paseo                          | T3 Code                                       |
|--------------------------------|-----------------------------------------------|
| project root / workspace       | project (reused if the root already exists)   |
| agent                          | thread with an imported native session ref    |
| title, model                   | thread title, model selection                 |
| worktree workspace + branch    | thread `worktreePath` + `branch`              |
| archived agent                 | archived thread (with `--include-archived`)   |
| archived sub-agent of a live agent | archived thread, always                   |
| conversation so far            | thread messages (`t3-history/backfill.ts`)    |
| sub-agent (`paseo.parent-agent-id`) | thread nested under its parent (`t3-history/backfill.ts`) |

**Claude threads are linked on import.** T3 Code (0.1.5 and main as of 2026-10-31) passes
`resume` to Claude Code only once a thread has a recorded provider turn. Left alone, the first
message in an imported Claude thread asks Claude Code to create a new session under the
existing id, which it refuses, so the message fails with "Provider turn failed". Because the
thread has no messages yet, T3 also retitles it from that message, so it loses its Paseo title.
To avoid both, the import sends each Claude thread one short `[paseo2t3]` note and waits for
that turn to fail. The failure records a provider turn, and the user's first message then
resumes the original session with full history (verified with a codeword planted in a
throwaway session). Claude Code rejects the note before any model call; T3 passes it to the
next turn as earlier context. Codex threads are not affected and are not linked. Tracked
upstream as [t3code#15243](https://github.com/pingdotgg/t3code/issues/15243), with a fix in
[t3code#23219](https://github.com/pingdotgg/t3code/pull/23219).

A thread is linked only when its transcript is where Claude Code looks it up
(`<claude home>/projects/<cwd with non-alphanumerics as '-'>/<id>.jsonl`); otherwise the
first turn would start a new session and reach the model, so the import prints
`not linked` instead. If T3 ever resumes on the first turn, the import notices, says so and
stops linking. `--no-link` turns linking off.

Not carried over: Paseo's in-memory timeline (Paseo never stores one), schedules, voice
settings. `import` alone leaves threads **empty in T3's UI** (the provider still has the full
context when you resume); run the history backfill below to show the conversation and nest
sub-agents. Providers T3 doesn't support here (Copilot, Pi, Antigravity, ...) are listed as
skipped.

## Install

Nightly builds for Linux (x86_64/aarch64, static musl), macOS (arm64/x86_64) and Windows
are on the [`nightly` release](https://github.com/mbdalpha/paseo2t3/releases/tag/nightly),
with `SHA256SUMS`. A nightly is only published after the E2E suite passes on all three OSes.
Or build it yourself: `cargo install --locked --git https://github.com/mbdalpha/paseo2t3`.

## Usage

```sh
# 1. See what would be ported (reads only).
paseo2t3 scan                                  # Paseo agents from ~/.paseo
paseo2t3 scan --source paseo,claude,codex      # plus native sessions
paseo2t3 scan --source claude,codex --since 2026-09-01 --cwd-prefix /home/dev/projects

# 2. Get a token for the T3 server that should receive the threads.
t3 auth session issue --token-only --ttl 1h --label paseo2t3 > /tmp/t3-token

# 3. Dry run, then import.
paseo2t3 import --t3-home ~/.t3 --token-file /tmp/t3-token
paseo2t3 import --t3-home ~/.t3 --token-file /tmp/t3-token --apply
```

- There is no default T3 target; pass `--t3-home` (origin read from
  `userdata/server-runtime.json`) or `--t3-url`.
- The T3 server must be running; the tool only uses its public API (`/api/projects/mutate`
  and the `orchestration.dispatchCommand` WebSocket RPC), never its database.
- Re-running is safe: ids are derived from `(provider, native session id)`, existing threads
  are reported as already present, and T3 deduplicates repeated commands. A re-run also
  links existing Claude threads that have no provider turn yet, and puts back the imported
  title on threads T3 retitled from a first message that failed (no turn completed).
- `--map FROM=TO` rewrites path prefixes when sessions were recorded under another home.
- Project roots must exist on the machine running the import (`--no-path-check` to skip).
- Claude sessions are placed at their exact original cwd, because Claude Code looks a
  session up by cwd when resuming.

## History and sub-agents

T3's public API can create a thread but not give it past messages or a parent. T3's own
session importer, which does write history, skips any session another thread already owns.
`t3-history/backfill.ts` therefore writes those events itself, through T3's own event store,
projection code and transcript parser, so the result is what T3's importer would write:

- each thread gets the user and assistant messages of its Claude Code or Codex transcript
  (the first message plus the latest 199), ahead of anything sent in T3;
- each Paseo sub-agent's thread becomes a sub-agent of its parent's thread, so it shows
  under the parent (T3's relationships menu) instead of in the sidebar. It is still a normal
  thread you can open and message.

It runs from a T3 Code source checkout of the same version as the server, with Node 26:

```sh
git clone --depth 1 --branch v0.1.5 https://github.com/pingdotgg/t3code
(cd t3code && corepack pnpm install --filter 't3...' --frozen-lockfile --ignore-scripts)
cp t3-history/backfill.ts t3code/apps/server/src/paseo2t3-backfill.ts

paseo2t3 scan --source paseo,claude,codex --json > scan.json    # same sources as the import
sqlite3 ~/.t3/userdata/statev2.sqlite ".backup statev2.before-backfill.sqlite"
cd t3code/apps/server/src
node paseo2t3-backfill.ts --db ~/.t3/userdata/statev2.sqlite --scan /path/to/scan.json
node paseo2t3-backfill.ts --db ~/.t3/userdata/statev2.sqlite --scan /path/to/scan.json --apply
```

- It writes to T3's database directly, so take the backup first. It works while the server
  runs: each thread is one transaction, retried if the server holds the write lock. Reload
  the T3 page afterwards so the sidebar picks up the nesting.
- Re-running is safe: event ids are derived from the thread, and threads already done are
  skipped.
- Transcripts are streamed and reduced to the fields T3's parser reads, so multi-hundred-MB
  Codex rollouts are fine.

## Testing

```sh
cargo test                      # parsers and helpers
cargo build --release
cd e2e && npm ci && npm test    # end to end against a real T3 Code server
```

The E2E run builds fixture Paseo, Claude Code and Codex stores (worktree, archived, legacy
layout, unsupported provider, never-started, internal, corrupt file, missing directory,
sub-agent thread), starts `t3 serve` on a temporary data directory and a free port, imports
everything, checks what T3 persisted (native session refs, archive state, worktree and branch,
titles, project count), then re-imports to prove nothing is duplicated. It never touches
`~/.t3`: the T3 runtime is cached in `e2e/.cache` (override with `T3_RUNTIME_HOME`). T3 runs
Claude threads through `e2e/fake-claude.mjs`, which logs its arguments and fails every turn, so
the run checks that the link turn asks for `--session-id`, that the next message asks for
`--resume`, and that a re-import restores a title T3 replaced. With `T3_SRC` (a prepared T3
source checkout) and `T3_NODE` (Node 26) set, it also runs the history backfill against the
running server and checks the messages and the sub-agent nesting. It needs Node 22.13+ and
git, and makes no model calls.
