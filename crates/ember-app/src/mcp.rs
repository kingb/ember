//! Minimal MCP (Model Context Protocol) stdio server exposing the debug control
//! surface as tools, so any MCP client (e.g. Claude Code) can drive + introspect
//! a running ember-term: `ember-term mcp`. Newline-delimited JSON-RPC 2.0 over
//! stdin/stdout. The tools proxy to a live instance's `EMBER_CONTROL` socket
//! (auto-targeting the sole instance, or `pid`/`sock` to pick one); `ember_screenshot`
//! renders headlessly and returns a path to read.

use std::io::{BufRead, Write};

use ember_core::Axis;
use serde_json::{Value, json};

use crate::{control, screenshot};

/// Run the JSON-RPC loop until stdin closes.
pub fn serve() -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let result = handle(method, req.get("params"));
        // Only requests (with an id) get a response; notifications get nothing.
        if let Some(id) = id {
            let msg = match result {
                Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
                Err(e) => {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": e}})
                }
            };
            writeln!(stdout, "{msg}").map_err(|e| e.to_string())?;
            stdout.flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn handle(method: &str, params: Option<&Value>) -> Result<Value, String> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "ember-term", "version": env!("CARGO_PKG_VERSION")},
        })),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => tools_call(params),
        "ping" => Ok(json!({})),
        _ => Err(format!("method not found: {method}")),
    }
}

/// Properties every instance-targeting tool accepts (optional disambiguation).
fn target_props() -> Value {
    json!({
        "pid": {"type": "integer", "description": "Target instance PID (optional; auto when only one is running)."},
        "sock": {"type": "string", "description": "Explicit control socket path (optional)."}
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "ember_list",
            "description": "List running ember-term instances (pid + control socket).",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "ember_state",
            "description": "Dump the live app state as JSON: scale_factor, surface size, a tabs array (index/active/title/sessions for EVERY tab), and each active-tab pane's dims/cursor/styles_known/screen-text. The primary way to SEE what a running ember-term is rendering.",
            "inputSchema": {"type": "object", "properties": target_props()}
        },
        {
            "name": "ember_focus",
            "description": "Focus the first tab whose title contains the query (case-insensitive) and raise the Ember window. Returns the matched index+title, or the list of titles seen when nothing matches.",
            "inputSchema": {"type": "object", "properties": with(target_props(), "query", json!({"type": "string", "description": "Substring to match against tab titles."})), "required": ["query"]}
        },
        {
            "name": "ember_raise",
            "description": "Bring the Ember window to the front and give it keyboard focus.",
            "inputSchema": {"type": "object", "properties": target_props()}
        },
        {
            "name": "ember_type",
            "description": "Type text into the focused pane's shell. Use \\n for Enter.",
            "inputSchema": {"type": "object", "properties": with(target_props(), "text", json!({"type": "string", "description": "Text to type (\\n = Enter)."})), "required": ["text"]}
        },
        {
            "name": "ember_key",
            "description": "Press a named key in the focused pane: Enter, Tab, Escape, Backspace, Space, Up/Down/Left/Right.",
            "inputSchema": {"type": "object", "properties": with(target_props(), "name", json!({"type": "string"})), "required": ["name"]}
        },
        {
            "name": "ember_chord",
            "description": "Run a multiplexer chord, e.g. cmd+d (split), cmd+shift+d, cmd+w, cmd+t, cmd+arrowright, cmd+1.",
            "inputSchema": {"type": "object", "properties": with(target_props(), "keys", json!({"type": "string"})), "required": ["keys"]}
        },
        {
            "name": "ember_screenshot",
            "description": "Render a deterministic scene to a PNG headlessly (no display) and return the path to read. Args: path, scale (default 2), split (h|v), tabs (int), run (shell command).",
            "inputSchema": {"type": "object", "properties": {
                "path": {"type": "string"},
                "scale": {"type": "number"},
                "split": {"type": "string", "enum": ["h", "v"]},
                "tabs": {"type": "integer"},
                "run": {"type": "string"}
            }}
        },
        {
            "name": "ember_live_screenshot",
            "description": "Capture the CURRENT on-screen state of a running ember-term to a PNG (pixel-identical to the window) and return the path to read. Args: path (default /tmp/ember-live.png), plus pid/sock to target an instance.",
            "inputSchema": {"type": "object", "properties": with(target_props(), "path", json!({"type": "string"}))}
        },
        {
            "name": "ember_move_tab",
            "description": "Move the focused tab to another window: a brand-new one, an existing 1-based window number, or the window next/previous in creation order. Returns {ok:true} or {ok:false,error}.",
            "inputSchema": {"type": "object", "properties": with(target_props(), "to", json!({"type": "string", "description": "new | next | prev | <1-based window number>"})), "required": ["to"]}
        },
        {
            "name": "ember_promote_pane",
            "description": "Promote the focused pane out of its current split: into its own new tab (same window) or its own brand-new window. Returns {ok:true} or {ok:false,error}.",
            "inputSchema": {"type": "object", "properties": with(target_props(), "to", json!({"type": "string", "enum": ["tab", "window"]})), "required": ["to"]}
        },
        {
            "name": "ember_merge_tab",
            "description": "Merge the focused tab into the tab immediately before it, as a horizontal split of that tab's focused pane. Returns {ok:true} or {ok:false,error} (e.g. no previous tab).",
            "inputSchema": {"type": "object", "properties": target_props()}
        }
    ])
}

/// Insert one property into a target-props object (small schema-builder helper).
fn with(mut props: Value, key: &str, schema: Value) -> Value {
    if let Some(obj) = props.as_object_mut() {
        obj.insert(key.to_string(), schema);
    }
    props
}

fn tools_call(params: Option<&Value>) -> Result<Value, String> {
    let params = params.ok_or("missing params")?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("missing tool name")?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match name {
        "ember_list" => {
            let listing: Vec<Value> = control::list_instances()
                .iter()
                .map(|(pid, path)| json!({"pid": pid, "sock": path.display().to_string()}))
                .collect();
            Ok(text(
                serde_json::to_string(&listing).unwrap_or_else(|_| "[]".into()),
            ))
        }
        "ember_state" => Ok(text(send(&args, json!({"cmd": "state"}))?)),
        "ember_focus" => {
            let q = args
                .get("query")
                .and_then(Value::as_str)
                .ok_or("query required")?;
            Ok(text(send(&args, json!({"cmd": "focus", "query": q}))?))
        }
        "ember_raise" => Ok(text(send(&args, json!({"cmd": "raise"}))?)),
        "ember_type" => {
            let t = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or("text required")?;
            Ok(text(send(&args, json!({"cmd": "type", "text": t}))?))
        }
        "ember_key" => {
            let k = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or("name required")?;
            Ok(text(send(&args, json!({"cmd": "key", "name": k}))?))
        }
        "ember_chord" => {
            let c = args
                .get("keys")
                .and_then(Value::as_str)
                .ok_or("keys required")?;
            Ok(text(send(&args, json!({"cmd": "chord", "keys": c}))?))
        }
        "ember_screenshot" => {
            let mut opts = screenshot::Opts::default();
            if let Some(p) = args.get("path").and_then(Value::as_str) {
                opts.path = p.to_string();
            }
            if let Some(s) = args.get("scale").and_then(Value::as_f64) {
                opts.scale = s as f32;
            }
            if let Some(s) = args.get("split").and_then(Value::as_str) {
                opts.split = match s {
                    "h" => Some(Axis::Horizontal),
                    "v" => Some(Axis::Vertical),
                    _ => None,
                };
            }
            if let Some(t) = args.get("tabs").and_then(Value::as_u64) {
                opts.tabs = (t as usize).max(1);
            }
            if let Some(r) = args.get("run").and_then(Value::as_str) {
                opts.runs = vec![r.to_string()];
            }
            let path = screenshot::run(opts)?;
            Ok(text(format!(
                "wrote {path} — read this file to view the render"
            )))
        }
        "ember_live_screenshot" => {
            // No default here: omitting `path` lets the server pick its
            // per-instance file under the owner-only control dir.
            let mut req = json!({"cmd": "screenshot"});
            if let Some(path) = args.get("path").and_then(Value::as_str) {
                req["path"] = json!(path);
            }
            Ok(text(send(&args, req)?))
        }
        "ember_move_tab" => {
            let to = args
                .get("to")
                .and_then(Value::as_str)
                .ok_or("to required")?;
            Ok(text(send(&args, json!({"cmd": "move-tab", "to": to}))?))
        }
        "ember_promote_pane" => {
            let to = args
                .get("to")
                .and_then(Value::as_str)
                .ok_or("to required")?;
            Ok(text(send(&args, json!({"cmd": "promote-pane", "to": to}))?))
        }
        "ember_merge_tab" => Ok(text(send(&args, json!({"cmd": "merge-tab"}))?)),
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Resolve the target instance from the tool args and send one request.
fn send(args: &Value, request: Value) -> Result<String, String> {
    let sock = args.get("sock").and_then(Value::as_str).map(String::from);
    let pid = args.get("pid").and_then(Value::as_u64).map(|p| p as u32);
    let socket = control::resolve_socket(sock, pid)?;
    control::send(&socket, &request)
}

/// Wrap a string as MCP text tool-result content.
fn text(s: impl Into<String>) -> Value {
    json!({ "content": [{"type": "text", "text": s.into()}] })
}
