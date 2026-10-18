//! The `translate_segment` tool: one bounded text segment translated through
//! a Sparehour-backed workload queue, with the translated file uploaded to
//! Blossom. Off by default — it only registers when the `HENCH_SPAREHOUR_*`
//! environment is fully configured (see [`SparehourConfig::from_env`]).
//!
//! Payment semantics mirror the Android app's paid jobs:
//!  - work is only sold when the backend can schedule it (the quote fails
//!    closed when the queue depth is invisible or the queue is busy);
//!  - the paid marker is persisted before the first backend call, so a job
//!    that is owed survives restarts (the startup sweep keeps it) and a
//!    repeat call is waived through the payment gate instead of re-invoiced;
//!  - a definitive failure after payment is a cached terminal error result —
//!    there are no refunds — while a merely slow or unreachable backend
//!    leaves the job resumable: repeat the exact same call to collect it.
//!
//! The Sparehour bearer token is a [`Secret`]: never logged, never printed,
//! never embedded in an error string, and only ever sent to the configured
//! base URL's origin (redirects are refused, artifact URIs are verified
//! path-relative before being joined).
use crate::config::Config;
use crate::state::EXECUTION_BUDGET_SECS;
use crate::store;
use crate::tools::{mcp_error, mcp_success, tool_error, QuoteDecision, Tool, ToolCtx};
use contextvm_sdk::core::types::JsonRpcMessage;
use contextvm_sdk::payments::types::PricedCapability;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

/// The Sparehour workload this tool drives.
const WORKLOAD: &str = "translate-segment";
/// Default route of the translate-segment workload.
const DEFAULT_ROUTE: &str = "libretranslate-v1";
/// Output artifact name and media type (workload output contract).
const OUTPUT_NAME: &str = "translation";
const OUTPUT_MIME: &str = "text/plain";
/// The workload caps the translation artifact at 2 MiB.
const MAX_ARTIFACT_BYTES: usize = 2_097_152;
/// One segment is at most this many Unicode characters (payload schema).
const MAX_TEXT_CHARS: usize = 20_000;
/// Control-plane requests (workloads, submit, poll).
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
/// Artifact download request.
const ARTIFACT_TIMEOUT: Duration = Duration::from_secs(30);
/// Returned while a paid job is neither done nor definitively failed.
const PENDING_MSG: &str = "translation is queued or the backend is briefly unavailable; repeat this exact call to collect the result (no new charge)";

/// Languages accepted for `source` ("auto" lets the service detect it).
const SOURCE_LANGUAGES: &[&str] = &[
    "auto", "en", "ar", "bg", "bn", "ca", "cs", "da", "de", "el", "es", "fa", "fr", "he", "hi",
    "hu", "id", "it", "ja", "ko", "lv", "nl", "pl", "pt", "ro", "ru", "sl", "sq", "sv", "sw", "th",
    "tr", "uk", "vi", "zh",
];
/// Languages accepted for `target` (explicit only, no "auto").
const TARGET_LANGUAGES: &[&str] = &[
    "en", "ar", "bg", "bn", "ca", "cs", "da", "de", "el", "es", "fa", "fr", "he", "hi", "hu", "id",
    "it", "ja", "ko", "lv", "nl", "pl", "pt", "ro", "ru", "sl", "sq", "sv", "sw", "th", "tr", "uk",
    "vi", "zh",
];

/// A bearer token that can never leak into output: `Debug` and `Display`
/// print `***`. Derive nothing that could print it (Clone is safe).
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The raw token — for the Authorization header only, never for logs or
    /// error strings.
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Environment keys read by [`SparehourConfig::from_env`].
const ENV_URL: &str = "HENCH_SPAREHOUR_URL";
const ENV_TOKEN: &str = "HENCH_SPAREHOUR_TOKEN";
const ENV_TOKEN_FILE: &str = "HENCH_SPAREHOUR_TOKEN_FILE";
const ENV_PROJECT: &str = "HENCH_SPAREHOUR_PROJECT";
const ENV_PRICE: &str = "HENCH_SPAREHOUR_PRICE_SATS";
const ENV_ROUTE: &str = "HENCH_SPAREHOUR_ROUTE";
const ENV_MAX_PENDING: &str = "HENCH_SPAREHOUR_MAX_PENDING";
const ENV_WAIT: &str = "HENCH_SPAREHOUR_WAIT_SECS";
const ENV_KEYS: &[&str] = &[
    ENV_URL,
    ENV_TOKEN,
    ENV_TOKEN_FILE,
    ENV_PROJECT,
    ENV_PRICE,
    ENV_ROUTE,
    ENV_MAX_PENDING,
    ENV_WAIT,
];

/// Connection + pricing parameters for the Sparehour control plane. Never
/// persisted to config.json and never shown in the dashboard; the token is
/// env/file-only and unprintable.
#[derive(Clone)]
pub struct SparehourConfig {
    /// API root (http/https enforced); every request path is joined onto it,
    /// so the token only ever travels to this origin.
    pub base_url: url::Url,
    pub token: Secret,
    pub project: String,
    /// Route of the translate-segment workload to submit to.
    pub route: String,
    /// Fixed price of one segment, in sats.
    pub price_sats: i64,
    /// Refuse to sell work above this queue depth.
    pub max_pending: u64,
    /// How long one call polls before returning PENDING (clamped below the
    /// execution budget so the reply always reaches the client).
    pub wait_secs: u64,
    /// Poll cadence while waiting for the backend.
    pub poll_interval_ms: u64,
}

impl SparehourConfig {
    /// Read the configuration from the process environment. `None` (silent)
    /// when no `HENCH_SPAREHOUR_*` variable is set at all; `None` plus one
    /// warning naming the offending variable names when it is partially set
    /// or invalid. Values are never logged.
    pub fn from_env() -> Option<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Testable core of [`SparehourConfig::from_env`]: `lookup` stands in
    /// for the environment (variable name -> value).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let value = |key: &str| {
            lookup(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        // Fully unconfigured: disabled without a word.
        if ENV_KEYS.iter().all(|k| value(k).is_none()) {
            return None;
        }

        let mut problems: Vec<&str> = vec![];
        let base_url = match value(ENV_URL).map(|raw| url::Url::parse(&raw)) {
            Some(Ok(mut u)) if u.scheme() == "http" || u.scheme() == "https" => {
                // Keep any path prefix (e.g. https://host/sparehour/): request
                // paths are joined relative to it, so it must end in '/'.
                if !u.path().ends_with('/') {
                    let path = format!("{}/", u.path());
                    u.set_path(&path);
                }
                Some(u)
            }
            Some(_) | None => {
                problems.push(ENV_URL);
                None
            }
        };
        // A direct token wins over the token file; both are trimmed.
        let token = value(ENV_TOKEN)
            .map(Secret::new)
            .or_else(|| {
                value(ENV_TOKEN_FILE).and_then(|path| {
                    std::fs::read_to_string(&path)
                        .ok()
                        .map(|c| c.trim().to_string())
                        .filter(|c| !c.is_empty())
                        .map(Secret::new)
                })
            })
            .or_else(|| {
                problems.push(ENV_TOKEN);
                None
            });
        let project = value(ENV_PROJECT).or_else(|| {
            problems.push(ENV_PROJECT);
            None
        });
        let price_sats = match value(ENV_PRICE).map(|v| v.parse::<i64>()) {
            Some(Ok(p)) if p > 0 => Some(p),
            Some(_) | None => {
                problems.push(ENV_PRICE);
                None
            }
        };
        let route = value(ENV_ROUTE).unwrap_or_else(|| DEFAULT_ROUTE.to_string());
        let max_pending = match value(ENV_MAX_PENDING) {
            None => 20,
            Some(v) => match v.parse::<u64>() {
                Ok(n) => n,
                Err(_) => {
                    problems.push(ENV_MAX_PENDING);
                    20
                }
            },
        };
        let wait_secs = match value(ENV_WAIT) {
            None => 240,
            Some(v) => match v.parse::<u64>() {
                Ok(s) => s,
                Err(_) => {
                    problems.push(ENV_WAIT);
                    240
                }
            },
        }
        // Always leave the client's reply window inside the budget.
        .min(EXECUTION_BUDGET_SECS.saturating_sub(60).max(1));

        if !problems.is_empty() {
            // Names only — never the values (the token is one of them).
            tracing::warn!(
                "translate_segment disabled: incomplete or invalid HENCH_SPAREHOUR_* configuration (check {})",
                problems.join(", ")
            );
            return None;
        }
        Some(Self {
            base_url: base_url.expect("emptiness pushed a problem"),
            token: token.expect("emptiness pushed a problem"),
            project: project.expect("emptiness pushed a problem"),
            price_sats: price_sats.expect("emptiness pushed a problem"),
            route,
            max_pending,
            wait_secs,
            poll_interval_ms: 2_000,
        })
    }
}

/// Locally validated `translate_segment` arguments (the workload's payload
/// schema, checked before quoting and again before running).
struct ValidatedArgs {
    text: String,
    source: String,
    target: String,
}

fn validate_args(args: &Value) -> Result<ValidatedArgs, String> {
    let Some(obj) = args.as_object() else {
        return Err("tools/call requires text, source, and target".into());
    };
    let extra: Vec<&str> = obj
        .keys()
        .filter(|k| !matches!(k.as_str(), "text" | "source" | "target"))
        .map(String::as_str)
        .collect();
    if !extra.is_empty() {
        return Err(format!(
            "unexpected arguments: {}; only text, source, and target are accepted",
            extra.join(", ")
        ));
    }
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call requires text, source, and target".to_string())?;
    if text.is_empty() {
        return Err("text must be a non-empty string".into());
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(format!("text must be at most {MAX_TEXT_CHARS} characters"));
    }
    let source = args
        .get("source")
        .and_then(Value::as_str)
        .filter(|s| SOURCE_LANGUAGES.contains(&s.to_ascii_lowercase().as_str()))
        .ok_or_else(|| {
            "source must be one of the supported languages ('auto' allowed)".to_string()
        })?;
    let target = args
        .get("target")
        .and_then(Value::as_str)
        .filter(|t| TARGET_LANGUAGES.contains(&t.to_ascii_lowercase().as_str()))
        .ok_or_else(|| {
            "target must be one of the supported languages and cannot be 'auto'".to_string()
        })?;
    Ok(ValidatedArgs {
        text: text.to_string(),
        source: source.to_ascii_lowercase(),
        target: target.to_ascii_lowercase(),
    })
}

/// Why the quote refused to sell translation work.
enum QueueError {
    /// Transport failure or non-2xx from the control plane.
    Unavailable,
    /// Workload or configured route absent.
    RouteMissing,
    /// Queue depth invisible: the token lacks the dynamic-scheduling perm.
    DepthHidden,
    /// Queue deeper than `max_pending`.
    Busy(u64),
}

/// Outcome of submitting a job to Sparehour.
enum SubmitOutcome {
    /// Submitted (or an idempotent replay of a prior submit); carries the
    /// job id.
    JobId(String),
    /// Definitive rejection (a 4xx the backend will give again): the job is
    /// dead — cached error result, no refund.
    Definitive(String),
    /// Transient (connect/timeout/5xx/429 or an unusable reply): keep the
    /// job resumable; the idempotency key prevents duplicate work.
    Transient,
}

/// The `translate_segment` tool.
#[derive(Clone)]
pub struct TranslateSegment {
    cfg: SparehourConfig,
    /// Dedicated client: no redirects (the token must never travel) and
    /// per-request timeouts, so a dead control plane fails fast into the
    /// resumable path.
    http: reqwest::Client,
}

impl TranslateSegment {
    pub fn new(cfg: SparehourConfig) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONTROL_TIMEOUT)
            .build()
            .expect("sparehour http client");
        Self { cfg, http }
    }

    fn join_base(&self, path: &str) -> Result<url::Url, String> {
        self.cfg
            .base_url
            .join(path.trim_start_matches('/'))
            .map_err(|_| "unusable backend request path".to_string())
    }

    /// GET /v1/workloads and decide whether work can be scheduled on the
    /// configured route right now.
    async fn check_queue(&self) -> Result<(), QueueError> {
        let url = self
            .join_base("/v1/workloads")
            .map_err(|_| QueueError::Unavailable)?;
        let resp = self
            .http
            .get(url)
            .bearer_auth(self.cfg.token.expose())
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .map_err(|_| QueueError::Unavailable)?;
        if !resp.status().is_success() {
            return Err(QueueError::Unavailable);
        }
        let docs: Value = resp.json().await.map_err(|_| QueueError::Unavailable)?;
        let route = docs
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|d| d.get("name").and_then(Value::as_str) == Some(WORKLOAD))
            })
            .and_then(|d| d.get("routes"))
            .and_then(|r| r.get(&self.cfg.route));
        let Some(route) = route else {
            return Err(QueueError::RouteMissing);
        };
        // Fail closed: without `perms=dynamic` the API omits pending_jobs and
        // this worker must not sell work it cannot schedule.
        let Some(depth) = route.get("pending_jobs").and_then(Value::as_u64) else {
            return Err(QueueError::DepthHidden);
        };
        if depth > self.cfg.max_pending {
            return Err(QueueError::Busy(depth));
        }
        Ok(())
    }

    /// POST /v1/jobs with the call's canonical id as the idempotency key, so
    /// a retry of this exact paid call can never create a second job.
    async fn submit(&self, canonical: &str, args: &ValidatedArgs) -> SubmitOutcome {
        let Ok(url) = self.join_base("/v1/jobs") else {
            return SubmitOutcome::Transient;
        };
        let body = json!({
            "project_id": self.cfg.project,
            "workload": WORKLOAD,
            "route": self.cfg.route,
            "payload": { "text": args.text, "source": args.source, "target": args.target },
            "priority": 0,
        });
        let resp = self
            .http
            .post(url)
            .header("Idempotency-Key", canonical)
            .bearer_auth(self.cfg.token.expose())
            .timeout(CONTROL_TIMEOUT)
            .json(&body)
            .send()
            .await;
        let Ok(resp) = resp else {
            return SubmitOutcome::Transient;
        };
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let doc: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            match doc.get("id").and_then(Value::as_str) {
                // A server-supplied id that could smuggle path components
                // into a later request URL is unusable: treat as transient
                // (the idempotency key keeps a retry single-job).
                Some(id) if is_safe_id(id) => SubmitOutcome::JobId(id.to_string()),
                _ => SubmitOutcome::Transient,
            }
        } else if matches!(status, 400 | 401 | 403 | 404 | 409 | 422) {
            let detail = doc
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            SubmitOutcome::Definitive(format!("{status}: {detail}"))
        } else {
            SubmitOutcome::Transient
        }
    }

    /// GET /v1/jobs/{id}.
    async fn fetch_job(&self, job_id: &str) -> Result<Value, String> {
        let url = self.join_base(&format!("/v1/jobs/{job_id}"))?;
        let resp = self
            .http
            .get(url)
            .bearer_auth(self.cfg.token.expose())
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("job status fetch failed: {}", e.without_url()))?;
        if !resp.status().is_success() {
            return Err(format!(
                "job status fetch failed: HTTP {}",
                resp.status().as_u16()
            ));
        }
        resp.json()
            .await
            .map_err(|e| format!("job status parse failed: {}", e.without_url()))
    }

    /// Download and verify the `translation` output of a succeeded job.
    async fn fetch_artifact(&self, doc: &Value, job_id: &str) -> Result<Vec<u8>, String> {
        let artifact = doc
            .get("outputs")
            .and_then(Value::as_array)
            .and_then(|a| {
                a.iter()
                    .find(|o| o.get("name").and_then(Value::as_str) == Some(OUTPUT_NAME))
            })
            .ok_or_else(|| "job has no translation output".to_string())?;
        let uri = artifact
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| "translation artifact has no uri".to_string())?;
        // Path-relative to this job only, then joined onto the configured
        // base — the token never travels anywhere else.
        let prefix = format!("/v1/jobs/{job_id}/artifacts/");
        if !uri.starts_with(&prefix) {
            return Err("unexpected translation artifact uri".into());
        }
        if let Some(size) = artifact.get("size").and_then(Value::as_u64) {
            if size as usize > MAX_ARTIFACT_BYTES {
                return Err(format!(
                    "translation artifact exceeds {MAX_ARTIFACT_BYTES} bytes"
                ));
            }
        }
        let expected = artifact
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| "translation artifact has no sha256".to_string())?
            .to_lowercase();
        let url = self
            .join_base(uri)
            .map_err(|_| "unusable translation artifact uri".to_string())?;
        let resp = self
            .http
            .get(url)
            .bearer_auth(self.cfg.token.expose())
            .timeout(ARTIFACT_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("artifact download failed: {}", e.without_url()))?;
        if !resp.status().is_success() {
            return Err(format!(
                "artifact download failed: HTTP {}",
                resp.status().as_u16()
            ));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("artifact download failed: {}", e.without_url()))?;
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(format!(
                "translation artifact exceeds {MAX_ARTIFACT_BYTES} bytes"
            ));
        }
        let digest = hex::encode(Sha256::digest(&bytes));
        if digest != expected {
            return Err("translation artifact checksum mismatch".into());
        }
        Ok(bytes.to_vec())
    }

    /// Succeeded job: persist the artifact locally, upload to Blossom, cache
    /// the terminal result.
    async fn finish(&self, ctx: ToolCtx<'_>, doc: &Value, job_id: &str) -> JsonRpcMessage {
        (ctx.progress)("UPLOADING").await;
        let bytes = match self.fetch_artifact(doc, job_id).await {
            Ok(b) => b,
            // Mirrors hench: a failure after payment is a cached terminal
            // error result — no refund, no re-run.
            Err(e) => return fail_terminal(&ctx, "UPLOAD", &e),
        };
        let out_dir = ctx.data_dir.join("jobs").join(sanitize(job_id));
        let file = out_dir.join("translation.txt");
        if let Err(e) = write_artifact(&out_dir, &file, &bytes).await {
            let _ = std::fs::remove_dir_all(&out_dir);
            return fail_terminal(&ctx, "UPLOAD", &e);
        }
        let upload =
            match crate::media::blossom_upload(ctx.http, &ctx.cfg.blossom, &file, ctx.keys).await {
                Ok(u) => u,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&out_dir);
                    return fail_terminal(&ctx, "UPLOAD", &e);
                }
            };
        let body = json!({
            "url": upload.url,
            "sha256": upload.sha256,
            "sizeBytes": upload.size,
            "mimeType": OUTPUT_MIME
        });
        let result = json!({
            "content": [
                { "type": "text", "text": body.to_string() },
                { "type": "resource_link", "uri": upload.url, "name": "translation.txt", "mimeType": OUTPUT_MIME, "description": "Translated by hench translate_segment" }
            ],
            "isError": false
        });
        if let Err(e) =
            ctx.store
                .mark_terminal(&ctx.canonical, store::COMPLETED, Some(result.clone()))
        {
            tracing::error!(
                "COMPLETED record NOT persisted for {} — a replay may re-invoice: {e:#}",
                ctx.canonical
            );
        }
        ctx.store.inflight_remove(&ctx.canonical);
        let _ = std::fs::remove_dir_all(&out_dir);
        tracing::info!(
            "job done: translate_segment ({} bytes) -> {}",
            upload.size,
            upload.url
        );
        mcp_success(&ctx.id, result)
    }
}

#[async_trait::async_trait]
impl Tool for TranslateSegment {
    fn name(&self) -> &'static str {
        "translate_segment"
    }

    fn descriptor(&self) -> Value {
        json!({
            "name": "translate_segment",
            "description": "Translate one text segment (up to 20,000 characters) between two languages. Returns a hosted text file URL.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["text", "source", "target"],
                "properties": {
                    "text": { "type": "string", "minLength": 1, "maxLength": 20000 },
                    "source": { "type": "string", "enum": SOURCE_LANGUAGES },
                    "target": { "type": "string", "enum": TARGET_LANGUAGES }
                }
            }
        })
    }

    fn priced_capability(&self, _cfg: &Config) -> Option<PricedCapability> {
        Some(PricedCapability {
            method: "tools/call".into(),
            name: Some("translate_segment".into()),
            amount: self.cfg.price_sats,
            max_amount: Some(self.cfg.price_sats),
            currency_unit: "sats".into(),
            description: Some("Hench text translation".into()),
        })
    }

    async fn quote(&self, _cfg: &Config, args: &Value) -> QuoteDecision {
        let args = match validate_args(args) {
            Ok(a) => a,
            Err(message) => return QuoteDecision::Reject { message },
        };
        match self.check_queue().await {
            Err(QueueError::Unavailable) => QuoteDecision::Reject {
                message: "translation backend unavailable".into(),
            },
            Err(QueueError::RouteMissing) => QuoteDecision::Reject {
                message: "translation route not available".into(),
            },
            Err(QueueError::DepthHidden) => QuoteDecision::Reject {
                message: "worker cannot see the translation queue depth; refusing to sell work it cannot schedule".into(),
            },
            Err(QueueError::Busy(n)) => QuoteDecision::Reject {
                message: format!("translation queue is busy ({n} pending); try again later"),
            },
            Ok(()) => QuoteDecision::Quote {
                amount: self.cfg.price_sats,
                description: format!("hench translate_segment ({}->{})", args.source, args.target),
                // Never the text itself — the record must stay free of it.
                url: WORKLOAD.into(),
                mode: format!("{}->{}", args.source, args.target),
            },
        }
    }

    fn resumable(&self) -> bool {
        true
    }

    async fn run(&self, ctx: ToolCtx<'_>) -> JsonRpcMessage {
        let canonical = ctx.canonical.as_str();
        let args = match validate_args(&ctx.args) {
            Ok(a) => a,
            Err(message) => return mcp_error(&ctx.id, -32602, &message),
        };
        // Work that is already paid for (a resume/collect call) is owed and
        // is not blocked by hours of operation; new work is.
        let already_paid = ctx
            .store
            .get(canonical)
            .map(|r| r.tool_paid())
            .unwrap_or(false);
        if !already_paid && !ctx.cfg.hours.open_now() {
            return tool_error(
                &ctx.id,
                "CLOSED",
                "worker is outside its hours of operation",
            );
        }
        if !ctx.store.inflight_add(&ctx.canonical) {
            return tool_error(
                &ctx.id,
                "BUSY",
                "execution already in progress for this request",
            );
        }
        // Free mode never ran the quoter, so no WAITING record exists yet:
        // create it here (amount 0) so terminal results still cache/replay
        // and the dashboard sees the job — same contract as download_media.
        if ctx.store.get(&ctx.canonical).is_none() {
            if let Err(e) = ctx.store.mark_waiting(store::JobRecord {
                canonical_id: ctx.canonical.clone(),
                status: store::WAITING.into(),
                amount_msat: 0,
                url: WORKLOAD.into(),
                mode: format!("{}->{}", args.source, args.target),
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

        // Durable paid marker BEFORE any backend call: from here on this job
        // is owed — restarts must resume it, never re-invoice or sweep it.
        let mut tool_state = ctx
            .store
            .get(canonical)
            .and_then(|r| r.tool_state)
            .unwrap_or(Value::Null);
        if !store::tool_state_paid(Some(&tool_state)) {
            if let Err(e) = ctx.store.set_tool_state(canonical, json!({ "paid": true })) {
                ctx.store.inflight_remove(&ctx.canonical);
                return tool_error(
                    &ctx.id,
                    "INTERNAL",
                    &format!("job store unavailable: {e:#}"),
                );
            }
            tool_state = json!({ "paid": true });
        }

        // Submit unless a previous attempt already saved the backend job id
        // (then we resume by polling it — no second job, no second charge).
        let job_id = match tool_state.get("sparehour_job_id").and_then(Value::as_str) {
            Some(id) if is_safe_id(id) => id.to_string(),
            _ => {
                (ctx.progress)("SUBMITTING").await;
                match self.submit(canonical, &args).await {
                    SubmitOutcome::JobId(id) => {
                        // Persisting the id only guards against a re-submit;
                        // the idempotency key already prevents a duplicate
                        // job, so a persist failure is warned, not fatal.
                        if let Err(e) = ctx.store.set_tool_state(
                            canonical,
                            json!({ "paid": true, "sparehour_job_id": id }),
                        ) {
                            tracing::warn!(
                                "sparehour job id NOT persisted for {canonical} — a repeat call resubmits under the same idempotency key: {e:#}"
                            );
                        }
                        id
                    }
                    SubmitOutcome::Definitive(message) => {
                        return fail_terminal(&ctx, "BACKEND_REJECTED", &message);
                    }
                    SubmitOutcome::Transient => return pending(&ctx),
                }
            }
        };

        (ctx.progress)("QUEUED").await;
        let deadline = Instant::now() + Duration::from_secs(self.cfg.wait_secs);
        let poll_every = Duration::from_millis(self.cfg.poll_interval_ms);
        loop {
            if let Ok(doc) = self.fetch_job(&job_id).await {
                let state = doc.get("state").and_then(Value::as_str).unwrap_or("");
                match state {
                    "succeeded" => return self.finish(ctx, &doc, &job_id).await,
                    "dead-letter" | "cancelled" => {
                        let code = doc.get("error_code").and_then(Value::as_str).unwrap_or("");
                        let message = doc
                            .get("error_message")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        // The backend gave up after its retries: cached
                        // terminal error, no refund — same as the app.
                        return fail_terminal(
                            &ctx,
                            "BACKEND_FAILED",
                            &format!("{state}: {code}: {message}"),
                        );
                    }
                    // pending/leased/running (or unknown): keep waiting.
                    _ => {}
                }
            }
            // A transient poll error falls through and retries instead of
            // failing the paid job.
            if Instant::now() + poll_every > deadline {
                // The record stays WAITING with the paid marker and saved
                // job id: the quoter waives the next identical call and this
                // loop resumes polling — nothing re-submitted or re-charged.
                return pending(&ctx);
            }
            tokio::time::sleep(poll_every).await;
        }
    }
}

/// Cached terminal failure (mirror of hench's no-refund contract): the
/// payment record keeps its money, the error result is what replays serve.
fn fail_terminal(ctx: &ToolCtx<'_>, code: &str, message: &str) -> JsonRpcMessage {
    if let Err(p) = ctx.store.mark_terminal(
        &ctx.canonical,
        store::FAILED,
        Some(store::error_result(code, message)),
    ) {
        tracing::error!(
            "terminal record NOT persisted for {} — replay may re-invoice: {p:#}",
            ctx.canonical
        );
    }
    ctx.store.inflight_remove(&ctx.canonical);
    tool_error(&ctx.id, code, message)
}

/// Non-terminal exit: the job stays resumable (WAITING, paid) and the client
/// repeats the exact same call to collect it.
fn pending(ctx: &ToolCtx<'_>) -> JsonRpcMessage {
    ctx.store.inflight_remove(&ctx.canonical);
    tool_error(&ctx.id, "PENDING", PENDING_MSG)
}

async fn write_artifact(dir: &Path, file: &Path, bytes: &[u8]) -> Result<(), String> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("job dir create failed: {e}"))?;
    tokio::fs::write(file, bytes)
        .await
        .map_err(|e| format!("translation write failed: {e}"))
}

/// Backend job ids must stay a single safe path segment (they are joined
/// into request URLs and scratch dir names).
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// Same scratch-dir sanitization as download_media.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::quoter;
    use crate::tools::ToolRegistry;
    use axum::extract::{Path, State};
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::routing::{get, post, put};
    use axum::Json;
    use contextvm_sdk::core::types::JsonRpcRequest;
    use contextvm_sdk::payments::canonical::compute_canonical_invocation_identity;
    use contextvm_sdk::payments::traits::ResolvePrice;
    use contextvm_sdk::payments::types::{ResolvePriceParams, ResolvePriceResult};
    use futures::FutureExt;
    use nostr_sdk::prelude::Keys;
    use std::collections::{HashMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, RwLock};

    const TOKEN: &str = "sparehour-test-token-do-not-print";
    const CLIENT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    // ── in-process Sparehour mock (127.0.0.1:0, request-counted) ──────

    struct MockJob {
        id: String,
        state: String,
        error_code: String,
        error_message: String,
        outputs: Value,
    }

    struct MockSparehour {
        token: String,
        /// `None` omits pending_jobs (a token without perms=dynamic).
        depth: Mutex<Option<u64>>,
        /// State (and error) freshly created jobs start in.
        initial_state: Mutex<String>,
        initial_error: Mutex<(String, String)>,
        /// One-shot POST failures (status, detail), consumed first-in.
        post_failures: Mutex<VecDeque<(u16, String)>>,
        jobs: Mutex<Vec<MockJob>>,
        by_key: Mutex<HashMap<String, String>>,
        artifact: Vec<u8>,
        posts: AtomicUsize,
        post_keys: Mutex<Vec<String>>,
        post_auths: Mutex<Vec<String>>,
        polls: AtomicUsize,
        auth_failures: AtomicUsize,
    }

    impl MockSparehour {
        fn new() -> Self {
            Self {
                token: TOKEN.to_string(),
                depth: Mutex::new(Some(0)),
                initial_state: Mutex::new("running".into()),
                initial_error: Mutex::new((String::new(), String::new())),
                post_failures: Mutex::new(VecDeque::new()),
                jobs: Mutex::new(vec![]),
                by_key: Mutex::new(HashMap::new()),
                artifact: b"Hola, mundo.".to_vec(),
                posts: AtomicUsize::new(0),
                post_keys: Mutex::new(vec![]),
                post_auths: Mutex::new(vec![]),
                polls: AtomicUsize::new(0),
                auth_failures: AtomicUsize::new(0),
            }
        }

        fn artifact_sha(&self) -> String {
            hex::encode(Sha256::digest(&self.artifact))
        }

        fn outputs_for(&self, job_id: &str) -> Value {
            json!([{
                "name": OUTPUT_NAME,
                "uri": format!("/v1/jobs/{job_id}/artifacts/{}", self.artifact_sha()),
                "media_type": OUTPUT_MIME,
                "size": self.artifact.len(),
                "sha256": self.artifact_sha(),
            }])
        }

        fn doc(&self, job: &MockJob) -> Value {
            json!({
                "id": job.id,
                "project_id": "proj",
                "workload": WORKLOAD,
                "route": DEFAULT_ROUTE,
                "state": job.state,
                "outputs": job.outputs,
                "error_code": job.error_code,
                "error_message": job.error_message,
            })
        }

        fn set_state(&self, job_id: &str, state: &str) {
            let mut jobs = self.jobs.lock().unwrap();
            for job in jobs.iter_mut() {
                if job.id == job_id {
                    job.state = state.to_string();
                    if state == "succeeded"
                        && job.outputs.as_array().map(|a| a.is_empty()).unwrap_or(true)
                    {
                        job.outputs = self.outputs_for(job_id);
                    }
                }
            }
        }

        fn first_job_id(&self) -> String {
            self.jobs.lock().unwrap()[0].id.clone()
        }

        fn posts(&self) -> usize {
            self.posts.load(Ordering::SeqCst)
        }

        fn polls(&self) -> usize {
            self.polls.load(Ordering::SeqCst)
        }
    }

    fn bearer(headers: &HeaderMap) -> Option<String> {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    async fn workloads(State(st): State<Arc<MockSparehour>>) -> Json<Value> {
        let depth = *st.depth.lock().unwrap();
        let mut route = json!({ "compatibility": { "capability": "libretranslate" } });
        if let Some(n) = depth {
            route["pending_jobs"] = json!(n);
        }
        Json(
            json!([{ "name": WORKLOAD, "default_route": DEFAULT_ROUTE, "routes": { DEFAULT_ROUTE: route } }]),
        )
    }

    async fn submit_job(
        State(st): State<Arc<MockSparehour>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let auth = bearer(&headers);
        if auth.as_deref() != Some(format!("Bearer {}", st.token).as_str()) {
            st.auth_failures.fetch_add(1, Ordering::SeqCst);
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "detail": "missing or invalid bearer token" })),
            );
        }
        st.posts.fetch_add(1, Ordering::SeqCst);
        st.post_auths
            .lock()
            .unwrap()
            .push(auth.clone().unwrap_or_default());
        let key = headers
            .get("idempotency-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        st.post_keys.lock().unwrap().push(key.clone());
        if let Some((status, detail)) = st.post_failures.lock().unwrap().pop_front() {
            return (
                StatusCode::from_u16(status).unwrap(),
                Json(json!({ "detail": detail })),
            );
        }
        assert_eq!(body["project_id"], "proj");
        assert_eq!(body["workload"], WORKLOAD);
        assert_eq!(body["route"], DEFAULT_ROUTE);
        assert_eq!(body["priority"], 0);
        assert_eq!(body["payload"]["text"], "Hello, world.");
        assert_eq!(body["payload"]["source"], "en");
        assert_eq!(body["payload"]["target"], "es");
        // Sparehour's idempotency contract: the same key returns the SAME
        // job (201); this mock never varies the request for a given key.
        let existing = st.by_key.lock().unwrap().get(&key).cloned();
        if let Some(id) = existing {
            let jobs = st.jobs.lock().unwrap();
            let job = jobs.iter().find(|j| j.id == id).unwrap();
            return (StatusCode::CREATED, Json(st.doc(job)));
        }
        let id = format!("job_{:x}", st.posts.load(Ordering::SeqCst));
        let state = st.initial_state.lock().unwrap().clone();
        let (error_code, error_message) = st.initial_error.lock().unwrap().clone();
        let outputs = if state == "succeeded" {
            st.outputs_for(&id)
        } else {
            json!([])
        };
        let job = MockJob {
            id: id.clone(),
            state,
            error_code,
            error_message,
            outputs,
        };
        let doc = st.doc(&job);
        st.jobs.lock().unwrap().push(job);
        st.by_key.lock().unwrap().insert(key, id);
        (StatusCode::CREATED, Json(doc))
    }

    async fn get_job(
        State(st): State<Arc<MockSparehour>>,
        Path(job_id): Path<String>,
    ) -> (StatusCode, Json<Value>) {
        st.polls.fetch_add(1, Ordering::SeqCst);
        let jobs = st.jobs.lock().unwrap();
        match jobs.iter().find(|j| j.id == job_id) {
            Some(job) => (StatusCode::OK, Json(st.doc(job))),
            None => (
                StatusCode::NOT_FOUND,
                Json(json!({ "detail": "no such job" })),
            ),
        }
    }

    async fn get_artifact(
        State(st): State<Arc<MockSparehour>>,
        Path((_job_id, _sha)): Path<(String, String)>,
    ) -> (StatusCode, [(header::HeaderName, &'static str); 1], Vec<u8>) {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain")],
            st.artifact.clone(),
        )
    }

    async fn spawn(router: axum::Router) -> url::Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        url::Url::parse(&format!("http://{addr}/")).unwrap()
    }

    fn sparehour_router(mock: Arc<MockSparehour>) -> axum::Router {
        axum::Router::new()
            .route("/v1/workloads", get(workloads))
            .route("/v1/jobs", post(submit_job))
            .route("/v1/jobs/{job_id}", get(get_job))
            .route("/v1/jobs/{job_id}/artifacts/{sha256}", get(get_artifact))
            .with_state(mock)
    }

    /// In-process Blossom mock (blossom_upload requires an https url back).
    async fn blossom_upload_mock(_headers: HeaderMap, body: axum::body::Bytes) -> Json<Value> {
        let sha = hex::encode(Sha256::digest(&body));
        Json(json!({
            "url": format!("https://cdn.example.com/{sha}"),
            "sha256": sha,
            "size": body.len(),
        }))
    }

    fn blossom_router() -> axum::Router {
        axum::Router::new().route("/upload", put(blossom_upload_mock))
    }

    // ── harness ───────────────────────────────────────────────────────

    struct Harness {
        dir: PathBuf,
        store: Arc<store::JobStore>,
        http: reqwest::Client,
        keys: Keys,
        blossom: Option<String>,
    }

    impl Harness {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("hench-sparehour-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self {
                store: Arc::new(store::JobStore::open(&dir).unwrap()),
                http: reqwest::Client::new(),
                keys: Keys::generate(),
                blossom: None,
                dir,
            }
        }

        fn with_blossom(mut self, base: url::Url) -> Self {
            self.blossom = Some(base.to_string());
            self
        }

        fn cfg(&self) -> Config {
            Config {
                blossom: self
                    .blossom
                    .as_deref()
                    .map(|b| vec![b.trim_end_matches('/').to_string()])
                    .unwrap_or_default(),
                ..Config::default()
            }
        }

        /// Simulate a process restart over the same data dir.
        fn reopen_store(&mut self) {
            self.store = Arc::new(store::JobStore::open(&self.dir).unwrap());
        }

        /// Mirrors handle_tool_call's gate exactly: a terminal record is
        /// served from cache, anything else runs the tool.
        async fn call(
            &self,
            tool: &TranslateSegment,
            canonical: &str,
            args: &Value,
        ) -> JsonRpcMessage {
            if let Some(cached) = crate::state::cached_terminal_result(&self.store, canonical) {
                return mcp_success(&json!(1), cached);
            }
            let cfg = self.cfg();
            let progress = |_status: &str| async {}.boxed();
            tool.run(ToolCtx {
                cfg,
                canonical: canonical.to_string(),
                args: args.clone(),
                id: json!(1),
                client_pubkey: CLIENT.to_string(),
                data_dir: &self.dir,
                http: &self.http,
                keys: &self.keys,
                store: &self.store,
                progress: &progress,
            })
            .await
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn make_tool(base: url::Url, wait_secs: u64) -> TranslateSegment {
        TranslateSegment::new(SparehourConfig {
            base_url: base,
            token: Secret::new(TOKEN.to_string()),
            project: "proj".into(),
            route: DEFAULT_ROUTE.into(),
            price_sats: 21,
            max_pending: 20,
            wait_secs,
            poll_interval_ms: 20,
        })
    }

    fn args() -> Value {
        json!({ "text": "Hello, world.", "source": "en", "target": "es" })
    }

    fn result_of(msg: JsonRpcMessage) -> Value {
        match msg {
            JsonRpcMessage::Response(r) => r.result,
            other => panic!("expected a response, got {other:?}"),
        }
    }

    fn error_body(result: &Value) -> Value {
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// The canonical id handle_tool_call/resolve_price compute for one call.
    fn canonical_for(name: &str, args: &Value) -> String {
        let params = json!({ "name": name, "arguments": args });
        let identity =
            compute_canonical_invocation_identity(CLIENT, "tools/call", Some(&params)).unwrap();
        format!("{}:{}", identity.client_pubkey, identity.invocation_hash)
    }

    fn price_params(tool: &TranslateSegment, args: &Value) -> ResolvePriceParams {
        ResolvePriceParams {
            capability: tool.priced_capability(&Config::default()).unwrap(),
            request: JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: json!(1),
                method: "tools/call".into(),
                params: Some(json!({ "name": "translate_segment", "arguments": args })),
            },
            client_pubkey: CLIENT.into(),
            request_event_id: "event".into(),
        }
    }

    // ── tests ─────────────────────────────────────────────────────────

    #[test]
    fn ids_and_paths_stay_on_the_configured_base() {
        assert!(is_safe_id("job_abc-123"));
        for bad in ["", "..", "a/b", "a.b", "job?x=1", "%2e%2e"] {
            assert!(!is_safe_id(bad), "{bad}");
        }
        let lookup = |prefix: &'static str| {
            move |k: &str| match k {
                "HENCH_SPAREHOUR_URL" => Some(prefix.to_string()),
                "HENCH_SPAREHOUR_TOKEN" => Some("t".into()),
                "HENCH_SPAREHOUR_PROJECT" => Some("p".into()),
                "HENCH_SPAREHOUR_PRICE_SATS" => Some("5".into()),
                _ => None,
            }
        };
        let tool = TranslateSegment::new(
            SparehourConfig::from_lookup(lookup("https://host.example/sparehour")).unwrap(),
        );
        assert_eq!(
            tool.join_base("/v1/jobs/job_1").unwrap().as_str(),
            "https://host.example/sparehour/v1/jobs/job_1"
        );
        let tool = TranslateSegment::new(
            SparehourConfig::from_lookup(lookup("https://host.example")).unwrap(),
        );
        assert_eq!(
            tool.join_base("/v1/workloads").unwrap().as_str(),
            "https://host.example/v1/workloads"
        );
    }

    #[test]
    fn secret_never_prints_its_value() {
        let secret = Secret::new("super-secret-token".into());
        assert_eq!(format!("{secret:?}"), "***");
        assert_eq!(format!("{secret}"), "***");
        let cfg = make_tool("http://127.0.0.1:1/".parse().unwrap(), 1).cfg;
        assert_eq!(format!("{:?}", cfg.token), "***");
    }

    #[test]
    fn config_is_none_when_unset_partial_or_invalid() {
        // Nothing set: silent None.
        assert!(SparehourConfig::from_lookup(|_| None).is_none());
        // Partial (no token): None.
        assert!(SparehourConfig::from_lookup(|k| match k {
            ENV_URL => Some("https://sparehour.example".into()),
            ENV_PROJECT => Some("proj".into()),
            ENV_PRICE => Some("21".into()),
            _ => None,
        })
        .is_none());
        // Invalid scheme and non-positive price: None.
        assert!(SparehourConfig::from_lookup(|k| match k {
            ENV_URL => Some("ftp://sparehour.example".into()),
            ENV_TOKEN => Some("t".into()),
            ENV_PROJECT => Some("proj".into()),
            ENV_PRICE => Some("0".into()),
            _ => None,
        })
        .is_none());
        // Unparsable numbers: None.
        assert!(SparehourConfig::from_lookup(|k| match k {
            ENV_URL => Some("https://sparehour.example".into()),
            ENV_TOKEN => Some("t".into()),
            ENV_PROJECT => Some("proj".into()),
            ENV_PRICE => Some("21".into()),
            ENV_MAX_PENDING => Some("many".into()),
            ENV_WAIT => Some("soon".into()),
            _ => None,
        })
        .is_none());
        // A lone optional variable counts as "some set": still None, no panic.
        assert!(SparehourConfig::from_lookup(|k| {
            (k == ENV_ROUTE).then(|| "libretranslate-v1".to_string())
        })
        .is_none());
    }

    #[test]
    fn config_parses_with_defaults_and_clamps() {
        let cfg = SparehourConfig::from_lookup(|k| match k {
            ENV_URL => Some("https://sparehour.example/".into()),
            ENV_TOKEN => Some(" tok ".into()),
            ENV_PROJECT => Some(" proj ".into()),
            ENV_PRICE => Some("42".into()),
            ENV_WAIT => Some("99999".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(cfg.base_url.as_str(), "https://sparehour.example/");
        assert_eq!(cfg.project, "proj");
        assert_eq!(cfg.route, DEFAULT_ROUTE);
        assert_eq!(cfg.price_sats, 42);
        assert_eq!(cfg.max_pending, 20);
        assert_eq!(cfg.wait_secs, EXECUTION_BUDGET_SECS - 60);
        assert_eq!(cfg.poll_interval_ms, 2_000);

        // Token read (trimmed) from a file when the direct token is absent.
        let dir = std::env::temp_dir().join(format!("hench-sparehour-tok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("token"), "  file-token \n").unwrap();
        let path = dir.join("token").to_str().unwrap().to_string();
        let cfg = SparehourConfig::from_lookup(move |k| match k {
            ENV_URL => Some("http://127.0.0.1:9".into()),
            ENV_TOKEN_FILE => Some(path.clone()),
            ENV_PROJECT => Some("proj".into()),
            ENV_PRICE => Some("5".into()),
            ENV_ROUTE => Some("libretranslate-v1".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(format!("{:?}", cfg.token), "***");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_by_default_keeps_the_default_registry() {
        // No HENCH_SPAREHOUR_* in the test environment: the env-built
        // registry equals the default (download_media-only) one.
        if std::env::var_os(ENV_URL).is_some() {
            eprintln!("skipping: HENCH_SPAREHOUR_URL is set in the environment");
            return;
        }
        let registry = crate::tools::registry_from_env();
        assert!(registry.get("translate_segment").is_none());
        assert!(registry.get("download_media").is_some());
        assert_eq!(
            registry.descriptors(),
            crate::tools::default_registry().descriptors()
        );
        // default_registry itself is unchanged: download_media only.
        assert_eq!(crate::tools::default_registry().descriptors().len(), 1);
    }

    #[test]
    fn descriptor_is_stable_and_mentions_no_backend() {
        let tool = make_tool("http://127.0.0.1:1/".parse().unwrap(), 1);
        let d = tool.descriptor();
        assert_eq!(d["name"], json!("translate_segment"));
        assert_eq!(
            d["description"],
            json!("Translate one text segment (up to 20,000 characters) between two languages. Returns a hosted text file URL.")
        );
        let schema = &d["inputSchema"];
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["required"], json!(["text", "source", "target"]));
        assert_eq!(schema["properties"]["text"]["maxLength"], json!(20000));
        assert_eq!(
            schema["properties"]["source"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            35
        );
        assert_eq!(
            schema["properties"]["target"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            34
        );
        assert!(!schema["properties"]["target"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("auto")));
        assert!(!serde_json::to_string(&d)
            .unwrap()
            .to_lowercase()
            .contains("sparehour"));
        let cap = tool.priced_capability(&Config::default()).unwrap();
        assert_eq!(cap.method, "tools/call");
        assert_eq!(cap.name.as_deref(), Some("translate_segment"));
        assert_eq!((cap.amount, cap.max_amount), (21, Some(21)));
        assert_eq!(cap.currency_unit, "sats");
        assert_eq!(cap.description.as_deref(), Some("Hench text translation"));
    }

    #[tokio::test]
    async fn quote_passes_and_run_completes_end_to_end() {
        let mock = Arc::new(MockSparehour::new());
        *mock.initial_state.lock().unwrap() = "succeeded".into();
        let base = spawn(sparehour_router(mock.clone())).await;
        let h = Harness::new("ok").with_blossom(spawn(blossom_router()).await);
        let tool = make_tool(base, 5);

        // Quote: workloads says pending_jobs 0 -> priced.
        match tool.quote(&h.cfg(), &args()).await {
            QuoteDecision::Quote {
                amount,
                description,
                url,
                mode,
            } => {
                assert_eq!(amount, 21);
                assert_eq!(description, "hench translate_segment (en->es)");
                assert_eq!(url, "translate-segment");
                assert_eq!(mode, "en->es");
            }
            other => panic!("expected quote, got {other:?}"),
        }

        let canonical = canonical_for("translate_segment", &args());
        let result = result_of(h.call(&tool, &canonical, &args()).await);
        assert_eq!(result["isError"], json!(false));
        let text = result["content"][0]["text"].as_str().unwrap();
        let body: Value = serde_json::from_str(text).unwrap();
        let sha = mock.artifact_sha();
        assert_eq!(body["mimeType"], json!("text/plain"));
        assert_eq!(body["sha256"], json!(sha));
        assert_eq!(body["sizeBytes"], json!(mock.artifact.len()));
        assert_eq!(body["url"], json!(format!("https://cdn.example.com/{sha}")));
        let link = &result["content"][1];
        assert_eq!(link["type"], json!("resource_link"));
        assert_eq!(link["uri"], body["url"]);
        assert_eq!(link["name"], json!("translation.txt"));
        assert_eq!(link["mimeType"], json!("text/plain"));
        assert_eq!(
            link["description"],
            json!("Translated by hench translate_segment")
        );

        let record = h.store.get(&canonical).unwrap();
        assert_eq!(record.status, store::COMPLETED);
        assert_eq!(record.result.as_ref().unwrap()["isError"], json!(false));
        // The record never carries the text, and no output carries the token.
        assert!(!serde_json::to_string(&record)
            .unwrap()
            .contains("Hello, world."));
        assert!(!serde_json::to_string(&result).unwrap().contains(TOKEN));

        // Wire details: bearer token + canonical idempotency key.
        assert_eq!(*mock.post_keys.lock().unwrap(), vec![canonical]);
        assert_eq!(
            *mock.post_auths.lock().unwrap(),
            vec![format!("Bearer {TOKEN}")]
        );
        assert_eq!(mock.auth_failures.load(Ordering::SeqCst), 0);
        assert_eq!(mock.posts(), 1);
    }

    #[tokio::test]
    async fn failure_after_payment_is_a_cached_error_with_no_refund() {
        let mock = Arc::new(MockSparehour::new());
        *mock.initial_state.lock().unwrap() = "dead-letter".into();
        *mock.initial_error.lock().unwrap() =
            ("model_server_error".into(), "translate failed".into());
        let base = spawn(sparehour_router(mock.clone())).await;
        let mut h = Harness::new("dead");
        let tool = make_tool(base, 2);
        let canonical = canonical_for("translate_segment", &args());

        let result = result_of(h.call(&tool, &canonical, &args()).await);
        assert_eq!(result["isError"], json!(true));
        let body = error_body(&result);
        assert_eq!(body["code"], json!("BACKEND_FAILED"));
        assert_eq!(
            body["message"],
            json!("dead-letter: model_server_error: translate failed")
        );
        assert!(!serde_json::to_string(&result).unwrap().contains(TOKEN));
        assert_eq!(h.store.get(&canonical).unwrap().status, store::FAILED);

        // Repeat call: the quoter waives (terminal replay) and the handler's
        // cache serves the error — the mock sees no new POST and no poll.
        let q = quoter(
            Arc::new(RwLock::new(h.cfg())),
            h.store.clone(),
            ToolRegistry::from_tools(vec![Arc::new(tool.clone())]),
        );
        match q.resolve_price(price_params(&tool, &args())).await.unwrap() {
            ResolvePriceResult::Waive { .. } => {}
            other => panic!("expected waive, got {other:?}"),
        }
        let (posts_before, polls_before) = (mock.posts(), mock.polls());
        let replay = result_of(h.call(&tool, &canonical, &args()).await);
        assert_eq!(replay, result);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(mock.posts(), posts_before);
        assert_eq!(mock.polls(), polls_before);
        h.reopen_store();
        assert_eq!(h.store.get(&canonical).unwrap().status, store::FAILED);
    }

    #[tokio::test]
    async fn backend_rejection_after_payment_is_terminal_and_token_free() {
        let mock = Arc::new(MockSparehour::new());
        mock.post_failures.lock().unwrap().push_back((
            409,
            "idempotency key was already used for a different request".into(),
        ));
        let base = spawn(sparehour_router(mock.clone())).await;
        let h = Harness::new("rejected");
        let tool = make_tool(base, 2);
        let canonical = canonical_for("translate_segment", &args());

        let result = result_of(h.call(&tool, &canonical, &args()).await);
        assert_eq!(result["isError"], json!(true));
        let body = error_body(&result);
        assert_eq!(body["code"], json!("BACKEND_REJECTED"));
        assert_eq!(
            body["message"],
            json!("409: idempotency key was already used for a different request")
        );
        assert_eq!(h.store.get(&canonical).unwrap().status, store::FAILED);
        assert!(!serde_json::to_string(&result).unwrap().contains(TOKEN));
    }

    #[tokio::test]
    async fn restart_and_repeat_resume_the_saved_job_without_resubmitting() {
        let mock = Arc::new(MockSparehour::new());
        let base = spawn(sparehour_router(mock.clone())).await;
        let mut h = Harness::new("resume").with_blossom(spawn(blossom_router()).await);
        let canonical = canonical_for("translate_segment", &args());
        let tool = make_tool(base, 1); // tiny wait: times out while "running"

        let result = result_of(h.call(&tool, &canonical, &args()).await);
        assert_eq!(error_body(&result)["code"], json!("PENDING"));
        let record = h.store.get(&canonical).unwrap();
        assert_eq!(record.status, store::WAITING);
        let job_id = mock.first_job_id();
        assert_eq!(
            record.tool_state,
            Some(json!({ "paid": true, "sparehour_job_id": job_id }))
        );

        // Process restart over the same data dir: the paid record is NOT
        // swept and the job id survives.
        h.reopen_store();
        let kept = h.store.get(&canonical).unwrap();
        assert_eq!(kept.status, store::WAITING);
        assert_eq!(
            kept.tool_state,
            Some(json!({ "paid": true, "sparehour_job_id": job_id }))
        );

        // The quoter waives the repeat (paid, resumable, not inflight) —
        // re-binding a settled payment, never a second invoice.
        let q = quoter(
            Arc::new(RwLock::new(h.cfg())),
            h.store.clone(),
            ToolRegistry::from_tools(vec![Arc::new(tool.clone())]),
        );
        match q.resolve_price(price_params(&tool, &args())).await.unwrap() {
            ResolvePriceResult::Waive { .. } => {}
            other => panic!("expected waive, got {other:?}"),
        }
        // Inflight still rejects with the same message as before.
        h.store.inflight_add(&canonical);
        match q.resolve_price(price_params(&tool, &args())).await.unwrap() {
            ResolvePriceResult::Reject { message } => {
                assert_eq!(
                    message.as_deref(),
                    Some("execution already in progress for this request")
                );
            }
            other => panic!("expected reject, got {other:?}"),
        }
        h.store.inflight_remove(&canonical);
        // A WAITING record without the paid marker still rejects as
        // interrupted (download_media semantics unchanged).
        let mut other_args = args();
        other_args["text"] = json!("a different segment, so the canonical differs");
        let other_canonical = canonical_for("translate_segment", &other_args);
        h.store
            .mark_waiting(store::JobRecord {
                canonical_id: other_canonical.clone(),
                status: store::WAITING.into(),
                amount_msat: 0,
                url: "translate-segment".into(),
                mode: "en->es".into(),
                client: CLIENT.into(),
                result: None,
                tool_state: None,
                updated_at: 0,
            })
            .unwrap();
        match q
            .resolve_price(price_params(&tool, &other_args))
            .await
            .unwrap()
        {
            ResolvePriceResult::Reject { message } => {
                assert_eq!(
                    message.as_deref(),
                    Some("a previous attempt of this exact request was interrupted; change the request to retry")
                );
            }
            other => panic!("expected reject, got {other:?}"),
        }
        assert!(h.store.get(&other_canonical).unwrap().tool_state.is_none());

        // Flip the backend to done and resume: COMPLETED, still ONE POST.
        mock.set_state(&job_id, "succeeded");
        let result = result_of(h.call(&tool, &canonical, &args()).await);
        assert_eq!(result["isError"], json!(false));
        assert_eq!(h.store.get(&canonical).unwrap().status, store::COMPLETED);
        assert_eq!(mock.posts(), 1, "resume must not resubmit");
    }

    #[tokio::test]
    async fn transient_submit_failure_is_resumable_and_idempotent() {
        let mock = Arc::new(MockSparehour::new());
        mock.post_failures
            .lock()
            .unwrap()
            .push_back((503, "backend briefly unavailable".into()));
        let base = spawn(sparehour_router(mock.clone())).await;
        let h = Harness::new("idem").with_blossom(spawn(blossom_router()).await);
        let canonical = canonical_for("translate_segment", &args());
        let tool = make_tool(base, 2);

        // First call: POST 503 -> PENDING; record WAITING + paid, no job id.
        let result = result_of(h.call(&tool, &canonical, &args()).await);
        assert_eq!(error_body(&result)["code"], json!("PENDING"));
        let record = h.store.get(&canonical).unwrap();
        assert_eq!(record.status, store::WAITING);
        assert_eq!(record.tool_state, Some(json!({ "paid": true })));

        // Second call for the same canonical: POSTs again under the SAME
        // idempotency key; the mock (like Sparehour) ends up with exactly
        // one job and the call completes.
        *mock.initial_state.lock().unwrap() = "succeeded".into();
        let result = result_of(h.call(&tool, &canonical, &args()).await);
        assert_eq!(result["isError"], json!(false));
        assert_eq!(h.store.get(&canonical).unwrap().status, store::COMPLETED);
        let keys = mock.post_keys.lock().unwrap().clone();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], canonical);
        assert_eq!(keys[0], keys[1]);
        assert_eq!(mock.jobs.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn quote_fails_closed_on_busy_blind_or_bad_args() {
        let mock = Arc::new(MockSparehour::new());
        let base = spawn(sparehour_router(mock.clone())).await;
        let h = Harness::new("quote-reject");
        let tool = make_tool(base, 1);
        let cfg = h.cfg();

        // Busy queue.
        *mock.depth.lock().unwrap() = Some(21);
        match tool.quote(&cfg, &args()).await {
            QuoteDecision::Reject { message } => {
                assert_eq!(
                    message,
                    "translation queue is busy (21 pending); try again later"
                )
            }
            other => panic!("expected reject, got {other:?}"),
        }
        // Depth invisible (token without perms=dynamic).
        *mock.depth.lock().unwrap() = None;
        match tool.quote(&cfg, &args()).await {
            QuoteDecision::Reject { message } => {
                assert_eq!(
                    message,
                    "worker cannot see the translation queue depth; refusing to sell work it cannot schedule"
                )
            }
            other => panic!("expected reject, got {other:?}"),
        }
        *mock.depth.lock().unwrap() = Some(0);

        // Bad arguments.
        let mut long = args();
        long["text"] = json!("x".repeat(20_001));
        match tool.quote(&cfg, &long).await {
            QuoteDecision::Reject { message } => {
                assert_eq!(message, "text must be at most 20000 characters")
            }
            other => panic!("expected reject, got {other:?}"),
        }
        let mut auto_target = args();
        auto_target["target"] = json!("auto");
        match tool.quote(&cfg, &auto_target).await {
            QuoteDecision::Reject { message } => {
                assert_eq!(
                    message,
                    "target must be one of the supported languages and cannot be 'auto'"
                )
            }
            other => panic!("expected reject, got {other:?}"),
        }
        let mut extra = args();
        extra["urgent"] = json!(true);
        match tool.quote(&cfg, &extra).await {
            QuoteDecision::Reject { message } => {
                assert_eq!(
                    message,
                    "unexpected arguments: urgent; only text, source, and target are accepted"
                )
            }
            other => panic!("expected reject, got {other:?}"),
        }

        // Nothing listening: backend unavailable.
        let dead = make_tool("http://127.0.0.1:9/".parse().unwrap(), 1);
        match dead.quote(&cfg, &args()).await {
            QuoteDecision::Reject { message } => {
                assert_eq!(message, "translation backend unavailable")
            }
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_rejects_bad_args_and_unserved_route_with_clear_errors() {
        let mock = Arc::new(MockSparehour::new());
        let base = spawn(sparehour_router(mock.clone())).await;
        let h = Harness::new("args");
        let tool = make_tool(base.clone(), 1);
        let canonical = canonical_for("translate_segment", &args());

        let mut bad = args();
        bad["source"] = json!("klingon");
        let other_canonical = canonical_for("translate_segment", &bad);
        match h.call(&tool, &other_canonical, &bad).await {
            JsonRpcMessage::ErrorResponse(e) => assert_eq!(e.error.code, -32602),
            other => panic!("expected error response, got {other:?}"),
        }
        assert!(h.store.get(&other_canonical).is_none());
        assert!(h.store.get(&canonical).is_none());

        // A configured route the backend does not serve: the quote refuses.
        let odd = TranslateSegment::new(SparehourConfig {
            base_url: base.clone(),
            token: Secret::new(TOKEN.into()),
            project: "proj".into(),
            route: "gpu-v9".into(),
            price_sats: 21,
            max_pending: 20,
            wait_secs: 1,
            poll_interval_ms: 20,
        });
        match odd.quote(&h.cfg(), &args()).await {
            QuoteDecision::Reject { message } => {
                assert_eq!(message, "translation route not available")
            }
            other => panic!("expected reject, got {other:?}"),
        }
    }
}
