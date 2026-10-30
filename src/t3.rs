//! Minimal T3 Code client: projects over HTTP, orchestration commands over the
//! Effect-RPC WebSocket (JSON serialization), both authorized with a bearer token
//! from `t3 auth session issue`.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::net::TcpStream;
use std::path::Path;
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

pub struct T3 {
    origin: String,
    token: String,
    ws: Option<WebSocket<MaybeTlsStream<TcpStream>>>,
    next_id: u64,
}

pub struct Project {
    pub id: String,
    pub workspace_root: String,
}

/// The origin a running server advertises in `<home>/userdata/server-runtime.json`.
pub fn origin_from_home(home: &Path) -> Result<String> {
    let path = home.join("userdata").join("server-runtime.json");
    let v: Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .with_context(|| format!("reading {} (is the T3 server running?)", path.display()))?,
    )?;
    v["origin"].as_str().map(str::to_string).ok_or_else(|| anyhow!("no origin in {}", path.display()))
}

impl T3 {
    pub fn new(origin: &str, token: &str) -> Self {
        Self { origin: origin.trim_end_matches('/').to_string(), token: token.trim().to_string(), ws: None, next_id: 1 }
    }

    fn post(&self, path: &str, body: Option<&Value>) -> Result<Value> {
        let req = ureq::post(&format!("{}{path}", self.origin)).set("Authorization", &format!("Bearer {}", self.token));
        let res = match body {
            Some(b) => req.send_json(b),
            None => req.call(),
        };
        http_json(res, path)
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let res = ureq::get(&format!("{}/api/projects", self.origin))
            .set("Authorization", &format!("Bearer {}", self.token))
            .call();
        let v = http_json(res, "/api/projects")?;
        Ok(v["projects"].as_array().into_iter().flatten().filter_map(parse_project).collect())
    }

    pub fn create_project(&self, command_id: &str, project_id: &str, title: &str, root: &str) -> Result<Project> {
        let body = json!({
            "type": "project.create",
            "commandId": command_id,
            "projectId": project_id,
            "title": title,
            "workspaceRoot": root,
        });
        let v = self.post("/api/projects/mutate", Some(&body))?;
        parse_project(&v).ok_or_else(|| anyhow!("unexpected project.create response: {v}"))
    }

    fn socket(&mut self) -> Result<&mut WebSocket<MaybeTlsStream<TcpStream>>> {
        if self.ws.is_none() {
            let ticket = self.post("/api/auth/websocket-ticket/rpc", None)?;
            let ticket = ticket["ticket"].as_str().ok_or_else(|| anyhow!("no websocket ticket in response"))?;
            let ws_origin = self.origin.replacen("http", "ws", 1);
            let url = format!("{ws_origin}/ws?orchestrationProtocol=2&wsTicket={ticket}");
            let (ws, _) = tungstenite::connect(url.as_str()).context("connecting to the T3 WebSocket")?;
            self.ws = Some(ws);
        }
        Ok(self.ws.as_mut().expect("socket was just opened"))
    }

    /// One Effect-RPC call: send a `Request`, answer `Ping`s, return the matching `Exit`'s value.
    pub fn rpc(&mut self, method: &str, payload: Value) -> Result<Value> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let request = json!({ "_tag": "Request", "id": id, "tag": method, "payload": payload, "headers": [] });
        let ws = self.socket()?;
        ws.send(Message::text(request.to_string()))?;
        loop {
            let text = match ws.read()? {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                Message::Close(frame) => bail!("T3 closed the WebSocket: {frame:?}"),
                _ => continue,
            };
            let messages = match serde_json::from_str::<Value>(&text)? {
                Value::Array(items) => items,
                single => vec![single],
            };
            for msg in messages {
                match msg["_tag"].as_str() {
                    Some("Ping") => ws.send(Message::text(json!({ "_tag": "Pong" }).to_string()))?,
                    Some("Defect") => bail!("T3 RPC defect: {}", msg["defect"]),
                    Some("Exit") if msg["requestId"].as_str() == Some(id.as_str()) => {
                        let exit = &msg["exit"];
                        return match exit["_tag"].as_str() {
                            Some("Success") => Ok(exit["value"].clone()),
                            _ => Err(anyhow!("{method} failed: {}", exit["cause"])),
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn dispatch(&mut self, command: Value) -> Result<Value> {
        self.rpc("orchestration.dispatchCommand", command)
    }
}

fn parse_project(v: &Value) -> Option<Project> {
    Some(Project {
        id: v.get("id").or_else(|| v.get("projectId"))?.as_str()?.to_string(),
        workspace_root: v.get("workspaceRoot")?.as_str()?.to_string(),
    })
}

fn http_json(res: Result<ureq::Response, ureq::Error>, what: &str) -> Result<Value> {
    match res {
        Ok(r) => Ok(r.into_json()?),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            bail!("{what} returned HTTP {code}: {body}")
        }
        Err(e) => Err(anyhow!("{what}: {e}")),
    }
}
