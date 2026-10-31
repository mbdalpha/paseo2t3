mod sources;
mod t3;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sources::{Scan, Session};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Port Paseo agents (and, optionally, native Claude Code / Codex sessions) into T3 Code.
///
/// Each session becomes a T3 thread bound to the provider's native session id, so the
/// next message in T3 resumes the original conversation. T3 does not backfill the old
/// transcript into its UI; the history lives with the provider.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List what would be ported, without contacting T3.
    Scan {
        #[command(flatten)]
        sources: SourceArgs,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Create T3 projects and threads. Dry run unless --apply is given.
    Import {
        #[command(flatten)]
        sources: SourceArgs,
        #[command(flatten)]
        target: TargetArgs,
        /// Actually write to T3 (otherwise only print the plan).
        #[arg(long)]
        apply: bool,
        /// Runtime mode for imported threads.
        #[arg(long, default_value = "full-access")]
        runtime_mode: String,
        /// Don't require project roots to exist on this machine.
        #[arg(long)]
        no_path_check: bool,
        /// Don't send the throwaway first turn that lets T3 resume imported Claude threads
        /// on the user's first message (see README).
        #[arg(long)]
        no_link: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
enum Source {
    Paseo,
    Claude,
    Codex,
}

#[derive(Args)]
struct SourceArgs {
    /// Stores to read, comma-separated.
    #[arg(long, value_enum, value_delimiter = ',', default_value = "paseo")]
    source: Vec<Source>,
    #[arg(long, env = "PASEO_HOME")]
    paseo_home: Option<PathBuf>,
    #[arg(long, env = "CLAUDE_CONFIG_DIR")]
    claude_home: Option<PathBuf>,
    #[arg(long, env = "CODEX_HOME")]
    codex_home: Option<PathBuf>,
    /// Include archived agents/sessions (they are imported, then archived in T3).
    #[arg(long)]
    include_archived: bool,
    /// Only sessions active on or after this date (YYYY-MM-DD or full ISO timestamp).
    #[arg(long)]
    since: Option<String>,
    /// Only sessions whose working directory starts with this path.
    #[arg(long)]
    cwd_prefix: Option<String>,
    /// Rewrite path prefixes, e.g. --map /home/dev=/home/alice. Repeatable.
    #[arg(long = "map", value_name = "FROM=TO")]
    maps: Vec<String>,
}

#[derive(Args)]
struct TargetArgs {
    /// T3 server origin, e.g. http://127.0.0.1:3773.
    #[arg(long, conflicts_with = "t3_home")]
    t3_url: Option<String>,
    /// T3 home of a running server; its origin is read from userdata/server-runtime.json.
    #[arg(long)]
    t3_home: Option<PathBuf>,
    /// File holding a bearer token from `t3 auth session issue --token-only`. Takes
    /// precedence over T3_TOKEN.
    #[arg(long)]
    token_file: Option<PathBuf>,
    #[arg(long, env = "T3_TOKEN", hide_env_values = true)]
    token: Option<String>,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Scan { sources, json } => {
            let scan = collect(&sources, false)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "sessions": scan.sessions, "skipped": scan.skipped }))?
                );
            } else {
                print_plan(&scan);
            }
            Ok(())
        }
        Command::Import { sources, target, apply, runtime_mode, no_path_check, no_link } => {
            let scan = collect(&sources, !no_path_check)?;
            print_plan(&scan);
            if !apply {
                println!("\nDry run: nothing was written. Re-run with --apply to import.");
                return Ok(());
            }
            let claude_home = sources.claude_home.clone().unwrap_or_else(|| home().join(".claude"));
            import(&scan.sessions, &target, &runtime_mode, (!no_link).then_some(claude_home.as_path()))
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn collect(args: &SourceArgs, require_roots: bool) -> Result<Scan> {
    let mut scan = Scan::default();
    let wanted: HashSet<Source> = args.source.iter().copied().collect();
    // Paseo first: its records carry titles and workspace info, and win de-duplication.
    if wanted.contains(&Source::Paseo) {
        sources::scan_paseo(&args.paseo_home.clone().unwrap_or_else(|| home().join(".paseo")), &mut scan)?;
    }
    if wanted.contains(&Source::Claude) {
        sources::scan_claude(&args.claude_home.clone().unwrap_or_else(|| home().join(".claude")), &mut scan)?;
    }
    if wanted.contains(&Source::Codex) {
        sources::scan_codex(&args.codex_home.clone().unwrap_or_else(|| home().join(".codex")), &mut scan)?;
    }

    let maps: Vec<(String, String)> = args
        .maps
        .iter()
        .map(|m| m.split_once('=').map(|(a, b)| (a.to_string(), b.to_string())).context("--map expects FROM=TO"))
        .collect::<Result<_>>()?;
    let remap = |p: &mut String| {
        if let Some((from, to)) = maps.iter().find(|(from, _)| p.starts_with(from.as_str())) {
            *p = format!("{to}{}", &p[from.len()..]);
        }
    };

    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    for mut s in std::mem::take(&mut scan.sessions) {
        if !seen.insert((s.driver.clone(), s.native_id.clone())) {
            continue;
        }
        if let Some(since) = &args.since {
            if s.updated_at.as_deref().is_none_or(|t| t < since.as_str()) {
                continue;
            }
        }
        remap(&mut s.cwd);
        for p in [&mut s.project_root, &mut s.worktree_path].into_iter().flatten() {
            remap(p);
        }
        if let Some(prefix) = &args.cwd_prefix {
            if !s.cwd.starts_with(prefix.as_str()) {
                continue;
            }
        }
        sources::resolve_project_root(&mut s);
        if require_roots && !s.project_root.as_deref().is_some_and(|r| Path::new(r).is_dir()) {
            let reason = format!("project root {} does not exist here", s.project_root.unwrap_or_default());
            scan.skipped.push(sources::Skipped { origin: s.origin, reason });
            continue;
        }
        kept.push(s);
    }
    // An archived sub-agent of a live agent that is being imported is kept (and archived in
    // T3), so the live agent's thread lists all of its sub-agents. Decided after every other
    // filter, so a parent filtered out above does not pull its archived children in.
    let live_agents: HashSet<String> = kept.iter().filter(|s| !s.archived).filter_map(|s| s.agent_id.clone()).collect();
    for s in kept {
        let live_parent = s.parent_agent_id.as_ref().is_some_and(|p| live_agents.contains(p));
        if s.archived && !args.include_archived && !live_parent {
            scan.skipped
                .push(sources::Skipped { origin: s.origin, reason: "archived (use --include-archived)".into() });
            continue;
        }
        scan.sessions.push(s);
    }
    scan.sessions.sort_by(|a, b| (&a.project_root, &b.updated_at).cmp(&(&b.project_root, &a.updated_at)));
    Ok(scan)
}

fn print_plan(scan: &Scan) {
    let mut by_root: BTreeMap<&str, Vec<&Session>> = BTreeMap::new();
    for s in &scan.sessions {
        by_root.entry(s.project_root.as_deref().unwrap_or("?")).or_default().push(s);
    }
    println!("{} session(s) in {} project(s); {} skipped", scan.sessions.len(), by_root.len(), scan.skipped.len());
    for (root, sessions) in &by_root {
        println!("\n{root}");
        for s in sessions {
            let date = s.updated_at.as_deref().map(|t| &t[..t.len().min(10)]).unwrap_or("?");
            let arch = if s.archived { " [archived]" } else { "" };
            let wt = s.worktree_path.as_deref().map(|w| format!(" (worktree {w})")).unwrap_or_default();
            println!("  {date}  {:<11} {}  {}{arch}{wt}", s.driver, short(&s.native_id), title_for(s));
        }
    }
    let mut reasons: BTreeMap<&str, usize> = BTreeMap::new();
    for k in &scan.skipped {
        *reasons.entry(k.reason.split(": ").next().unwrap_or("")).or_default() += 1;
    }
    if !reasons.is_empty() {
        println!("\nSkipped:");
        for (reason, n) in reasons {
            println!("  {n:>4}  {reason}");
        }
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

fn title_for(s: &Session) -> String {
    let raw = s.title.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let line = raw.and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty())).unwrap_or("");
    if line.is_empty() {
        let tool = if s.driver == "claudeAgent" { "Claude" } else { s.driver.as_str() };
        return format!("Imported {tool} session {}", short(&s.native_id));
    }
    let mut out: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        out.push('…');
    }
    out
}

/// Deterministic UUID-shaped id, so re-running the import reuses ids and T3's
/// command receipts make each command idempotent.
fn stable_id(kind: &str, key: &str) -> String {
    let h = Sha256::digest(format!("paseo2t3\0{kind}\0{key}").as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&h[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x: String = b.iter().map(|v| format!("{v:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &x[..8], &x[8..12], &x[12..16], &x[16..20], &x[20..])
}

fn norm_root(p: &str) -> String {
    let t = p.trim_end_matches(['/', '\\']);
    if cfg!(windows) {
        t.replace('/', "\\").to_lowercase()
    } else {
        t.to_string()
    }
}

fn default_model(driver: &str) -> &'static str {
    // T3 Code 0.1.5 DEFAULT_MODEL_BY_PROVIDER.
    match driver {
        "claudeAgent" => "claude-opus-5-5",
        "opencode" => "openai/gpt-5",
        _ => "gpt-6-astra",
    }
}

fn import(sessions: &[Session], target: &TargetArgs, runtime_mode: &str, claude_home: Option<&Path>) -> Result<()> {
    let origin = match (&target.t3_url, &target.t3_home) {
        (Some(url), _) => url.clone(),
        (None, Some(home)) => t3::origin_from_home(home)?,
        (None, None) => bail!("name the T3 server with --t3-url or --t3-home (there is deliberately no default)"),
    };
    let token = match (&target.token_file, target.token.as_deref().filter(|t| !t.trim().is_empty())) {
        (Some(f), _) => std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?,
        (None, Some(t)) => t.to_string(),
        (None, None) => {
            bail!("pass --token-file or set T3_TOKEN (create one with `t3 auth session issue --token-only`)")
        }
    };
    let mut t3 = t3::T3::new(&origin, &token);
    println!("\nImporting into {origin}");

    let mut projects: HashMap<String, String> =
        t3.projects()?.into_iter().map(|p| (norm_root(&p.workspace_root), p.id)).collect();

    let (mut created, mut existing, mut failed) = (0usize, 0usize, 0usize);
    let mut links = Vec::new();
    for s in sessions {
        let root = s.project_root.clone().unwrap_or_else(|| s.cwd.clone());
        let key = norm_root(&root);
        let project_id = match projects.get(&key) {
            Some(id) => id.clone(),
            None => {
                let title = Path::new(&root).file_name().and_then(|n| n.to_str()).unwrap_or(&root).to_string();
                let pid = stable_id("project", &key);
                match t3.create_project(&stable_id("project.create", &key), &pid, &title, &root) {
                    Ok(p) => {
                        println!("  + project {title} ({root})");
                        projects.insert(key, p.id.clone());
                        p.id
                    }
                    Err(e) => {
                        println!("  ! project {root}: {e:#}");
                        failed += 1;
                        continue;
                    }
                }
            }
        };

        let thread_key = format!("{}:{}", s.driver, s.native_id);
        let thread_id = stable_id("thread", &thread_key);
        let title = title_for(s);
        let link_home = claude_home.filter(|_| s.driver == "claudeAgent");
        if let Ok(projection) = t3.rpc("orchestration.getThreadProjection", json!({ "threadId": thread_id })) {
            existing += 1;
            println!("  = thread {} {}  already imported", s.driver, short(&s.native_id));
            let thread = &projection["thread"];
            if !thread["deletedAt"].is_null() {
                continue;
            }
            if let Err(e) = restore_title(&mut t3, &projection, &thread_key) {
                println!("    ! title restore failed: {e:#}");
            }
            if let Some(home) = link_home.filter(|_| thread["archivedAt"].is_null()) {
                match LinkJob::new(&projection, s, home, &thread_id, &thread_key, false) {
                    Ok(job) => links.extend(job),
                    Err(e) => {
                        failed += 1;
                        println!("    ! not linked: {e:#}");
                    }
                }
            }
            continue;
        }
        let model_selection = json!({
            "instanceId": s.driver,
            "model": s.model.clone().unwrap_or_else(|| default_model(&s.driver).to_string()),
        });
        let mut metadata = json!({ "title": title, "modelSelection": model_selection });
        if let Some(t) = &s.updated_at {
            metadata["updatedAt"] = Value::String(t.clone());
        }
        let command = json!({
            "type": "thread.create",
            "createdBy": "user",
            "creationSource": "web",
            "commandId": stable_id("thread.create", &thread_key),
            "threadId": thread_id,
            "projectId": project_id,
            "title": title,
            "modelSelection": model_selection,
            "runtimeMode": runtime_mode,
            "interactionMode": "default",
            "branch": s.branch,
            "worktreePath": s.worktree_path,
            "importedNativeThread": {
                "ref": { "driver": s.driver, "nativeId": s.native_id, "strength": "strong" },
                "metadata": metadata,
            },
        });
        if let Err(e) = t3.dispatch(command) {
            failed += 1;
            println!("  ! thread {} {}: {e:#}", s.driver, short(&s.native_id));
            continue;
        }
        created += 1;
        println!("  + thread {} {}  {title}", s.driver, short(&s.native_id));
        // An archived thread should not start turns, so one that gets linked is archived
        // once its link turn settles.
        let mut job = None;
        if let Some(home) = link_home {
            let job_for = |p: Value| LinkJob::new(&p, s, home, &thread_id, &thread_key, s.archived);
            match t3.rpc("orchestration.getThreadProjection", json!({ "threadId": thread_id })).and_then(job_for) {
                Ok(j) => job = j,
                Err(e) => {
                    failed += 1;
                    println!("    ! not linked: {e:#}");
                }
            }
        }
        match job {
            Some(job) => links.push(job),
            None if s.archived => archive(&mut t3, &thread_id, &thread_key),
            None => {}
        }
    }
    failed += link_all(&mut t3, links);
    println!("\nDone: {created} imported, {existing} already present, {failed} failure(s).");
    if claude_home.is_none() && sessions.iter().any(|s| s.driver == "claudeAgent") {
        println!(
            "\nNote: Claude threads were not linked (--no-link). In T3 Code 0.1.5 the first message\n\
             in an unlinked imported Claude thread fails with \"Provider turn failed\"; send it\n\
             again and the thread resumes the original session."
        );
    }
    if failed > 0 {
        bail!("{failed} item(s) failed");
    }
    Ok(())
}

fn archive(t3: &mut t3::T3, thread_id: &str, thread_key: &str) {
    let archive = json!({
        "type": "thread.archive",
        "commandId": stable_id("thread.archive", thread_key),
        "threadId": thread_id,
    });
    if let Err(e) = t3.dispatch(archive) {
        println!("    ! archive {thread_id} failed: {e:#}");
    }
}

/// The note sent as the throwaway first turn of an imported Claude thread. Claude Code
/// rejects that turn before any model call, and T3 hands the note to the next turn as
/// earlier context, so it must read as information rather than a request.
fn link_note(native_id: &str) -> String {
    format!(
        "{LINK_NOTE_PREFIX} Imported Claude Code session {native_id}. Earlier messages are not shown \
         in T3; the session keeps them. T3 Code cannot resume a session on a thread's first \
         turn (t3code#15243), so this turn fails and the next message continues the session."
    )
}

const LINK_NOTE_PREFIX: &str = "[paseo2t3]";

/// How many link turns run at once, each starting its own Claude Code process.
const PARALLEL_LINKS: usize = 8;
const LINK_TIMEOUT: Duration = Duration::from_secs(120);

/// T3 Code 0.1.5 passes `resume` to Claude Code only once the thread has a recorded provider
/// turn (t3code#15243). Without one, the first message asks Claude Code to create a session
/// under the imported id, which it refuses, so the user's first message fails and, because
/// the thread has no messages yet, T3 also retitles the thread from it. Sending one note
/// first takes that failure here, before the user ever opens the thread.
struct LinkJob {
    thread_id: String,
    thread_key: String,
    native_id: String,
    /// Runs that existed before the link turn, to tell the link turn's run apart.
    runs_before: HashSet<String>,
    archive: bool,
}

enum LinkState {
    Pending,
    /// The turn failed as expected and left a provider turn behind.
    Linked,
    /// The turn reached the model: this T3 resumes on the first turn and needs no linking.
    Resumed(String),
    Failed(anyhow::Error),
    TimedOut,
}

impl LinkJob {
    /// The link to make for a thread, or None when it needs none or cannot be linked.
    fn new(
        projection: &Value,
        s: &Session,
        claude_home: &Path,
        thread_id: &str,
        thread_key: &str,
        archive: bool,
    ) -> Result<Option<LinkJob>> {
        if has_provider_turn(projection) {
            return Ok(None);
        }
        // T3 cannot start Claude Code in a directory that is gone (a deleted worktree), and
        // Claude Code could not resume the session there anyway.
        if !Path::new(&s.cwd).is_dir() {
            println!("    not linked: {} no longer exists", s.cwd);
            return Ok(None);
        }
        // Without the transcript where Claude Code looks it up, the "throwaway" turn would start
        // a new session under this id and reach the model instead of failing.
        let transcript = claude_transcript(claude_home, &s.cwd, &s.native_id);
        if !transcript.is_file() {
            println!("    not linked: no transcript at {}", transcript.display());
            return Ok(None);
        }
        if active_run(projection) {
            bail!("the thread has a run in progress; re-run the import once it finishes");
        }
        Ok(Some(LinkJob {
            thread_id: thread_id.to_string(),
            thread_key: thread_key.to_string(),
            native_id: s.native_id.clone(),
            runs_before: run_ids(projection).map(str::to_string).collect(),
            archive,
        }))
    }

    fn dispatch(&self, t3: &mut t3::T3) -> Result<()> {
        // T3 answers a repeated commandId from its receipt without running anything, so each
        // attempt needs its own; every attempt adds a run, which makes the run count one.
        let attempt = format!("{}\0{}", self.thread_key, self.runs_before.len());
        t3.dispatch(json!({
            "type": "message.dispatch",
            "commandId": stable_id("thread.link", &attempt),
            "threadId": self.thread_id,
            "messageId": stable_id("message.link", &attempt),
            "text": link_note(&self.native_id),
            "attachments": [],
            "createdBy": "user",
            "creationSource": "web",
            "dispatchMode": { "type": "start_immediately" },
        }))?;
        Ok(())
    }

    fn poll(&self, t3: &mut t3::T3) -> LinkState {
        match t3.rpc("orchestration.getThreadProjection", json!({ "threadId": self.thread_id })) {
            Ok(p) => link_state(&p, &self.runs_before),
            Err(e) => LinkState::Failed(e),
        }
    }
}

fn link_state(p: &Value, runs_before: &HashSet<String>) -> LinkState {
    let runs: Vec<&Value> = p["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["id"].as_str().is_some_and(|id| !runs_before.contains(id)))
        .collect();
    let Some(run) = runs.first() else { return LinkState::Pending };
    let run_id = run["id"].as_str().unwrap_or_default();
    // A reply means the model ran: decided as soon as one appears, so the run can be stopped.
    let replied =
        p["messages"].as_array().into_iter().flatten().any(|m| m["runId"] == run_id && m["role"] == "assistant");
    if replied || run["status"] == "completed" {
        return LinkState::Resumed(run_id.to_string());
    }
    if active_run(p) {
        LinkState::Pending
    } else if has_provider_turn(p) {
        LinkState::Linked
    } else {
        LinkState::Failed(anyhow!(
            "the first turn ended without reaching Claude Code (is the claude CLI installed for T3?)"
        ))
    }
}

/// Sends the link turns and waits for them to settle. The first goes alone, and the rest
/// only once one has linked: on a T3 that resumes on the first turn, the note reaches the
/// model, and that should happen to one session at most. Returns the number of failures.
fn link_all(t3: &mut t3::T3, jobs: Vec<LinkJob>) -> usize {
    let mut queue: std::collections::VecDeque<LinkJob> = jobs.into();
    let mut in_flight: Vec<(LinkJob, Instant)> = Vec::new();
    let (mut failed, mut proven, mut stop) = (0, false, None::<String>);
    loop {
        while stop.is_none() && in_flight.len() < if proven { PARALLEL_LINKS } else { 1 } {
            let Some(job) = queue.pop_front() else { break };
            match job.dispatch(t3) {
                Ok(()) => in_flight.push((job, Instant::now())),
                Err(e) => {
                    failed += 1;
                    println!("  ! link {} failed: {e:#}", short(&job.native_id));
                    settle(t3, &job);
                }
            }
        }
        if in_flight.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
        let mut i = 0;
        while i < in_flight.len() {
            let (job, started) = &in_flight[i];
            let state = match job.poll(t3) {
                LinkState::Pending if started.elapsed() > LINK_TIMEOUT => LinkState::TimedOut,
                state => state,
            };
            let short_id = short(&job.native_id).to_string();
            match state {
                LinkState::Pending => {
                    i += 1;
                    continue;
                }
                LinkState::Linked => {
                    proven = true;
                    println!("  linked {short_id}: the next message resumes the session");
                }
                LinkState::Resumed(run_id) => {
                    println!("  T3 resumed session {short_id} on its first turn, so this T3 needs no linking");
                    let interrupt = json!({
                        "type": "run.interrupt",
                        "commandId": stable_id("thread.link.interrupt", &run_id),
                        "threadId": job.thread_id,
                        "runId": run_id,
                    });
                    if let Err(e) = t3.dispatch(interrupt) {
                        println!("    ! stopping that turn failed: {e:#}");
                    }
                    stop.get_or_insert_with(|| "this T3 resumes on the first turn".into());
                }
                LinkState::Failed(e) => {
                    failed += 1;
                    println!("  ! link {short_id} failed: {e:#}");
                }
                LinkState::TimedOut => {
                    failed += 1;
                    println!("  ! link {short_id} failed: timed out waiting for the first turn to finish");
                    // Until one link works, a turn that runs this long may be one that reached the model.
                    if !proven {
                        stop.get_or_insert_with(|| "no link could be confirmed".into());
                    }
                }
            }
            let (job, _) = in_flight.swap_remove(i);
            settle(t3, &job);
        }
    }
    if let Some(reason) = stop {
        if !queue.is_empty() {
            println!("  not linking {} other thread(s): {reason}", queue.len());
        }
        for job in queue {
            settle(t3, &job);
        }
    }
    failed
}

fn settle(t3: &mut t3::T3, job: &LinkJob) {
    if job.archive {
        archive(t3, &job.thread_id, &job.thread_key);
    }
}

/// Where Claude Code keeps a session: `projects/<cwd with every non-alphanumeric character
/// replaced by '-'>/<id>.jsonl`. Claude Code replaces per UTF-16 code unit, so a character
/// outside the BMP becomes two dashes. It shortens directory names over 200 characters with
/// a hash suffix, so those are matched on the 200-character prefix.
fn claude_transcript(claude_home: &Path, cwd: &str, native_id: &str) -> PathBuf {
    let projects = claude_home.join("projects");
    let dir: String = cwd
        .encode_utf16()
        .map(|u| char::from_u32(u.into()).filter(char::is_ascii_alphanumeric).unwrap_or('-'))
        .collect();
    let file = format!("{native_id}.jsonl");
    if dir.len() <= 200 {
        return projects.join(dir).join(file);
    }
    std::fs::read_dir(&projects)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join(&file))
        .find(|p| {
            p.parent().and_then(Path::file_name).and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&dir[..200]))
                && p.is_file()
        })
        .unwrap_or_else(|| projects.join(&dir[..200]).join(file))
}

fn has_provider_turn(projection: &Value) -> bool {
    projection["providerTurns"].as_array().is_some_and(|t| !t.is_empty())
}

fn run_ids(projection: &Value) -> impl Iterator<Item = &str> {
    projection["runs"].as_array().into_iter().flatten().filter_map(|r| r["id"].as_str())
}

fn active_run(projection: &Value) -> bool {
    projection["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|r| matches!(r["status"].as_str(), Some("preparing" | "queued" | "starting" | "running" | "waiting")))
}

/// T3 titles a thread from the first message it is sent while the thread has no messages,
/// which happens to an imported Claude thread whose first message came from the user before
/// it was linked. Put the imported title back there, as long as no turn has completed.
///
/// The title to restore is the one the thread was created with, which T3 keeps in the
/// provider thread's native metadata. It is not recomputed from the current sources: a
/// session found through both Paseo and Claude Code has a different title in each.
fn restore_title(t3: &mut t3::T3, projection: &Value, thread_key: &str) -> Result<()> {
    let Some((title, retitled_by)) = title_to_restore(projection) else { return Ok(()) };
    let current = projection["thread"]["title"].as_str().unwrap_or("");
    // Keyed by the message that caused the retitle, so it is undone once and a later rename
    // by the user is left alone: T3 answers a repeated commandId without running it.
    t3.dispatch(json!({
        "type": "thread.metadata.update",
        "commandId": stable_id("thread.title.restore", &format!("{thread_key}\0{retitled_by}")),
        "threadId": projection["thread"]["id"],
        "title": title,
    }))?;
    let after = t3.rpc("orchestration.getThreadProjection", json!({ "threadId": projection["thread"]["id"] }))?;
    if after["thread"]["title"] == title {
        println!("    title restored from \"{current}\"");
    }
    Ok(())
}

/// The imported title and the id of the message T3 retitled the thread from.
fn title_to_restore(projection: &Value) -> Option<(&str, &str)> {
    let current = projection["thread"]["title"].as_str().unwrap_or("");
    let imported = projection["providerThreads"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|p| p["nativeMetadata"]["title"].as_str().filter(|t| !t.trim().is_empty()))?;
    // The first message sent through T3; backfilled history (see t3-history) is not.
    let first = projection["messages"]
        .as_array()?
        .iter()
        .filter(|m| m["role"] == "user" && m["id"].as_str().is_some_and(|id| !id.contains(":paseo2t3:")))
        .min_by_key(|m| m["createdAt"].as_str().unwrap_or(""))?;
    if first["text"].as_str().is_none_or(|t| t.starts_with(LINK_NOTE_PREFIX)) {
        return None;
    }
    let completed = projection["runs"].as_array()?.iter().any(|r| r["status"] == "completed");
    (current != imported && !completed).then_some((imported, first["id"].as_str()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(title: Option<&str>) -> Session {
        Session {
            driver: "claudeAgent".into(),
            native_id: "0123456789abcdef".into(),
            cwd: "/w".into(),
            title: title.map(str::to_string),
            model: None,
            updated_at: None,
            archived: false,
            branch: None,
            worktree_path: None,
            project_root: None,
            origin: String::new(),
            agent_id: None,
            parent_agent_id: None,
        }
    }

    #[test]
    fn stable_ids_are_deterministic_uuids_scoped_by_kind() {
        let a = stable_id("thread", "codex:1");
        assert_eq!(a, stable_id("thread", "codex:1"));
        assert_ne!(a, stable_id("thread.create", "codex:1"));
        assert_ne!(a, stable_id("thread", "codex:2"));
        let groups: Vec<_> = a.split('-').map(str::len).collect();
        assert_eq!(groups, [8, 4, 4, 4, 12]);
        assert_eq!(&a[14..15], "5", "version nibble");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"), "RFC 4122 variant");
    }

    #[test]
    fn titles_use_the_first_line_and_are_bounded() {
        assert_eq!(title_for(&session(Some("\n  Fix it  \nmore detail"))), "Fix it");
        let t = title_for(&session(Some(&"x".repeat(100))));
        assert_eq!(t.chars().count(), 81);
        assert!(t.ends_with('…'));
        assert_eq!(title_for(&session(Some("   "))), "Imported Claude session 01234567");
        assert_eq!(title_for(&session(None)), "Imported Claude session 01234567");
    }

    #[test]
    fn roots_compare_without_trailing_separators() {
        assert_eq!(norm_root("/a/b/"), norm_root("/a/b"));
        if cfg!(windows) {
            assert_eq!(norm_root(r"C:\Repo\"), norm_root("c:/repo"));
        }
    }

    #[test]
    fn claude_transcripts_are_found_under_the_encoded_cwd() {
        let home = std::env::temp_dir().join(format!("paseo2t3-claude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(
            claude_transcript(&home, "/home/k/.paseo/worktrees/3avpotug/chilly-camel", "abc"),
            home.join("projects").join("-home-k--paseo-worktrees-3avpotug-chilly-camel").join("abc.jsonl")
        );
        // Over 200 characters Claude Code appends a hash, so the directory is found by prefix.
        let long = format!("/{}", "x".repeat(250));
        let dir = home.join("projects").join(format!("-{}-1a2b3c", "x".repeat(199)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("abc.jsonl"), "").unwrap();
        assert_eq!(claude_transcript(&home, &long, "abc"), dir.join("abc.jsonl"));
        // Claude Code replaces UTF-16 code units, so an emoji takes two dashes.
        assert_eq!(
            claude_transcript(&home, "/w/proj-\u{1F680}", "abc"),
            home.join("projects").join("-w-proj---").join("abc.jsonl")
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn titles_are_restored_only_after_a_retitle_by_the_users_first_message() {
        let projection = |title: &str, first: &str, runs: &[&str]| {
            json!({
                "thread": { "title": title },
                "providerThreads": [{ "nativeMetadata": { "title": "From Paseo" } }],
                "messages": [
                    { "id": "t:paseo2t3:000000", "role": "user", "text": "old history", "createdAt": "2026-01-01T00:00:00Z" },
                    { "id": "m2", "role": "user", "text": "later", "createdAt": "2026-10-31T00:00:02Z" },
                    { "id": "m1", "role": "user", "text": first, "createdAt": "2026-10-31T00:00:01Z" },
                ],
                "runs": runs.iter().map(|s| json!({ "status": s })).collect::<Vec<_>>(),
            })
        };
        assert_eq!(
            title_to_restore(&projection("Resume previous session", "Resume previous session", &["failed"])),
            Some(("From Paseo", "m1"))
        );
        assert_eq!(title_to_restore(&projection("From Paseo", "hi", &["failed"])), None);
        // After the link note T3 never retitles, so a different title is the user's.
        assert_eq!(title_to_restore(&projection("Renamed by hand", &link_note("x"), &["failed"])), None);
        assert_eq!(title_to_restore(&projection("Retitled later", "hi", &["failed", "completed"])), None);
        let mut no_messages = projection("Renamed by hand", "hi", &[]);
        no_messages["messages"] = json!([]);
        assert_eq!(title_to_restore(&no_messages), None);
        let mut no_metadata = projection("Resume previous session", "hi", &["failed"]);
        no_metadata["providerThreads"] = json!([{ "nativeMetadata": null }]);
        assert_eq!(title_to_restore(&no_metadata), None);
    }

    #[test]
    fn link_turns_are_judged_by_the_run_they_started() {
        let before: HashSet<String> = ["r0".to_string()].into();
        let state = |runs: Value, turns: usize, messages: Value| {
            let p = json!({ "runs": runs, "providerTurns": vec![json!({}); turns], "messages": messages });
            match link_state(&p, &before) {
                LinkState::Pending => "pending",
                LinkState::Linked => "linked",
                LinkState::Resumed(_) => "resumed",
                LinkState::Failed(_) => "failed",
                LinkState::TimedOut => "timed out",
            }
        };
        let old = json!({ "id": "r0", "status": "failed" });
        assert_eq!(state(json!([old]), 0, json!([])), "pending");
        assert_eq!(state(json!([old, { "id": "r1", "status": "starting" }]), 0, json!([])), "pending");
        assert_eq!(state(json!([old, { "id": "r1", "status": "failed" }]), 1, json!([])), "linked");
        assert_eq!(state(json!([old, { "id": "r1", "status": "failed" }]), 0, json!([])), "failed");
        assert_eq!(state(json!([old, { "id": "r1", "status": "completed" }]), 1, json!([])), "resumed");
        let reply = json!([{ "runId": "r1", "role": "assistant" }]);
        assert_eq!(state(json!([old, { "id": "r1", "status": "running" }]), 1, reply), "resumed");
    }

    #[test]
    fn unknown_models_fall_back_to_t3_defaults() {
        assert_eq!(default_model("claudeAgent"), "claude-opus-5-5");
        assert_eq!(default_model("codex"), "gpt-6-astra");
        assert_eq!(default_model("opencode"), "openai/gpt-5");
    }
}
