mod sources;
mod t3;

use anyhow::{bail, Context, Result};
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
            let scan = collect(&sources)?;
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
            let mut scan = collect(&sources)?;
            if !no_path_check {
                let (keep, missing): (Vec<_>, Vec<_>) = scan
                    .sessions
                    .drain(..)
                    .partition(|s| s.project_root.as_deref().is_some_and(|r| Path::new(r).is_dir()));
                scan.sessions = keep;
                for s in missing {
                    scan.skipped.push(sources::Skipped {
                        origin: s.origin,
                        reason: format!("project root {} does not exist here", s.project_root.unwrap_or_default()),
                    });
                }
            }
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

fn collect(args: &SourceArgs) -> Result<Scan> {
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

    // An archived sub-agent of a live agent is kept (and archived in T3), so the live agent's
    // thread lists all of its sub-agents.
    let live_agents: HashSet<String> =
        scan.sessions.iter().filter(|s| !s.archived).filter_map(|s| s.agent_id.clone()).collect();
    let mut seen = HashSet::new();
    let sessions = std::mem::take(&mut scan.sessions);
    for mut s in sessions {
        if !seen.insert((s.driver.clone(), s.native_id.clone())) {
            continue;
        }
        let live_parent = s.parent_agent_id.as_ref().is_some_and(|p| live_agents.contains(p));
        if s.archived && !args.include_archived && !live_parent {
            scan.skipped
                .push(sources::Skipped { origin: s.origin, reason: "archived (use --include-archived)".into() });
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
    let mut link = claude_home.is_some();
    let claude_home = claude_home.unwrap_or(Path::new(""));

    let mut projects: HashMap<String, String> =
        t3.projects()?.into_iter().map(|p| (norm_root(&p.workspace_root), p.id)).collect();

    let (mut created, mut existing, mut failed) = (0usize, 0usize, 0usize);
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
            if link && s.driver == "claudeAgent" && thread["archivedAt"].is_null() {
                link_outcome(&mut t3, &projection, s, claude_home, &mut link, &mut failed);
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
        match t3.dispatch(command) {
            Ok(_) => {
                created += 1;
                println!("  + thread {} {}  {title}", s.driver, short(&s.native_id));
                // Link before archiving: an archived thread should not start turns.
                if link && s.driver == "claudeAgent" {
                    match t3.rpc("orchestration.getThreadProjection", json!({ "threadId": thread_id })) {
                        Ok(projection) => link_outcome(&mut t3, &projection, s, claude_home, &mut link, &mut failed),
                        Err(e) => {
                            failed += 1;
                            println!("    ! link failed: {e:#}");
                        }
                    }
                }
                if s.archived {
                    let archive = json!({
                        "type": "thread.archive",
                        "commandId": stable_id("thread.archive", &thread_key),
                        "threadId": thread_id,
                    });
                    if let Err(e) = t3.dispatch(archive) {
                        println!("    ! archive failed: {e:#}");
                    }
                }
            }
            Err(e) => {
                failed += 1;
                println!("  ! thread {} {}: {e:#}", s.driver, short(&s.native_id));
            }
        }
    }
    println!("\nDone: {created} imported, {existing} already present, {failed} failure(s).");
    if !link && sessions.iter().any(|s| s.driver == "claudeAgent") {
        println!(
            "\nNote: Claude threads were not linked. In T3 Code 0.1.5 the first message in an\n\
             unlinked imported Claude thread fails with \"Provider turn failed\"; send it again\n\
             and the thread resumes the original session."
        );
    }
    if failed > 0 {
        bail!("{failed} item(s) failed");
    }
    Ok(())
}

/// The note sent as the throwaway first turn of an imported Claude thread. Claude Code
/// rejects that turn before any model call, and T3 hands the note to the next turn as
/// earlier context, so it must read as information rather than a request.
fn link_note(native_id: &str) -> String {
    format!(
        "[paseo2t3] Imported Claude Code session {native_id}. Earlier messages are not shown \
         in T3; the session keeps them. T3 Code cannot resume a session on a thread's first \
         turn (t3code#15243), so this turn fails and the next message continues the session."
    )
}

enum Link {
    /// The thread already has a provider turn, so T3 will resume it.
    AlreadyLinked,
    /// The throwaway turn failed as expected and left a provider turn behind.
    Linked,
    /// T3 resumed the session on its first turn: this T3 no longer needs linking.
    Resumed,
}

fn link_outcome(
    t3: &mut t3::T3,
    projection: &Value,
    s: &Session,
    claude_home: &Path,
    link: &mut bool,
    failed: &mut usize,
) {
    // T3 cannot start Claude Code in a directory that is gone (a deleted worktree), and
    // Claude Code could not resume the session there anyway.
    if !Path::new(&s.cwd).is_dir() {
        println!("    not linked: {} no longer exists", s.cwd);
        return;
    }
    // Without the transcript where Claude Code looks it up, the "throwaway" turn would start
    // a new session under this id and reach the model instead of failing.
    let transcript = claude_transcript(claude_home, &s.cwd, &s.native_id);
    if !transcript.is_file() {
        println!("    not linked: no transcript at {}", transcript.display());
        return;
    }
    match link_claude_thread(t3, projection, s) {
        Ok(Link::AlreadyLinked) => {}
        Ok(Link::Linked) => println!("    linked: the next message resumes session {}", short(&s.native_id)),
        Ok(Link::Resumed) => {
            println!("    T3 resumed the session on its first turn, so linking is not needed; skipping it from here");
            *link = false;
        }
        Err(e) => {
            *failed += 1;
            println!("    ! link failed: {e:#}");
        }
    }
}

/// T3 Code 0.1.5 passes `resume` to Claude Code only once the thread has a recorded provider
/// turn (t3code#15243). Without one, the first message asks Claude Code to create a session
/// under the imported id, which it refuses, so the user's first message fails and, because
/// the thread has no messages yet, T3 also retitles the thread from it. Sending one note
/// first takes that failure here, before the user ever opens the thread.
fn link_claude_thread(t3: &mut t3::T3, projection: &Value, s: &Session) -> Result<Link> {
    let thread_key = format!("{}:{}", s.driver, s.native_id);
    let thread_id = stable_id("thread", &thread_key);
    if has_provider_turn(projection) {
        return Ok(Link::AlreadyLinked);
    }
    if active_run(projection) {
        bail!("the thread has a run in progress; re-run the import once it finishes");
    }
    let run_count = |p: &Value| p["runs"].as_array().map_or(0, Vec::len);
    let runs_before = run_count(projection);
    t3.dispatch(json!({
        "type": "message.dispatch",
        "commandId": stable_id("thread.link", &thread_key),
        "threadId": thread_id,
        "messageId": stable_id("message.link", &thread_key),
        "text": link_note(&s.native_id),
        "attachments": [],
        "createdBy": "user",
        "creationSource": "web",
        "dispatchMode": { "type": "start_immediately" },
    }))?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let p = t3.rpc("orchestration.getThreadProjection", json!({ "threadId": thread_id }))?;
        if has_provider_turn(&p) && !active_run(&p) {
            let resumed = p["runs"].as_array().into_iter().flatten().any(|r| r["status"] == "completed");
            return Ok(if resumed { Link::Resumed } else { Link::Linked });
        }
        if !active_run(&p) && run_count(&p) > runs_before {
            bail!("the first turn ended without reaching Claude Code (is the claude CLI installed for T3?)");
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for the first turn to finish");
        }
    }
}

/// Where Claude Code keeps a session: `projects/<cwd with every non-alphanumeric character
/// replaced by '-'>/<id>.jsonl`. Claude Code shortens directory names over 200 characters
/// with a hash suffix, so those are matched on the 200-character prefix.
fn claude_transcript(claude_home: &Path, cwd: &str, native_id: &str) -> PathBuf {
    let projects = claude_home.join("projects");
    let dir: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
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

fn active_run(projection: &Value) -> bool {
    projection["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|r| matches!(r["status"].as_str(), Some("preparing" | "queued" | "starting" | "running" | "waiting")))
}

/// T3 titles a thread from its first message when the thread has no messages, which every
/// unlinked imported thread qualifies for. Put the imported title back on threads where no
/// turn has completed yet, i.e. where the current title can only have come from that.
///
/// The title to restore is the one the thread was created with, which T3 keeps in the
/// provider thread's native metadata. It is not recomputed from the current sources: a
/// session found through both Paseo and Claude Code has a different title in each.
fn restore_title(t3: &mut t3::T3, projection: &Value, thread_key: &str) -> Result<()> {
    let Some(title) = title_to_restore(projection) else { return Ok(()) };
    let current = projection["thread"]["title"].as_str().unwrap_or("");
    t3.dispatch(json!({
        "type": "thread.metadata.update",
        "commandId": stable_id("thread.title.restore", &format!("{thread_key}\0{current}")),
        "threadId": projection["thread"]["id"],
        "title": title,
    }))?;
    println!("    title restored from \"{current}\"");
    Ok(())
}

fn title_to_restore(projection: &Value) -> Option<&str> {
    let current = projection["thread"]["title"].as_str().unwrap_or("");
    let imported = projection["providerThreads"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|p| p["nativeMetadata"]["title"].as_str().filter(|t| !t.trim().is_empty()))?;
    let runs = projection["runs"].as_array()?;
    let completed = runs.iter().any(|r| r["status"] == "completed");
    (current != imported && !runs.is_empty() && !completed).then_some(imported)
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
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn titles_are_restored_to_the_imported_title_only_before_any_turn_completes() {
        let projection = |title: &str, runs: &[&str]| {
            json!({
                "thread": { "title": title },
                "providerThreads": [{ "nativeMetadata": { "title": "From Paseo" } }],
                "runs": runs.iter().map(|s| json!({ "status": s })).collect::<Vec<_>>(),
            })
        };
        assert_eq!(title_to_restore(&projection("Resume previous session", &["failed"])), Some("From Paseo"));
        assert_eq!(title_to_restore(&projection("From Paseo", &["failed"])), None);
        assert_eq!(title_to_restore(&projection("Renamed by hand", &[])), None);
        assert_eq!(title_to_restore(&projection("Retitled later", &["failed", "completed"])), None);
        let mut no_metadata = projection("Resume previous session", &["failed"]);
        no_metadata["providerThreads"] = json!([{ "nativeMetadata": null }]);
        assert_eq!(title_to_restore(&no_metadata), None);
    }

    #[test]
    fn unknown_models_fall_back_to_t3_defaults() {
        assert_eq!(default_model("claudeAgent"), "claude-opus-5-5");
        assert_eq!(default_model("codex"), "gpt-6-astra");
        assert_eq!(default_model("opencode"), "openai/gpt-5");
    }
}
