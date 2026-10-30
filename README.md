# paseo2t3

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

**Known T3 quirk (0.1.5 and main as of 2026-10-30):** the first message in an imported
Claude thread fails with "Provider turn failed". The Claude adapter only passes `resume` once
the thread has a recorded provider turn, so the first open asks Claude Code to create a new
session under the existing id, which it refuses. That failed attempt records a turn; send the
message again and the thread resumes the original session with full history (verified with a
codeword planted in a throwaway session). Codex threads are not affected by this check.

Not carried over: Paseo's in-memory timeline (Paseo never stores one), schedules, voice
settings. Imported threads open **empty in T3's UI**: the history stays with the provider,
which has full context when you resume. Providers T3 doesn't support here (Copilot, Pi,
Antigravity, ...) are listed as skipped.

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
  are reported as already present, and T3 deduplicates repeated commands.
- `--map FROM=TO` rewrites path prefixes when sessions were recorded under another home.
- Project roots must exist on the machine running the import (`--no-path-check` to skip).
- Claude sessions are placed at their exact original cwd, because Claude Code looks a
  session up by cwd when resuming.

## Trial

`trial/make-fixture.js` writes a Paseo-shaped home covering each case (worktree, archived,
legacy layout, unsupported provider, never-started, internal, corrupt file, missing dir).
Run T3 against a scratch home (`t3 serve --base-dir <scratch>`), never your real `~/.t3`.
