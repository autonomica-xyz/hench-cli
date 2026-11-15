//! User-installed script tools: a directory `<data_dir>/scripts/<dir>/`
//! containing `manifest.json` + `script.py` becomes one MCP tool, enabled
//! only when its manifest `name` appears in the worker's enabled-script set
//! (Android parity: `ToolPackageManager` + `ScriptTool`).
//!
//! manifest.json (same contract as the Android app):
//! ```json
//! {
//!   "name": "my_tool",                    // [a-z0-9_]+, the MCP tool name
//!   "description": "...",                 // shown in tools/list
//!   "inputSchema": { ... JSON Schema ... },
//!   "priceSats": 5,                       // flat per-call price, whole sats
//!   "output": "text" | "file",            // stdout text | file at argv path
//!   "timeoutSecs": 120                    // optional, default 120, max 600
//! }
//! ```
//!
//! Script contract (desktop variant of the Android one):
//!   stdin   = JSON-encoded tool arguments (Android passed them as argv[1];
//!             stdin has no ARG_MAX ceiling and is friendlier to `json.load`)
//!   argv[1] = output file path (only when output == "file"; upload the file
//!             the script writes there — returned as a resource_link)
//!   stdout  = the tool result text (when output == "text")
//!   exit 0  = success, anything else = failure (stderr tail in the error)
//!
//! Trust model (v1, deliberate — same as Android): a script runs as the
//! worker's user with full host access — files, network, the works.
//! Installing/enabling a script tool is like installing an app: only enable
//! packages you trust. Manifests describe; they do not confine.
use crate::config::Config;
use crate::store;
use crate::tools::{mcp_success, tool_error, QuoteDecision, Tool, ToolCtx};
use contextvm_sdk::core::types::JsonRpcMessage;
use contextvm_sdk::payments::types::PricedCapability;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// Text results longer than this are uploaded to Blossom instead of being
/// truncated away (Android simply took the first 100k chars; the CLI keeps
/// the whole output reachable).
const MAX_INLINE_CHARS: usize = 100_000;
/// Default per-call timeout, matching the Android manifest default.
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// stderr tail kept in SCRIPT_ERROR messages (Android: take(500)).
const STDERR_TAIL: usize = 500;

/// A parsed + validated `manifest.json` (port of ScriptToolManifest.parse).
#[derive(Debug, Clone)]
pub struct ScriptManifest {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Flat per-call price in whole sats (manifest `priceSats`, 1..=100_000).
    pub price_sats: i64,
    /// "text" (stdout is the result) or "file" (script writes argv[1]).
    pub output: String,
    pub timeout_secs: u64,
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Longest valid priceSats literal, mirroring Android's
/// `content.toLongOrNull()` on a JSON number or string.
fn price_sats(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok())),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

impl ScriptManifest {
    /// Parse and validate manifest JSON. `None` on any contract violation —
    /// callers skip the package (same as Android's runCatching parse).
    pub fn parse(text: &str) -> Option<Self> {
        let obj = serde_json::from_str::<Value>(text).ok()?;
        let obj = obj.as_object()?;
        let name = obj.get("name")?.as_str()?;
        let description = obj.get("description")?.as_str()?;
        let schema = obj.get("inputSchema")?.clone();
        let price = price_sats(obj.get("priceSats")?)?;
        let output = obj
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or("text")
            .to_string();
        let timeout = obj
            .get("timeoutSecs")
            .and_then(|v| match v {
                Value::Number(n) => n.as_i64().or_else(|| n.as_u64().map(|u| u as i64)),
                Value::String(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            })
            .map(|t| t.clamp(5, 600) as u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        if !valid_tool_name(name) {
            return None;
        }
        if description.trim().is_empty() {
            return None;
        }
        if !(1..=100_000).contains(&price) {
            return None;
        }
        if output != "text" && output != "file" {
            return None;
        }
        // Android additionally requires every `required` entry to be a
        // string; the schema itself is passed through verbatim.
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            if required.iter().any(|r| !r.is_string()) {
                return None;
            }
        }
        Some(Self {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: schema,
            price_sats: price,
            output,
            timeout_secs: timeout,
        })
    }

    /// Required argument names derived from the manifest schema
    /// (Android preflight: presence-only check, scripts own deeper
    /// validation).
    fn required_args(&self) -> Vec<String> {
        self.input_schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Process-global table backing `Tool::name() -> &'static str` for
/// dynamically discovered script names. Entries are append-only and never
/// removed or mutated, so a `String`'s heap buffer stays put for the life
/// of the process — the returned `&'static str` cannot dangle.
// shortcut: interner exists only because Tool::name() returns &'static str;
// if the trait ever loosens to `fn name(&self) -> &str`, delete this and
// store an owned String/Arc<str> instead.
fn intern_name(name: &str) -> &'static str {
    static NAMES: LazyLock<Mutex<HashMap<u64, String>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut table = NAMES.lock().unwrap();
    // Keyed by content hash: identical names share one entry.
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut h);
        h.finish()
    };
    let s = table.entry(key).or_insert_with(|| name.to_string());
    // SAFETY: `s` lives in the append-only interner for the process
    // lifetime; its buffer is never reallocated (no further writes to the
    // String) and the entry is never removed, so the pointer stays valid.
    unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(s.as_ptr(), s.len())) }
}

/// One enabled script package = one registered tool.
pub struct ScriptTool {
    manifest: ScriptManifest,
    /// `<data_dir>/scripts/<dir>/script.py`
    script: PathBuf,
    name: &'static str,
}

impl ScriptTool {
    fn new(manifest: ScriptManifest, script: PathBuf) -> Self {
        let name = intern_name(&manifest.name);
        Self {
            manifest,
            script,
            name,
        }
    }
}

/// Result of one script subprocess: exit status plus drained output.
struct ScriptOutput {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

/// `python3 script.py <out-path?>`, args JSON on stdin, hard timeout.
/// kill_on_drop + wait_with_output keeps pipes drained (no SIGPIPE-style
/// deadlock on chatty scripts) and guarantees the process is killed.
async fn run_script(
    script: &Path,
    args_json: &[u8],
    out_path: Option<&Path>,
    timeout: Duration,
) -> anyhow::Result<ScriptOutput> {
    let mut cmd = tokio::process::Command::new("python3");
    cmd.arg(script);
    if let Some(p) = out_path {
        cmd.arg(p);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn python3: {e} (is python3 installed?)"))?;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        // Broken pipe = the script exited before reading stdin; the exit
        // status reports the real failure.
        let _ = stdin.write_all(args_json).await;
    }
    // wait_with_output consumes the child; on timeout the child is dropped
    // (kill_on_drop kills it) — we can't reap it afterwards, so report
    // timed_out with no output. The pipes are already drained by
    // wait_with_output on the success path.
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(ScriptOutput {
            code: out.status.code(),
            stdout: out.stdout,
            stderr: out.stderr,
            timed_out: false,
        }),
        Ok(Err(e)) => Err(anyhow::anyhow!("script wait failed: {e}")),
        Err(_) => Ok(ScriptOutput {
            code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: true,
        }),
    }
}

fn stderr_tail(stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    s.trim()
        .chars()
        .rev()
        .take(STDERR_TAIL)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
}

#[async_trait::async_trait]
impl Tool for ScriptTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn descriptor(&self) -> Value {
        json!({
            "name": self.manifest.name,
            "description": self.manifest.description,
            "inputSchema": self.manifest.input_schema,
        })
    }

    fn priced_capability(&self, _cfg: &Config) -> Option<PricedCapability> {
        // Android prices every script tool at a flat manifest priceSats.
        Some(PricedCapability {
            method: "tools/call".into(),
            name: Some(self.manifest.name.clone()),
            amount: self.manifest.price_sats,
            max_amount: Some(self.manifest.price_sats),
            currency_unit: "sats".into(),
            description: Some(format!("Hench script tool {}", self.manifest.name)),
        })
    }

    async fn quote(&self, _cfg: &Config, args: &Value) -> QuoteDecision {
        // Android preflight: required-field presence check only.
        for key in self.manifest.required_args() {
            if args.get(&key).is_none() {
                return QuoteDecision::Reject {
                    message: format!("missing required argument '{key}'"),
                };
            }
        }
        QuoteDecision::Quote {
            amount: self.manifest.price_sats,
            description: format!("hench script {}", self.manifest.name),
            url: self.manifest.name.clone(),
            mode: "script".into(),
        }
    }

    async fn run(&self, ctx: ToolCtx<'_>) -> JsonRpcMessage {
        for key in self.manifest.required_args() {
            if ctx.args.get(&key).is_none() {
                return tool_error(
                    &ctx.id,
                    "INVALID_PARAMS",
                    &format!("missing required argument '{key}'"),
                );
            }
        }
        if !ctx.store.inflight_add(&ctx.canonical) {
            return tool_error(
                &ctx.id,
                "BUSY",
                "execution already in progress for this request",
            );
        }
        // Free mode never ran the quoter, so no WAITING record exists yet:
        // create it here (amount 0) so terminal results still cache/replay —
        // same contract as download_media.
        if ctx.store.get(&ctx.canonical).is_none() {
            if let Err(e) = ctx.store.mark_waiting(store::JobRecord {
                canonical_id: ctx.canonical.clone(),
                status: store::WAITING.into(),
                amount_msat: 0,
                url: self.manifest.name.clone(),
                mode: "script".into(),
                client: ctx.client_pubkey.clone(),
                result: None,
                tool_state: None,
                updated_at: 0,
            }) {
                ctx.store.inflight_remove(&ctx.canonical);
                return tool_error(
                    &ctx.id,
                    "INTERNAL",
                    &format!("job store unavailable: {e:#}"),
                );
            }
        }

        let args_json = serde_json::to_vec(&ctx.args).unwrap_or_else(|_| b"{}".to_vec());
        let job_id: String = ctx.canonical.chars().rev().take(16).collect();
        let work_dir = ctx.data_dir.join("jobs").join(format!("script-{job_id}"));
        let out_path = if self.manifest.output == "file" {
            let _ = std::fs::create_dir_all(&work_dir);
            Some(work_dir.join("output.bin"))
        } else {
            None
        };
        (ctx.progress)("RUNNING").await;

        let out = match run_script(
            &self.script,
            &args_json,
            out_path.as_deref(),
            Duration::from_secs(self.manifest.timeout_secs),
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                return self
                    .finish(
                        &ctx,
                        store::FAILED,
                        tool_error(&ctx.id, "SCRIPT_ERROR", &format!("{e:#}")),
                        Some(store::error_result("SCRIPT_ERROR", &format!("{e:#}"))),
                    )
                    .await;
            }
        };
        let _ = std::fs::remove_dir_all(&work_dir);

        if out.timed_out {
            let msg = format!("script exceeded {}s", self.manifest.timeout_secs);
            return self
                .finish(
                    &ctx,
                    store::FAILED,
                    tool_error(&ctx.id, "TIMEOUT", &msg),
                    Some(store::error_result("TIMEOUT", &msg)),
                )
                .await;
        }
        if out.code != Some(0) {
            let msg = format!(
                "script exited {}: {}",
                out.code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
                stderr_tail(&out.stderr)
            );
            return self
                .finish(
                    &ctx,
                    store::FAILED,
                    tool_error(&ctx.id, "SCRIPT_ERROR", &msg),
                    Some(store::error_result("SCRIPT_ERROR", &msg)),
                )
                .await;
        }

        if self.manifest.output == "file" {
            let Some(path) = out_path else {
                return self.finish_spawn_error(ctx, "script produced no output file").await;
            };
            match std::fs::metadata(&path) {
                Ok(m) if m.len() > 0 => {}
                _ => {
                    return self
                        .finish_spawn_error(ctx, "script produced no output file")
                        .await
                }
            };
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    return self
                        .finish_spawn_error(ctx, &format!("cannot read script output: {e}"))
                        .await
                }
            };
            let mime = crate::media::mime_for(&path).to_string();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("bin")
                .to_string();
            return match crate::media::upload_bytes(&ctx, bytes, &mime, &ext).await {
                Ok(up) => {
                    let body = json!({
                        "url": up.url,
                        "sha256": up.sha256,
                        "sizeBytes": up.size,
                        "mimeType": mime,
                    });
                    let result = json!({
                        "content": [
                            { "type": "text", "text": body.to_string() },
                            { "type": "resource_link", "uri": up.url, "name": format!("{}.{}", up.sha256.chars().take(16).collect::<String>(), ext), "mimeType": mime, "description": format!("Produced by hench script tool {}", self.manifest.name) }
                        ],
                        "isError": false
                    });
                    self.finish(&ctx, store::COMPLETED, mcp_success(&ctx.id, result.clone()), Some(result))
                        .await
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    self.finish(
                        &ctx,
                        store::FAILED,
                        tool_error(&ctx.id, "UPLOAD", &msg),
                        Some(store::error_result("UPLOAD", &msg)),
                    )
                    .await
                }
            };
        }

        // output == "text": stdout is the result. Trim like Android; when it
        // exceeds the inline cap, upload the whole output and return the
        // hosted URL instead of silently dropping the tail.
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if text.chars().count() > MAX_INLINE_CHARS {
            match crate::media::upload_bytes(&ctx, out.stdout.clone(), "text/plain", "txt").await {
                Ok(up) => {
                    let body = json!({
                        "url": up.url,
                        "sha256": up.sha256,
                        "sizeBytes": up.size,
                        "mimeType": "text/plain",
                        "truncated": true,
                    });
                    let result = json!({
                        "content": [
                            { "type": "text", "text": body.to_string() },
                            { "type": "resource_link", "uri": up.url, "name": "output.txt", "mimeType": "text/plain", "description": format!("Full output of hench script tool {} (exceeds inline limit)", self.manifest.name) }
                        ],
                        "isError": false
                    });
                    return self
                        .finish(&ctx, store::COMPLETED, mcp_success(&ctx.id, result.clone()), Some(result))
                        .await;
                }
                Err(e) => {
                    // Upload is best-effort here: the client still gets the
                    // capped inline text rather than a hard failure.
                    tracing::warn!("script output upload failed, serving inline cap only: {e:#}");
                }
            }
        }
        let capped: String = text.chars().take(MAX_INLINE_CHARS).collect();
        let result = json!({
            "content": [{ "type": "text", "text": capped }],
            "isError": false
        });
        self.finish(&ctx, store::COMPLETED, mcp_success(&ctx.id, result.clone()), Some(result))
            .await
    }
}

impl ScriptTool {
    /// SCRIPT_ERROR-shaped terminal failure for malformed script output.
    async fn finish_spawn_error(&self, ctx: ToolCtx<'_>, msg: &str) -> JsonRpcMessage {
        let err = tool_error(&ctx.id, "SCRIPT_ERROR", msg);
        let result = store::error_result("SCRIPT_ERROR", msg);
        self.finish(&ctx, store::FAILED, err, Some(result)).await
    }

    /// Terminal persistence + inflight release, then the client message.
    /// Mirrors download_media: a persist failure is logged loudly but the
    /// live request still gets its answer.
    async fn finish(
        &self,
        ctx: &ToolCtx<'_>,
        status: &str,
        msg: JsonRpcMessage,
        result: Option<Value>,
    ) -> JsonRpcMessage {
        if let Err(e) = ctx
            .store
            .mark_terminal(&ctx.canonical, status, result)
        {
            tracing::error!(
                "terminal record NOT persisted for {} — replay may re-invoice: {e:#}",
                ctx.canonical
            );
        }
        ctx.store.inflight_remove(&ctx.canonical);
        msg
    }
}

/// Discovers + instantiates enabled script tools under a scripts root
/// (`<data_dir>/scripts`). Port of ToolPackageManager.discoverDirs, filtered
/// to the enabled set — script tools are strictly opt-in (never advertised
/// unless the user enabled them, same as Android).
pub struct ScriptTools {
    enabled: Vec<String>,
}

impl ScriptTools {
    pub fn new(enabled: Vec<String>) -> Self {
        Self { enabled }
    }

    /// Scan `dir` for `*/manifest.json` + `script.py` packages; returns one
    /// tool per package whose manifest name is in the enabled set.
    /// Invalid manifests are skipped with a warning (Android parity).
    /// First package wins on manifest-name collision (dirs sorted by name).
    pub fn discover(&self, dir: &Path) -> Vec<Arc<dyn Tool>> {
        let mut dirs: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect(),
            Err(_) => return Vec::new(),
        };
        dirs.sort();
        let mut seen = std::collections::HashSet::new();
        let mut tools = Vec::new();
        for d in dirs {
            let manifest_path = d.join("manifest.json");
            let script = d.join("script.py");
            if !manifest_path.is_file() || !script.is_file() {
                continue;
            }
            let manifest = std::fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|t| ScriptManifest::parse(&t));
            let Some(manifest) = manifest else {
                tracing::warn!("invalid script manifest in {}, skipping", d.display());
                continue;
            };
            if !self.enabled.iter().any(|e| e == &manifest.name) {
                continue;
            }
            if !seen.insert(manifest.name.clone()) {
                continue;
            }
            tools.push(Arc::new(ScriptTool::new(manifest, script)) as Arc<dyn Tool>);
        }
        tools
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn manifest_json(name: &str) -> String {
        format!(
            r#"{{"name":"{name}","description":"test tool","inputSchema":{{"type":"object","required":["q"]}},"priceSats":5,"output":"text"}}"#
        )
    }

    #[test]
    fn parses_valid_manifest() {
        let m = ScriptManifest::parse(&manifest_json("my_tool")).unwrap();
        assert_eq!(m.name, "my_tool");
        assert_eq!(m.description, "test tool");
        assert_eq!(m.price_sats, 5);
        assert_eq!(m.output, "text");
        assert_eq!(m.timeout_secs, 120);
        assert_eq!(m.required_args(), vec!["q".to_string()]);
    }

    #[test]
    fn applies_defaults_and_clamps() {
        let m = ScriptManifest::parse(
            r#"{"name":"t","description":"d","inputSchema":{},"priceSats":1,"timeoutSecs":9999}"#,
        )
        .unwrap();
        assert_eq!(m.timeout_secs, 600);
        let m = ScriptManifest::parse(
            r#"{"name":"t","description":"d","inputSchema":{},"priceSats":1,"timeoutSecs":1}"#,
        )
        .unwrap();
        assert_eq!(m.timeout_secs, 5);
    }

    #[test]
    fn rejects_bad_manifests() {
        for bad in [
            "not json",
            r#"{"name":"Bad-Name","description":"d","inputSchema":{},"priceSats":1}"#,
            r#"{"name":"ok_name","description":"","inputSchema":{},"priceSats":1}"#,
            r#"{"name":"ok_name","description":"d","inputSchema":{},"priceSats":0}"#,
            r#"{"name":"ok_name","description":"d","inputSchema":{},"priceSats":100001}"#,
            r#"{"name":"ok_name","description":"d","inputSchema":{},"priceSats":1,"output":"xml"}"#,
            r#"{"name":"ok_name","description":"d","priceSats":1}"#, // no inputSchema
            r#"{"name":"ok_name","description":"d","inputSchema":{"required":[1]},"priceSats":1}"#,
        ] {
            assert!(ScriptManifest::parse(bad).is_none(), "accepted: {bad}");
        }
    }

    #[test]
    fn discovers_only_enabled_packages() {
        let dir = std::env::temp_dir().join(format!("hench-scripts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, enabled_name) in [("alpha", "alpha"), ("beta", "beta")] {
            let pkg = dir.join(name);
            std::fs::create_dir_all(&pkg).unwrap();
            std::fs::write(pkg.join("manifest.json"), manifest_json(enabled_name)).unwrap();
            std::fs::write(pkg.join("script.py"), "print(1)\n").unwrap();
        }
        // A third package that fails validation must be skipped.
        let bad = dir.join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("manifest.json"), "{}").unwrap();
        std::fs::write(bad.join("script.py"), "").unwrap();
        // And one missing script.py.
        let noscript = dir.join("noscript");
        std::fs::create_dir_all(&noscript).unwrap();
        std::fs::write(noscript.join("manifest.json"), manifest_json("noscript")).unwrap();

        let tools = ScriptTools::new(vec!["alpha".into(), "beta".into()]).discover(&dir);
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert_eq!(tools[0].descriptor()["name"], "alpha");
        assert_eq!(tools[0].descriptor()["description"], "test tool");
        assert_eq!(tools[0].descriptor()["inputSchema"]["required"][0], "q");

        // Disabled packages are not advertised.
        let tools = ScriptTools::new(vec!["alpha".into()]).discover(&dir);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "alpha");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_scripts_dir_discovers_nothing() {
        let tools = ScriptTools::new(vec!["x".into()])
            .discover(Path::new("/nonexistent/hench-scripts"));
        assert!(tools.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires python3 on PATH; hermetic CI may lack it"]
    async fn script_roundtrip_via_python3() {
        let dir = std::env::temp_dir().join(format!("hench-script-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("script.py");
        std::fs::write(&script, "import sys, json\nprint(json.load(sys.stdin)['q'])\n").unwrap();
        let out = run_script(&script, br#"{"q":"hello"}"#, None, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.code, Some(0));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
