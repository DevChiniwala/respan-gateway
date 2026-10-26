//! E2B (<https://e2b.dev>): code run in cloud sandboxes. Its control plane
//! at `https://api.e2b.app` creates (`POST /v2/sandboxes`), reconnects to
//! (`POST /v2/sandboxes/{id}/connect`), lists (`GET /v2/sandboxes`) and
//! kills (`DELETE /sandboxes/{id}`) sandboxes, the key in `X-API-Key`. Code
//! runs on the sandbox's own host, in the code-interpreter template's
//! Jupyter server (`POST https://49999-{id}.{domain}/execute`, answering in
//! NDJSON), with the sandbox's access tokens from create or connect and not
//! the API key. Sandboxes are billed per second until killed or expired;
//! reconnecting can extend their expiry, but cannot shorten it.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use respan_gateway_tool_vendor_kit::args::{
    object, optional_bool, optional_enum, optional_int, optional_str, required_str,
};
use respan_gateway_tool_vendor_kit::reqwest::Method;
use respan_gateway_tool_vendor_kit::{
    Api, Content, KeyIn, ToolError, ToolOutput, ToolSpec, ToolVendor, Tools, decode_base64,
};
use serde_json::{Map, Value, json};

/// E2B.
pub const VENDOR: ToolVendor = ToolVendor {
    id: "e2b",
    key_env: &["E2B_API_KEY"],
    key_in: KeyIn::Header("X-API-Key"),
    base_url: "https://api.e2b.app",
    tools: &E2bTools,
};

/// The template a new sandbox is made from: E2B's code interpreter.
pub const TEMPLATE: &str = "code-interpreter-v1";
/// The code interpreter's port on the sandbox's host.
pub const JUPYTER_PORT: u16 = 49999;
/// The sandbox lifetime requested at creation or reconnection.
pub const DEFAULT_TIMEOUT: u64 = 180;

/// The most of one stream or traceback a result quotes.
const CAP: usize = 20_000;

const LANGUAGES: &[&str] = &["python", "javascript", "typescript", "r", "java", "bash"];

/// E2B's tools: `run_code`, `list_sandboxes` and `kill_sandbox`.
#[derive(Debug, Clone, Copy)]
pub struct E2bTools;

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "run_code",
        description: "Run code in an E2B cloud sandbox (a Jupyter-style interpreter: state, \
            imports and files persist between runs in the same sandbox) and get its stdout, \
            stderr, results and error; charts and images come back as images. Without \
            `sandbox_id` it creates a new sandbox and returns its id: pass that id to later \
            calls to reuse it. New sandboxes live `timeout_seconds` (default 180) from \
            creation; reconnecting can extend but cannot shorten an existing expiry. \
            Execution uses part of that lifetime. Sandboxes are billed per second while \
            running: call `kill_sandbox` when done to stop billing sooner.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "code": {"type": "string", "description": "The code to run, as a notebook cell: the last expression's value is returned as the result."},
    "language": {"type": "string", "enum": ["python", "javascript", "typescript", "r", "java", "bash"], "default": "python", "description": "The code's language. Each language keeps its own state within a sandbox."},
    "sandbox_id": {"type": "string", "description": "A sandbox from an earlier run_code call, to run in it again (and extend its life). Omit to create a new sandbox."},
    "timeout_seconds": {"type": "integer", "minimum": 10, "maximum": 3600, "default": 180, "description": "Sandbox lifetime requested before running code, and the longest this run may take. Reusing a sandbox can extend but cannot shorten its expiry. Keep it short: running time is billed."},
    "allow_internet": {"type": "boolean", "default": true, "description": "For a new sandbox: whether its code may reach the internet."},
    "template": {"type": "string", "default": "code-interpreter-v1", "description": "For a new sandbox: the E2B template to make it from, one built on the code interpreter."}
  },
  "required": ["code"]
}"#,
    },
    ToolSpec {
        name: "list_sandboxes",
        description: "List the E2B account's sandboxes: id, template, state, start and expiry \
            time, CPUs and memory. Free; each running sandbox listed is being billed.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "state": {"type": "string", "enum": ["running", "paused"], "description": "Only sandboxes in this state."},
    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 100, "description": "At most this many, newest first."}
  }
}"#,
    },
    ToolSpec {
        name: "kill_sandbox",
        description: "Kill an E2B sandbox now, ending its billing. Its state and files are lost.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "sandbox_id": {"type": "string", "description": "The sandbox's id, as run_code or list_sandboxes gave it."}
  },
  "required": ["sandbox_id"]
}"#,
    },
];

#[async_trait]
impl Tools for E2bTools {
    fn list(&self) -> &'static [ToolSpec] {
        TOOLS
    }

    async fn call(&self, api: &Api, name: &str, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args = object(&arguments)?;
        match name {
            "run_code" => run_code(api, args).await,
            "list_sandboxes" => {
                let query = list_query(args)?;
                let req = api.request(Method::GET, "/v2/sandboxes").query(&query);
                Ok(render_list(&api.json(req).await?).into())
            }
            "kill_sandbox" => {
                let id = sandbox_id(args)?;
                api.json(api.request(Method::DELETE, &format!("/sandboxes/{id}")))
                    .await?;
                Ok(format!("Sandbox {id} killed.").into())
            }
            _ => Err(ToolError::new(format!("no tool named {name:?}"))),
        }
    }
}

/// A sandbox as create or connect describes it: where its host is, and the
/// tokens its host takes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sandbox {
    id: String,
    domain: String,
    envd_token: Option<String>,
    traffic_token: Option<String>,
}

/// What `run_code` was asked: the sandbox, its life, and the cell.
#[derive(Debug, Clone, PartialEq)]
struct Run {
    sandbox_id: Option<String>,
    timeout: u64,
    create: Value,
    execute: Value,
}

/// The run, with the `seconds` from creating or reconnecting its sandbox
/// to the run's answer: E2B bills a running sandbox per second
/// (<https://docs.e2b.dev/billing>) and reports no usage, so the call is
/// timed; the sandbox's life past the call bills outside it.
async fn run_code(api: &Api, args: &Map<String, Value>) -> Result<ToolOutput, ToolError> {
    let run = run_request(args)?;
    let since = Instant::now();
    let (response, created) = match &run.sandbox_id {
        Some(id) => {
            let path = format!("/v2/sandboxes/{id}/connect");
            let body = json!({"timeout": run.timeout});
            (api.post_json(&path, &body).await?, false)
        }
        None => (api.post_json("/v2/sandboxes", &run.create).await?, true),
    };
    let sandbox = sandbox_of(&response, &default_domain(api))?;
    let lines = match execute(api, &sandbox, &run).await {
        Ok(lines) => lines,
        Err(e) if created => {
            let path = format!("/sandboxes/{}", sandbox.id);
            let killed = api.json(api.request(Method::DELETE, &path)).await.is_ok();
            let then = if killed {
                "it was killed"
            } else {
                "it expires on its own"
            };
            return Err(ToolError::new(format!(
                "{e} (in new sandbox {}; {then})",
                sandbox.id
            )));
        }
        Err(e) => return Err(ToolError::new(format!("{e} (in sandbox {})", sandbox.id))),
    };
    let lives = if created {
        format!("new; lives {} s from creation", run.timeout)
    } else {
        format!(
            "reused; lives at least {} s from before this run",
            run.timeout
        )
    };
    let header = format!("Sandbox: {} ({lives}; kill when done)", sandbox.id);
    Ok(render_execution(&header, &lines)?.with_usage("seconds", since.elapsed().as_secs_f64()))
}

/// The cell sent to the sandbox's code interpreter, its NDJSON answer read
/// whole.
async fn execute(api: &Api, sandbox: &Sandbox, run: &Run) -> Result<String, ToolError> {
    // Not through `api.request`: the sandbox's host takes its own tokens,
    // and the API key has no business inside a sandbox. Its own timeout
    // replaces the client's.
    let mut req = (api.client())
        .post(execute_url(sandbox))
        .header("E2b-Sandbox-Id", &sandbox.id)
        .header("E2b-Sandbox-Port", JUPYTER_PORT.to_string())
        .timeout(Duration::from_secs(run.timeout))
        .json(&run.execute);
    if let Some(token) = &sandbox.traffic_token {
        req = req.header("E2B-Traffic-Access-Token", token);
    }
    if let Some(token) = &sandbox.envd_token {
        req = req.header("X-Access-Token", token);
    }
    api.text(req).await
}

fn execute_url(sandbox: &Sandbox) -> String {
    format!(
        "https://{JUPYTER_PORT}-{}.{}/execute",
        sandbox.id, sandbox.domain
    )
}

/// The sandboxes' domain when create doesn't say: the API's host without
/// its `api.`, so a moved base URL moves it too.
fn default_domain(api: &Api) -> String {
    let url = api.url("");
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or_default()
        .split(['/', ':'])
        .next()
        .unwrap_or_default();
    match host.strip_prefix("api.") {
        Some(domain) if !domain.is_empty() => domain.to_owned(),
        _ => "e2b.app".to_owned(),
    }
}

fn sandbox_id(args: &Map<String, Value>) -> Result<&str, ToolError> {
    let id = required_str(args, "sandbox_id")?;
    checked_id(id)
}

fn checked_id(id: &str) -> Result<&str, ToolError> {
    if id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Ok(id)
    } else {
        Err(ToolError::new(format!(
            "`sandbox_id` is not a sandbox id: {id:?}"
        )))
    }
}

/// `run_code`'s requests from the tool's arguments.
fn run_request(args: &Map<String, Value>) -> Result<Run, ToolError> {
    let code = required_str(args, "code")?;
    let sandbox_id = match optional_str(args, "sandbox_id")? {
        Some(id) => Some(checked_id(id)?.to_owned()),
        None => None,
    };
    let timeout = optional_int(args, "timeout_seconds", 10, 3600)?.unwrap_or(DEFAULT_TIMEOUT);
    let mut execute = Map::new();
    execute.insert("code".into(), code.into());
    if let Some(v) = optional_enum(args, "language", LANGUAGES)? {
        execute.insert("language".into(), v.into());
    }
    let mut create = Map::new();
    let template = optional_str(args, "template")?.unwrap_or(TEMPLATE);
    create.insert("templateID".into(), template.into());
    create.insert("timeout".into(), timeout.into());
    if let Some(v) = optional_bool(args, "allow_internet")? {
        create.insert("allow_internet_access".into(), v.into());
    }
    create.insert("metadata".into(), json!({"created_by": "respan-gateway"}));
    Ok(Run {
        sandbox_id,
        timeout,
        create: Value::Object(create),
        execute: Value::Object(execute),
    })
}

/// `GET /v2/sandboxes`'s query from the tool's arguments.
fn list_query(args: &Map<String, Value>) -> Result<Vec<(&'static str, String)>, ToolError> {
    let mut q = Vec::new();
    if let Some(v) = optional_enum(args, "state", &["running", "paused"])? {
        q.push(("state", v.to_owned()));
    }
    if let Some(v) = optional_int(args, "limit", 1, 100)? {
        q.push(("limit", v.to_string()));
    }
    Ok(q)
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// The sandbox create or connect answered with.
fn sandbox_of(response: &Value, default_domain: &str) -> Result<Sandbox, ToolError> {
    let id = str_of(response, "sandboxID")
        .ok_or_else(|| ToolError::new("E2B answered with no sandbox id"))?;
    Ok(Sandbox {
        id: checked_id(id)?.to_owned(),
        domain: str_of(response, "domain")
            .unwrap_or(default_domain)
            .to_owned(),
        envd_token: str_of(response, "envdAccessToken").map(str::to_owned),
        traffic_token: str_of(response, "trafficAccessToken").map(str::to_owned),
    })
}

/// The sandboxes as JSON, with the fields worth reading.
fn render_list(response: &Value) -> String {
    const KEEP: &[(&str, &str)] = &[
        ("sandboxID", "sandbox_id"),
        ("alias", "template"),
        ("templateID", "template_id"),
        ("state", "state"),
        ("startedAt", "started_at"),
        ("endAt", "ends_at"),
        ("cpuCount", "cpus"),
        ("memoryMB", "memory_mb"),
        ("metadata", "metadata"),
    ];
    let sandboxes: Vec<Value> = response
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|s| {
            let kept: Map<String, Value> = KEEP
                .iter()
                .filter_map(|(from, to)| {
                    s.get(*from)
                        .filter(|v| !v.is_null())
                        .map(|v| ((*to).to_owned(), v.clone()))
                })
                .collect();
            Value::Object(kept)
        })
        .collect();
    if sandboxes.is_empty() {
        return "No sandboxes.".into();
    }
    serde_json::to_string_pretty(&sandboxes).unwrap_or_default()
}

/// A run's NDJSON as the model reads it: the header, the streams, each
/// result's text and images, and the error. A run that raised is a
/// [`ToolError`], its output kept.
fn render_execution(header: &str, ndjson: &str) -> Result<ToolOutput, ToolError> {
    let (mut stdout, mut stderr) = (String::new(), String::new());
    let mut results = Vec::new();
    let mut images = Vec::new();
    let mut error = None;
    for msg in ndjson
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        match str_of(&msg, "type") {
            Some("stdout") => stdout.push_str(str_of(&msg, "text").unwrap_or_default()),
            Some("stderr") => stderr.push_str(str_of(&msg, "text").unwrap_or_default()),
            Some("result") => {
                let main = msg.get("is_main_result") == Some(&Value::Bool(true));
                let mut shown = Vec::new();
                for (key, mime) in [("png", "image/png"), ("jpeg", "image/jpeg")] {
                    if let Some(data) = str_of(&msg, key).and_then(decode_base64) {
                        shown.push(format!("[{mime} image {}]", images.len() + 1));
                        images.push(Content::Image {
                            data,
                            mime_type: mime.into(),
                        });
                    }
                }
                let text = ["text", "markdown"]
                    .iter()
                    .find_map(|k| str_of(&msg, k))
                    .map(str::to_owned)
                    .or_else(|| {
                        let other: Vec<&str> = ["html", "svg", "latex", "json", "pdf"]
                            .into_iter()
                            .filter(|k| msg.get(*k).is_some_and(|v| !v.is_null()))
                            .collect();
                        (shown.is_empty() && !other.is_empty())
                            .then(|| format!("(a result as {})", other.join(", ")))
                    });
                shown.extend(text);
                if !shown.is_empty() {
                    let label = if main { "Result" } else { "Display" };
                    results.push(format!("{label}:\n{}", cap(&shown.join("\n"))));
                }
            }
            Some("error") => {
                let mut e = format!(
                    "{}: {}",
                    str_of(&msg, "name").unwrap_or("Error"),
                    str_of(&msg, "value").unwrap_or_default()
                );
                if let Some(tb) = str_of(&msg, "traceback") {
                    e.push('\n');
                    e.push_str(&cap(&strip_ansi(tb)));
                }
                error = Some(e);
            }
            Some("unexpected_end_of_execution") if error.is_none() => {
                error = Some("The run ended unexpectedly, before finishing.".into());
            }
            _ => {}
        }
    }
    let mut parts = vec![header.to_owned()];
    if !stdout.is_empty() {
        parts.push(format!("stdout:\n{}", cap(stdout.trim_end())));
    }
    if !stderr.is_empty() {
        parts.push(format!("stderr:\n{}", cap(stderr.trim_end())));
    }
    parts.extend(results);
    if let Some(e) = error {
        parts.push(format!("Error: {e}"));
        return Err(ToolError::new(parts.join("\n\n")));
    }
    if parts.len() == 1 {
        parts.push("(no output)".into());
    }
    let mut blocks = vec![Content::Text(parts.join("\n\n"))];
    blocks.extend(images);
    Ok(blocks.into())
}

/// `text` up to [`CAP`] bytes, cut at a character.
fn cap(text: &str) -> String {
    if text.len() <= CAP {
        return text.to_owned();
    }
    let mut end = CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes more)", &text[..end], text.len() - end)
}

/// `text` without its terminal escape sequences, which tracebacks are
/// coloured with.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => Map::new(),
        }
    }

    fn text_of(output: &ToolOutput) -> &str {
        match output.content.first() {
            Some(Content::Text(t)) => t,
            _ => "",
        }
    }

    #[test]
    fn run_request_maps_arguments() {
        let run = run_request(&args(json!({
            "code": "1 + 1", "language": "r", "timeout_seconds": 60, "allow_internet": false
        })))
        .expect("valid");
        assert_eq!(
            run,
            Run {
                sandbox_id: None,
                timeout: 60,
                create: json!({
                    "templateID": "code-interpreter-v1", "timeout": 60,
                    "allow_internet_access": false,
                    "metadata": {"created_by": "respan-gateway"}
                }),
                execute: json!({"code": "1 + 1", "language": "r"}),
            }
        );
        let reuse = run_request(&args(json!({"code": "x", "sandbox_id": "i1a2b3"}))).expect("ok");
        assert_eq!(reuse.sandbox_id.as_deref(), Some("i1a2b3"));
        assert_eq!(reuse.timeout, DEFAULT_TIMEOUT);
        assert_eq!(reuse.execute, json!({"code": "x"}));
    }

    #[test]
    fn run_request_rejects() {
        assert!(run_request(&args(json!({}))).is_err());
        assert!(run_request(&args(json!({"code": " "}))).is_err());
        assert!(run_request(&args(json!({"code": "x", "language": "cobol"}))).is_err());
        assert!(run_request(&args(json!({"code": "x", "timeout_seconds": 5}))).is_err());
        assert!(run_request(&args(json!({"code": "x", "sandbox_id": "a/../b"}))).is_err());
        assert!(sandbox_id(&args(json!({}))).is_err());
        assert!(list_query(&args(json!({"state": "dead"}))).is_err());
        assert_eq!(
            list_query(&args(json!({"state": "running", "limit": 5}))).expect("ok"),
            vec![("state", "running".to_owned()), ("limit", "5".to_owned())]
        );
    }

    #[test]
    fn reads_the_sandbox_and_its_host() {
        let sandbox = sandbox_of(
            &json!({"templateID": "nlhz8vlwyupq845jsdg9", "sandboxID": "ixq2p3", "alias": "code-interpreter-v1",
                "clientID": "6532622b", "envdVersion": "0.2.0", "envdAccessToken": "tok", "domain": null}),
            "e2b.app",
        )
        .expect("a sandbox");
        assert_eq!(
            sandbox,
            Sandbox {
                id: "ixq2p3".into(),
                domain: "e2b.app".into(),
                envd_token: Some("tok".into()),
                traffic_token: None,
            }
        );
        assert_eq!(
            execute_url(&sandbox),
            "https://49999-ixq2p3.e2b.app/execute"
        );
        assert!(sandbox_of(&json!({}), "e2b.app").is_err());
        let api = |base: &str| Api::new(base, "k", KeyIn::Header("X-API-Key"));
        assert_eq!(default_domain(&api("https://api.e2b.app")), "e2b.app");
        assert_eq!(default_domain(&api("https://api.e2b.dev/")), "e2b.dev");
        assert_eq!(default_domain(&api("http://localhost:3000")), "e2b.app");
    }

    #[test]
    fn renders_an_execution() {
        let png = "iVBORw0KGgo=";
        let ndjson = [
            json!({"type": "number_of_executions", "execution_count": 1}),
            json!({"type": "stdout", "text": "hello\n", "timestamp": 1}),
            json!({"type": "stderr", "text": "warn\n", "timestamp": 2}),
            json!({"type": "result", "png": png, "text": "<Figure size 640x480 with 1 Axes>", "is_main_result": false}),
            json!({"type": "result", "text": "2", "is_main_result": true}),
            json!({"type": "end_of_execution"}),
        ]
        .map(|v| v.to_string())
        .join("\n");
        let out = render_execution("Sandbox: s", &ndjson).expect("ran");
        assert_eq!(
            text_of(&out),
            "Sandbox: s\n\nstdout:\nhello\n\nstderr:\nwarn\n\nDisplay:\n[image/png image 1]\n\
             <Figure size 640x480 with 1 Axes>\n\nResult:\n2"
        );
        assert_eq!(
            out.content.get(1),
            Some(&Content::Image {
                data: b"\x89PNG\r\n\x1a\n".to_vec(),
                mime_type: "image/png".into()
            })
        );
        let quiet = render_execution("Sandbox: s", r#"{"type": "end_of_execution"}"#).expect("ran");
        assert_eq!(text_of(&quiet), "Sandbox: s\n\n(no output)");
    }

    #[test]
    fn a_raised_error_fails_the_call() {
        let ndjson = [
            json!({"type": "stdout", "text": "before\n"}),
            json!({"type": "error", "name": "ZeroDivisionError", "value": "division by zero",
                "traceback": "\u{1b}[0;31mZeroDivisionError\u{1b}[0m: division by zero"}),
        ]
        .map(|v| v.to_string())
        .join("\n");
        assert_eq!(
            render_execution("Sandbox: s", &ndjson),
            Err(ToolError::new(
                "Sandbox: s\n\nstdout:\nbefore\n\nError: ZeroDivisionError: division by zero\n\
                 ZeroDivisionError: division by zero"
            ))
        );
    }

    #[test]
    fn renders_a_list() {
        let text = render_list(&json!([{
            "templateID": "nlhz8vlwyupq845jsdg9", "alias": "code-interpreter-v1", "sandboxID": "ixq2p3",
            "clientID": "6532622b", "startedAt": "2025-01-01T00:00:00Z", "endAt": "2025-01-01T00:03:00Z",
            "cpuCount": 2, "memoryMB": 512, "diskSizeMB": 1024, "state": "running", "envdVersion": "0.2.0",
            "metadata": {"created_by": "respan-gateway"}
        }]));
        let parsed: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(
            parsed,
            json!([{
                "sandbox_id": "ixq2p3", "template": "code-interpreter-v1",
                "template_id": "nlhz8vlwyupq845jsdg9", "state": "running",
                "started_at": "2025-01-01T00:00:00Z", "ends_at": "2025-01-01T00:03:00Z",
                "cpus": 2, "memory_mb": 512, "metadata": {"created_by": "respan-gateway"}
            }])
        );
        assert_eq!(render_list(&json!([])), "No sandboxes.");
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\u{1b}[1;32mok\u{1b}[0m"), "ok");
    }
}
