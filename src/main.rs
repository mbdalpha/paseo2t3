mod sources;
mod t3;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sources::{Scan, Session};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

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
    /// File holding a bearer token from `t3 auth session issue --token-only`.
    #[arg(long, conflicts_with = "token")]
    token_file: Option<PathBuf>,
    #[arg(long, env = "T3_TOKEN", hide_env_values = true)]
    token: Option<String>,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Scan { sources, json } => {
            let scan = collect(&sources)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&json!({ "sessions": scan.sessions, "skipped": scan.skipped }))?);
            } else {
                print_plan(&scan);
            }
            Ok(())
        }
        Command::Import { sources, target, apply, runtime_mode, no_path_check } => {
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
            import(&scan.sessions, &target, &runtime_mode)
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

    let mut seen = HashSet::new();
    let sessions = std::mem::take(&mut scan.sessions);
    for mut s in sessions {
        if !seen.insert((s.driver.clone(), s.native_id.clone())) {
            continue;
        }
        if s.archived && !args.include_archived {
            scan.skipped.push(sources::Skipped { origin: s.origin, reason: "archived (use --include-archived)".into() });
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
    if cfg!(windows) { t.replace('/', "\\").to_lowercase() } else { t.to_string() }
}

fn default_model(driver: &str) -> &'static str {
    // T3 Code 0.1.5 DEFAULT_MODEL_BY_PROVIDER.
    match driver {
        "claudeAgent" => "claude-opus-5-5",
        "opencode" => "openai/gpt-5",
        _ => "gpt-6-astra",
    }
}

fn import(sessions: &[Session], target: &TargetArgs, runtime_mode: &str) -> Result<()> {
    let origin = match (&target.t3_url, &target.t3_home) {
        (Some(url), _) => url.clone(),
        (None, Some(home)) => t3::origin_from_home(home)?,
        (None, None) => bail!("name the T3 server with --t3-url or --t3-home (there is deliberately no default)"),
    };
    let token = match (&target.token_file, &target.token) {
        (Some(f), _) => std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?,
        (None, Some(t)) => t.clone(),
        (None, None) => bail!("pass --token-file or set T3_TOKEN (create one with `t3 auth session issue --token-only`)"),
    };
    let mut t3 = t3::T3::new(&origin, &token);
    println!("\nImporting into {origin}");

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
        if t3.rpc("orchestration.getThreadProjection", json!({ "threadId": thread_id })).is_ok() {
            existing += 1;
            println!("  = thread {} {}  already imported", s.driver, short(&s.native_id));
            continue;
        }
        let model_selection = json!({
            "instanceId": s.driver,
            "model": s.model.clone().unwrap_or_else(|| default_model(&s.driver).to_string()),
        });
        let title = title_for(s);
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
    if sessions.iter().any(|s| s.driver == "claudeAgent") {
        // T3 0.1.5 only resumes a Claude session once the thread has a recorded provider turn,
        // so the first open of an imported thread asks Claude Code for a *new* session with the
        // existing id and fails; the failed attempt records a turn and later messages resume.
        println!(
            "\nNote: in T3 Code 0.1.5 the first message in an imported Claude thread fails with\n\
             \"Provider turn failed\". Send it again: from then on the thread resumes the original\n\
             session with full history."
        );
    }
    if failed > 0 {
        bail!("{failed} item(s) failed");
    }
    Ok(())
}
