//! Readers for the session stores we port from. Each reader yields `Session`s that
//! point at a provider-native conversation T3 Code can resume.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Session {
    /// T3 provider driver kind: `claudeAgent`, `codex` or `opencode`.
    pub driver: String,
    pub native_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub updated_at: Option<String>,
    pub archived: bool,
    pub branch: Option<String>,
    /// Set when the session ran inside a git worktree of `project_root`.
    pub worktree_path: Option<String>,
    /// Workspace root for the T3 project; filled by `resolve_project_root`.
    pub project_root: Option<String>,
    /// Where we found it, e.g. `paseo:agt_123` or `claude:<file>`.
    pub origin: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Skipped {
    pub origin: String,
    pub reason: String,
}

#[derive(Default)]
pub struct Scan {
    pub sessions: Vec<Session>,
    pub skipped: Vec<Skipped>,
}

impl Scan {
    fn skip(&mut self, origin: impl Into<String>, reason: impl Into<String>) {
        self.skipped.push(Skipped { origin: origin.into(), reason: reason.into() });
    }
}

pub fn driver_for_paseo_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "claude" => Some("claudeAgent"),
        "codex" => Some("codex"),
        "opencode" => Some("opencode"),
        _ => None,
    }
}

fn str_field<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().filter(|s| !s.trim().is_empty())
}

fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json") && p.is_file())
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------- Paseo

/// Paseo 0.11.x keeps agents at `agents/<projectDir>/<agentId>.json` (older ones directly in
/// `agents/`) and projects/workspaces as JSON arrays under `projects/`.
pub fn scan_paseo(home: &Path, scan: &mut Scan) -> Result<()> {
    let agents_dir = home.join("agents");
    if !agents_dir.is_dir() {
        scan.skip(format!("paseo:{}", home.display()), "no agents/ directory (Paseo has not persisted any agents)");
        return Ok(());
    }

    let projects: HashMap<String, Value> = read_array(&home.join("projects").join("projects.json"))?
        .into_iter()
        .filter_map(|p| Some((str_field(&p, &["projectId"])?.to_string(), p)))
        .collect();
    let workspaces: HashMap<String, Value> = read_array(&home.join("projects").join("workspaces.json"))?
        .into_iter()
        .filter_map(|w| Some((str_field(&w, &["workspaceId"])?.to_string(), w)))
        .collect();

    let mut files = json_files(&agents_dir);
    if let Ok(entries) = fs::read_dir(&agents_dir) {
        for dir in entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            files.extend(json_files(&dir));
        }
    }

    for file in files {
        let origin = format!("paseo:{}", file.display());
        let agent = match read_json(&file) {
            Ok(v) => v,
            Err(e) => {
                scan.skip(origin, format!("unreadable: {e:#}"));
                continue;
            }
        };
        let origin = format!("paseo:{}", str_field(&agent, &["id"]).unwrap_or("?"));
        if agent.get("internal").and_then(Value::as_bool) == Some(true) {
            scan.skip(origin, "internal Paseo agent");
            continue;
        }
        // persistence.provider names the native tool even for custom provider ids.
        let provider = str_field(&agent, &["persistence", "provider"])
            .or_else(|| str_field(&agent, &["provider"]))
            .unwrap_or("?");
        let Some(driver) = driver_for_paseo_provider(provider) else {
            scan.skip(origin, format!("provider `{provider}` has no T3 equivalent this tool supports"));
            continue;
        };
        let Some(native_id) = str_field(&agent, &["persistence", "sessionId"])
            .or_else(|| str_field(&agent, &["runtimeInfo", "sessionId"]))
        else {
            scan.skip(origin, "no provider session id (agent never started)");
            continue;
        };
        let Some(cwd) = str_field(&agent, &["cwd"]) else {
            scan.skip(origin, "no cwd");
            continue;
        };

        let workspace = str_field(&agent, &["workspaceId"]).and_then(|id| workspaces.get(id));
        let project = workspace
            .and_then(|w| str_field(w, &["projectId"]))
            .and_then(|id| projects.get(id));
        let is_worktree = workspace.and_then(|w| str_field(w, &["kind"])) == Some("worktree");

        scan.sessions.push(Session {
            driver: driver.to_string(),
            native_id: native_id.to_string(),
            cwd: cwd.to_string(),
            title: str_field(&agent, &["title"]).map(str::to_string),
            model: str_field(&agent, &["config", "model"])
                .or_else(|| str_field(&agent, &["runtimeInfo", "model"]))
                .map(str::to_string),
            updated_at: str_field(&agent, &["lastActivityAt"])
                .or_else(|| str_field(&agent, &["updatedAt"]))
                .map(str::to_string),
            archived: str_field(&agent, &["archivedAt"]).is_some(),
            branch: workspace.and_then(|w| str_field(w, &["branch"])).map(str::to_string),
            worktree_path: is_worktree
                .then(|| workspace.and_then(|w| str_field(w, &["cwd"])).map(str::to_string))
                .flatten(),
            project_root: workspace
                .and_then(|w| str_field(w, &["mainRepoRoot"]))
                .or_else(|| project.and_then(|p| str_field(p, &["rootPath"])))
                .map(str::to_string),
            origin,
        });
    }
    Ok(())
}

fn read_array(path: &Path) -> Result<Vec<Value>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    match read_json(path)? {
        Value::Array(items) => Ok(items),
        _ => anyhow::bail!("{} is not a JSON array", path.display()),
    }
}

// ---------------------------------------------------------------- Claude Code

/// `<claude home>/projects/<encoded cwd>/<sessionId>.jsonl`. Subagent transcripts live in
/// nested directories and are not top-level sessions, so only the first level is read.
pub fn scan_claude(home: &Path, scan: &mut Scan) -> Result<()> {
    let projects = home.join("projects");
    let Ok(dirs) = fs::read_dir(&projects) else {
        scan.skip(format!("claude:{}", projects.display()), "no projects/ directory");
        return Ok(());
    };
    for dir in dirs.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        let Ok(files) = fs::read_dir(&dir) else { continue };
        for file in files.flatten().map(|e| e.path()) {
            if file.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            let origin = format!("claude:{}", file.display());
            match read_claude_session(&file) {
                Ok(Some(s)) => scan.sessions.push(Session { origin, ..s }),
                Ok(None) => scan.skip(origin, "no user messages"),
                Err(e) => scan.skip(origin, format!("unreadable: {e:#}")),
            }
        }
    }
    Ok(())
}

fn read_claude_session(file: &Path) -> Result<Option<Session>> {
    let reader = BufReader::new(fs::File::open(file)?);
    let mut session_id = file.file_stem().and_then(|s| s.to_str()).map(str::to_string);
    let (mut cwd, mut ai_title, mut summary, mut first_prompt) = (None, None, None, None);
    let (mut model, mut branch, mut last_ts) = (None, None, None);

    for line in reader.lines() {
        let Ok(v) = serde_json::from_str::<Value>(&line?) else { continue };
        if let Some(ts) = str_field(&v, &["timestamp"]) {
            last_ts = Some(ts.to_string());
        }
        match str_field(&v, &["type"]).unwrap_or("") {
            "ai-title" => ai_title = str_field(&v, &["aiTitle"]).map(str::to_string),
            "summary" => summary = str_field(&v, &["summary"]).map(str::to_string),
            "user" if v.get("isSidechain").and_then(Value::as_bool) != Some(true) => {
                if let Some(id) = str_field(&v, &["sessionId"]) {
                    session_id = Some(id.to_string());
                }
                if cwd.is_none() {
                    cwd = str_field(&v, &["cwd"]).map(str::to_string);
                    branch = str_field(&v, &["gitBranch"]).map(str::to_string);
                }
                if first_prompt.is_none() {
                    first_prompt = claude_user_text(&v);
                }
            }
            "assistant" => {
                if let Some(m) = str_field(&v, &["message", "model"]).filter(|m| !m.starts_with('<')) {
                    model = Some(m.to_string());
                }
            }
            _ => {}
        }
    }

    let (Some(native_id), Some(cwd)) = (session_id, cwd) else { return Ok(None) };
    if first_prompt.is_none() && ai_title.is_none() && summary.is_none() {
        return Ok(None);
    }
    Ok(Some(Session {
        driver: "claudeAgent".into(),
        native_id,
        cwd,
        title: ai_title.or(summary).or(first_prompt),
        model,
        updated_at: last_ts,
        archived: false,
        branch: branch.filter(|b| b != "HEAD"),
        worktree_path: None,
        project_root: None,
        origin: String::new(),
    }))
}

fn claude_user_text(v: &Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|b| b.get("text")?.as_str())?
            .to_string(),
        _ => return None,
    };
    // Slash-command and hook envelopes are not prompts a person wrote.
    let text = text.trim();
    (!text.is_empty() && !text.starts_with('<')).then(|| text.to_string())
}

// ---------------------------------------------------------------- Codex

/// `<codex home>/sessions/YYYY/MM/DD/rollout-*.jsonl` (plus `archived_sessions/`). Thread
/// names come from `session_index.jsonl`; sub-agent threads (guardian reviews etc.) are skipped.
pub fn scan_codex(home: &Path, scan: &mut Scan) -> Result<()> {
    let mut names: HashMap<String, String> = HashMap::new();
    if let Ok(f) = fs::File::open(home.join("session_index.jsonl")) {
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if let (Some(id), Some(name)) = (str_field(&v, &["id"]), str_field(&v, &["thread_name"])) {
                    names.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    for (dir, archived) in [(home.join("sessions"), false), (home.join("archived_sessions"), true)] {
        let mut files = Vec::new();
        collect_jsonl(&dir, &mut files);
        files.sort();
        for file in files {
            let origin = format!("codex:{}", file.display());
            match read_codex_session(&file, &names) {
                Ok(Ok(s)) => scan.sessions.push(Session { origin, archived, ..s }),
                Ok(Err(reason)) => scan.skip(origin, reason),
                Err(e) => scan.skip(origin, format!("unreadable: {e:#}")),
            }
        }
    }
    Ok(())
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        if path.is_dir() {
            collect_jsonl(&path, out);
        } else if path.extension().is_some_and(|x| x == "jsonl") {
            out.push(path);
        }
    }
}

fn read_codex_session(file: &Path, names: &HashMap<String, String>) -> Result<Result<Session, String>> {
    let mut lines = BufReader::new(fs::File::open(file)?).lines();
    let Some(first) = lines.next().transpose()? else { return Ok(Err("empty file".into())) };
    let meta: Value = serde_json::from_str(&first)?;
    if str_field(&meta, &["type"]) != Some("session_meta") {
        return Ok(Err("first record is not session_meta".into()));
    }
    let p = &meta["payload"];
    if p.get("source").and_then(|s| s.get("subagent")).is_some() || str_field(p, &["parent_thread_id"]).is_some() {
        return Ok(Err("sub-agent thread".into()));
    }
    let (Some(id), Some(cwd)) = (str_field(p, &["id"]), str_field(p, &["cwd"])) else {
        return Ok(Err("session_meta lacks id or cwd".into()));
    };

    let (mut model, mut first_prompt, mut last_ts) = (None, None, str_field(&meta, &["timestamp"]).map(str::to_string));
    for line in lines.map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(ts) = str_field(&v, &["timestamp"]) {
            last_ts = Some(ts.to_string());
        }
        match (str_field(&v, &["type"]), str_field(&v, &["payload", "type"])) {
            (Some("turn_context"), _) if model.is_none() => {
                model = str_field(&v, &["payload", "model"]).map(str::to_string);
            }
            (Some("event_msg"), Some("user_message")) if first_prompt.is_none() => {
                first_prompt = str_field(&v, &["payload", "message"]).map(|s| s.trim().to_string());
            }
            _ => {}
        }
    }
    if first_prompt.is_none() && !names.contains_key(id) {
        return Ok(Err("no user messages".into()));
    }
    Ok(Ok(Session {
        driver: "codex".into(),
        native_id: id.to_string(),
        cwd: cwd.to_string(),
        title: names.get(id).cloned().or(first_prompt),
        model,
        updated_at: last_ts,
        archived: false,
        branch: None,
        worktree_path: None,
        project_root: None,
        origin: String::new(),
    }))
}

// ---------------------------------------------------------------- project roots

/// Choose the T3 project root for a session: keep what Paseo recorded, otherwise use the
/// enclosing git repository. A linked worktree belongs to its main repository's project.
pub fn resolve_project_root(s: &mut Session) {
    find_project_root(s);
    // Claude Code finds a session under its exact original cwd, and T3 runs the thread in
    // its worktree or project root, so those must equal the cwd for the resume to work.
    if s.driver == "claudeAgent" && s.worktree_path.as_ref().or(s.project_root.as_ref()) != Some(&s.cwd) {
        s.project_root = Some(s.cwd.clone());
        s.worktree_path = None;
    }
}

fn find_project_root(s: &mut Session) {
    if s.project_root.is_some() {
        return;
    }
    let cwd = PathBuf::from(&s.cwd);
    let mut dir = Some(cwd.as_path());
    while let Some(d) = dir {
        let dot_git = d.join(".git");
        if dot_git.is_dir() {
            s.project_root = Some(d.display().to_string());
            return;
        }
        if dot_git.is_file() {
            if let Some((main, branch)) = linked_worktree(&dot_git) {
                s.worktree_path.get_or_insert_with(|| d.display().to_string());
                if s.branch.is_none() {
                    s.branch = branch;
                }
                s.project_root = Some(main);
            } else {
                s.project_root = Some(d.display().to_string());
            }
            return;
        }
        dir = d.parent();
    }
    s.project_root = Some(s.cwd.clone());
}

/// A worktree's `.git` file reads `gitdir: <main>/.git/worktrees/<name>`.
fn linked_worktree(dot_git: &Path) -> Option<(String, Option<String>)> {
    let text = fs::read_to_string(dot_git).ok()?;
    let gitdir = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() { gitdir } else { dot_git.parent()?.join(gitdir) };
    let worktrees = gitdir.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let main = worktrees.parent()?.parent()?.display().to_string();
    let branch = fs::read_to_string(gitdir.join("HEAD"))
        .ok()
        .and_then(|h| h.trim().strip_prefix("ref: refs/heads/").map(str::to_string));
    Some((main, branch))
}
