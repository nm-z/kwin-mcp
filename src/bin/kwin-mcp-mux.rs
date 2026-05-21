use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct ChildSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

struct Mux {
    child_bin: std::path::PathBuf,
    sessions: HashMap<String, ChildSession>,
    initialize_request: Option<Value>,
    initialized_notification: Option<Value>,
    next_internal_id: u64,
}

fn write_msg<W: Write>(mut out: W, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut out, value)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn read_msg(reader: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(anyhow!("child MCP server closed stdout"));
    }
    Ok(serde_json::from_str(line.trim_end())?)
}

fn request_id(value: &Value) -> Option<&Value> {
    value.get("id")
}

fn method(value: &Value) -> Option<&str> {
    value.get("method").and_then(Value::as_str)
}

fn result_text(text: impl Into<String>, structured: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": text.into()}],
        "structuredContent": structured,
    })
}

fn child_binary_path() -> Result<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("KWIN_MCP_CHILD") {
        return Ok(path.into());
    }
    let current = std::env::current_exe()?;
    let dir = current
        .parent()
        .ok_or_else(|| anyhow!("current executable has no parent directory"))?;
    Ok(dir.join("kwin-mcp"))
}

fn valid_session_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn session_key_from_call(req: &Value) -> Result<String> {
    let args = req
        .get("params")
        .and_then(|p| p.get("arguments"))
        .and_then(Value::as_object);
    let key = args
        .and_then(|m| m.get("session"))
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned();
    if !valid_session_key(&key) {
        return Err(anyhow!(
            "invalid session key '{key}': use ASCII letters, digits, hyphen, underscore, or dot"
        ));
    }
    Ok(key)
}

fn strip_mux_args(req: &Value) -> Value {
    let mut out = req.clone();
    if let Some(args) = out
        .get_mut("params")
        .and_then(|p| p.get_mut("arguments"))
        .and_then(Value::as_object_mut)
    {
        args.remove("session");
        args.remove("all");
    }
    out
}

fn add_session_param_to_tools(response: &mut Value) {
    let Some(tools) = response
        .get_mut("result")
        .and_then(|r| r.get_mut("tools"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };

    for tool in tools {
        if let Some(desc) = tool.get_mut("description").and_then(Value::as_str) {
            let new_desc = format!(
                "Mux session-aware. Pass session=<unique teammate/session name> to isolate this call. {desc}"
            );
            tool["description"] = Value::String(new_desc);
        }

        let schema = tool
            .get_mut("inputSchema")
            .or_else(|| tool.get_mut("input_schema"));
        let Some(schema) = schema.and_then(Value::as_object_mut) else {
            continue;
        };

        let props = schema
            .entry("properties".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(props) = props.as_object_mut() else {
            continue;
        };
        props.insert(
            "session".to_owned(),
            json!({
                "type": "string",
                "description": "Logical kwin-mcp session key. In Agent Teams, use the teammate name, e.g. probe-a or slave-4. Calls with different session values get different child kwin-mcp processes and isolated KWin workdirs. Omit for default."
            }),
        );

        if tool.get("name").and_then(Value::as_str) == Some("session_stop") {
            props.insert(
                "all".to_owned(),
                json!({
                    "type": "boolean",
                    "description": "Mux extension. Stop every logical session owned by this mux process."
                }),
            );
        }
    }
}

impl Mux {
    fn new() -> Result<Self> {
        Ok(Self {
            child_bin: child_binary_path()?,
            sessions: HashMap::new(),
            initialize_request: None,
            initialized_notification: None,
            next_internal_id: 1,
        })
    }

    fn next_id(&mut self) -> Value {
        let id = self.next_internal_id;
        self.next_internal_id = self.next_internal_id.saturating_add(1);
        Value::String(format!("kwin-mcp-mux-{id}"))
    }

    fn spawn_session(&mut self, key: &str) -> Result<()> {
        if self.sessions.contains_key(key) {
            return Ok(());
        }

        let mut child = Command::new(&self.child_bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawn child MCP server {}", self.child_bin.display()))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("child stdin missing"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("child stdout missing"))?;
        let mut sess = ChildSession {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };

        if let Some(init_template) = self.initialize_request.clone() {
            let mut init = init_template;
            let init_id = self.next_id();
            init["id"] = init_id.clone();
            write_msg(&mut sess.stdin, &init)?;
            loop {
                let response = read_msg(&mut sess.stdout)?;
                if request_id(&response) == Some(&init_id) {
                    break;
                }
                eprintln!("kwin-mcp-mux: dropping child pre-init message for {key}: {response}");
            }
        }

        if let Some(initialized) = self.initialized_notification.clone() {
            write_msg(&mut sess.stdin, &initialized)?;
        }

        self.sessions.insert(key.to_owned(), sess);
        Ok(())
    }

    fn forward_to_session(&mut self, key: &str, req: &Value, mutate_tools_list: bool) -> Result<Value> {
        self.spawn_session(key)?;
        let sess = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| anyhow!("session '{key}' missing after spawn"))?;
        let id = request_id(req).cloned();
        write_msg(&mut sess.stdin, req)?;

        loop {
            let mut response = read_msg(&mut sess.stdout)?;
            if id.as_ref().is_some_and(|want| request_id(&response) == Some(want)) {
                if mutate_tools_list {
                    add_session_param_to_tools(&mut response);
                }
                return Ok(response);
            }
            write_msg(std::io::stdout(), &response)?;
        }
    }

    fn stop_all(&mut self, req: &Value) -> Value {
        let id = request_id(req).cloned().unwrap_or(Value::Null);
        let count = self.sessions.len();
        for (_key, mut sess) in self.sessions.drain() {
            let _ = sess.child.kill();
            let _ = sess.child.wait();
        }
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result_text(
                format!("kwin-mcp-mux stopped {count} logical sessions"),
                json!({"status": "stopped", "count": count})
            )
        })
    }

    fn handle(&mut self, req: Value) -> Result<Option<Value>> {
        match method(&req) {
            Some("initialize") => {
                self.initialize_request = Some(req.clone());
                let response = self.forward_to_session("__control", &req, false)?;
                Ok(Some(response))
            }
            Some("notifications/initialized") => {
                self.initialized_notification = Some(req.clone());
                for sess in self.sessions.values_mut() {
                    write_msg(&mut sess.stdin, &req)?;
                }
                Ok(None)
            }
            Some("tools/list") => {
                let response = self.forward_to_session("__control", &req, true)?;
                Ok(Some(response))
            }
            Some("tools/call") => {
                let name = req
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let args = req
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .and_then(Value::as_object);
                if name == "session_stop"
                    && args
                        .and_then(|m| m.get("all"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                {
                    return Ok(Some(self.stop_all(&req)));
                }

                let key = session_key_from_call(&req)?;
                let child_req = strip_mux_args(&req);
                let response = self.forward_to_session(&key, &child_req, false)?;

                if name == "session_stop" {
                    if let Some(mut sess) = self.sessions.remove(&key) {
                        let _ = sess.child.wait();
                    }
                }

                Ok(Some(response))
            }
            Some(_) => {
                let response = self.forward_to_session("__control", &req, false)?;
                Ok(Some(response))
            }
            None => Ok(None),
        }
    }
}

fn main() -> Result<()> {
    let stdin = std::io::stdin();
    let mut mux = Mux::new()?;

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = serde_json::from_str(&line)?;
        match mux.handle(req) {
            Ok(Some(response)) => write_msg(std::io::stdout(), &response)?,
            Ok(None) => {}
            Err(err) => {
                let fallback = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32603, "message": err.to_string()}
                });
                write_msg(std::io::stdout(), &fallback)?;
            }
        }
    }

    Ok(())
}
