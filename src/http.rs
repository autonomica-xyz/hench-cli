//! Web dashboard: static single-page UI + JSON API.
//!   GET  /            dashboard page
//!   GET  /api/state   live state (config, stats, jobs, worker)
//!   POST /api/config  validate + persist + live-apply settings (restarts worker)
//!   POST /api/worker  { "enabled": bool } — start/stop the worker
//! Auth: HENCH_DASH_TOKEN env — Bearer header or ?token=. Required when
//! bound to a non-loopback address.
use crate::config::{self, Config};
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Serve the dashboard on an already-bound listener (bind errors must fail
/// startup in main, not a detached task). `bind`/`loopback_only` drive the
/// Host policy: a loopback listener refuses non-loopback Host headers
/// (DNS-rebinding), any other bind accepts its own network names because the
/// token is mandatory there.
pub async fn serve(
    state: Arc<AppState>,
    listener: tokio::net::TcpListener,
    bind: &str,
    loopback_only: bool,
    token: Option<String>,
) -> anyhow::Result<()> {
    if !loopback_only && token.is_none() {
        anyhow::bail!(
            "refusing to expose the dashboard on a non-loopback bind without HENCH_DASH_TOKEN"
        );
    }
    let auth = Arc::new(token);
    let allowed_host = host_part(bind).to_lowercase();
    let app = Router::new()
        .route("/", get(index))
        .route("/api/state", get(api_state))
        .route("/api/config", post(api_config))
        .route("/api/worker", post(api_worker))
        .route("/api/shutdown", post(api_shutdown))
        .with_state((state, auth, loopback_only, allowed_host));
    tracing::info!(
        "dashboard listening on {}",
        listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default()
    );
    axum::serve(listener, app).await?;
    Ok(())
}

/// True when `bind` listens on loopback only ("127.0.0.1:8932", "[::1]:8932",
/// "localhost:…"). Anything else is remote-exposed.
pub fn loopback_bind(bind: &str) -> bool {
    let host = host_part(bind);
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// Host part of a bind/listen address ("[::1]:8932" -> "::1").
fn host_part(bind: &str) -> String {
    if let Some(rest) = bind.strip_prefix('[') {
        rest.split(']').next().unwrap_or("").to_string()
    } else {
        bind.split(':').next().unwrap_or("").to_string()
    }
}

/// Split "host[:port]" / "[v6][:port]" into lowercase host + optional port.
fn split_host_port(authority: &str) -> (String, Option<u16>) {
    let a = authority.trim().to_lowercase();
    if let Some(rest) = a.strip_prefix('[') {
        let mut it = rest.splitn(2, ']');
        let host = it.next().unwrap_or("").to_string();
        let port = it
            .next()
            .and_then(|p| p.strip_prefix(':'))
            .and_then(|p| p.parse().ok());
        return (host, port);
    }
    match a.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h.to_string(), p.parse().ok()),
        _ => (a, None), // bare host or bare IPv6 literal
    }
}

type Ctx = (Arc<AppState>, Arc<Option<String>>, bool, String);

/// DNS-rebinding defense: a loopback dashboard must only serve requests
/// addressed to loopback itself. A hostile site re-binding its DNS to
/// 127.0.0.1 arrives with `Host: attacker.example` and is refused. On a
/// remote (token-required) bind any Host is fine — the credential is the
/// token, not the hostname.
fn check_host(ctx: &Ctx, headers: &HeaderMap) -> StatusCode {
    let (_, _, loopback_only, allowed_host) = ctx;
    if !*loopback_only {
        return StatusCode::OK;
    }
    let Some(host) = headers.get("host").and_then(|v| v.to_str().ok()) else {
        return StatusCode::FORBIDDEN; // HTTP/1.1 requires Host
    };
    let host = split_host_port(host).0;
    if host == *allowed_host || host == "localhost" {
        return StatusCode::OK;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_loopback() => StatusCode::OK,
        _ => StatusCode::FORBIDDEN,
    }
}

/// Same-origin enforcement for mutating routes. Host checks alone do not
/// stop a cross-origin simple POST (an HTML <form> needs no CORS preflight
/// and sends no JSON content type). curl/CLI requests carry no Origin or
/// Sec-Fetch-Site and pass; browser requests are denied unless their origin
/// authority matches the Host they addressed.
fn check_mutation(ctx: &Ctx, headers: &HeaderMap) -> StatusCode {
    if check_host(ctx, headers) != StatusCode::OK {
        return StatusCode::FORBIDDEN;
    }
    match headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        Some("cross-site") | Some("same-site") => return StatusCode::FORBIDDEN,
        _ => {}
    }
    let (host, host_port) = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(split_host_port)
        .unwrap_or_default();
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        let ok = url::Url::parse(origin)
            .ok()
            .map(|o| {
                let oport = o.port_or_known_default();
                // Host may omit the default port.
                host == o.host_str().unwrap_or_default().to_lowercase()
                    && match (host_port, oport) {
                        (Some(hp), Some(op)) => hp == op,
                        (None, Some(op)) => op == 80,
                        (Some(_), None) => false,
                        (None, None) => true,
                    }
            })
            .unwrap_or(false);
        if !ok {
            return StatusCode::FORBIDDEN;
        }
    }
    StatusCode::OK
}

fn check_auth(ctx: &Ctx, headers: &HeaderMap, query: &HashMap<String, String>) -> StatusCode {
    let (_, auth, _, _) = ctx;
    let Some(expected) = auth.as_ref() else {
        return StatusCode::OK;
    };
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| query.get("token").cloned());
    match provided {
        Some(t) if constant_time_eq(t.as_bytes(), expected.as_bytes()) => StatusCode::OK,
        _ => StatusCode::UNAUTHORIZED,
    }
}

// ponytail: byte-wise const-time compare instead of a crate; token is the only secret compared here.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn index(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<&'static str>, StatusCode> {
    if check_host(&ctx, &headers) != StatusCode::OK {
        return Err(StatusCode::FORBIDDEN);
    }
    let status = check_auth(&ctx, &headers, &q);
    if status != StatusCode::OK {
        return Err(status);
    }
    Ok(Html(DASHBOARD_HTML))
}

async fn api_state(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    let (state, ..) = &ctx;
    if check_host(&ctx, &headers) != StatusCode::OK
        || (check_auth(&ctx, &headers, &q)) != StatusCode::OK
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let cfg = state.config.read().unwrap().clone();
    let (earned_msat, completed, failed) = state.store.stats();
    let running = state.worker.lock().await.is_some();
    let jobs: Vec<Value> = state
        .store
        .recent(20)
        .into_iter()
        .map(|j| {
            json!({
                "canonical": j.canonical_id,
                "status": j.status,
                "url": j.url,
                "mode": j.mode,
                "amountSats": j.amount_msat.saturating_add(999) / 1000,
                "updatedAt": j.updated_at,
                "detail": j.result.as_ref()
                    .and_then(|r| r.get("content"))
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("text"))
                    .cloned()
                    .unwrap_or(Value::Null),
            })
        })
        .collect();
    let mut config_json = serde_json::to_value(&cfg).unwrap_or_default();
    // Never echo the wallet secret back; the UI shows a write-only field.
    if let Some(uri) = config_json.get("nwcUri").and_then(|v| v.as_str()) {
        config_json["nwcUri"] = json!(if uri.is_empty() {
            ""
        } else {
            "••••• set (hidden)"
        });
    }
    config_json["nwcConfigured"] = json!(cfg
        .nwc_uri
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false));
    Ok(Json(json!({
        "running": running,
        "uptimeSecs": state.worker.lock().await.as_ref().map(|w| w.started_at.elapsed().as_secs()).unwrap_or(0),
        "processUptimeSecs": state.startup.elapsed().as_secs(),
        "npub": state.npub(),
        "pubkeyHex": state.keys.public_key().to_hex(),
        "free": state.free,
        "platforms": config::platform_names(),
        "config": config_json,
        "stats": {
            "earnedSats": earned_msat / 1000,
            "completed": completed,
            "failed": failed,
        },
        "jobs": jobs,
    })))
}

async fn api_config(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Json(mut new_cfg): Json<Config>,
) -> Result<Json<Value>, StatusCode> {
    let (state, ..) = &ctx;
    if check_mutation(&ctx, &headers) != StatusCode::OK
        || (check_auth(&ctx, &headers, &q)) != StatusCode::OK
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Write-only wallet field: keep the stored URI unless a real one is sent.
    let masked = new_cfg.nwc_uri.as_deref().unwrap_or("").contains("••");
    if masked || new_cfg.nwc_uri.as_deref().unwrap_or("").is_empty() {
        new_cfg.nwc_uri = state.config.read().unwrap().nwc_uri.clone();
    }
    if state.free {
        new_cfg.nwc_uri = None;
    }
    let issues = config::validate(&new_cfg, state.free);
    if !issues.is_empty() {
        return Ok(Json(json!({ "ok": false, "issues": issues })));
    }
    let was_running = state.worker.lock().await.is_some();
    // Persist before committing to memory: a failed save must leave the
    // running worker and the file in agreement.
    config::save(&state.data_dir, &new_cfg).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    *state.config.write().unwrap() = new_cfg;
    let restarted = was_running && { state.restart_worker().await.is_ok() };
    tracing::info!(
        "settings saved{}",
        if restarted { " (worker restarted)" } else { "" }
    );
    Ok(Json(json!({ "ok": true, "restarted": restarted })))
}

async fn api_worker(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let (state, ..) = &ctx;
    if check_mutation(&ctx, &headers) != StatusCode::OK
        || (check_auth(&ctx, &headers, &q)) != StatusCode::OK
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let enable = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    state.config.write().unwrap().enabled = enable;
    config::save(&state.data_dir, &state.config.read().unwrap().clone())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if enable {
        match state.start_worker().await {
            Ok(()) => Ok(Json(json!({ "ok": true, "enabled": true }))),
            Err(e) => {
                state.config.write().unwrap().enabled = false;
                let _ = config::save(&state.data_dir, &state.config.read().unwrap().clone());
                Ok(Json(json!({ "ok": false, "error": format!("{e:#}") })))
            }
        }
    } else {
        state.stop_worker().await;
        Ok(Json(json!({ "ok": true, "enabled": false })))
    }
}

async fn api_shutdown(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    if check_mutation(&ctx, &headers) != StatusCode::OK
        || (check_auth(&ctx, &headers, &q)) != StatusCode::OK
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    tracing::warn!("shutdown requested from dashboard");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;

    fn ctx(loopback_only: bool, allowed_host: &str) -> Ctx {
        let dir = std::env::temp_dir().join(format!(
            "hench-http-test-{}-{loopback_only}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let state = Arc::new(AppState {
            data_dir: dir.clone(),
            free: true,
            keys: nostr_sdk::prelude::Keys::generate(),
            tools: crate::tools::default_registry(),
            config: Arc::new(RwLock::new(Config::default())),
            store: Arc::new(crate::store::JobStore::open(&dir).unwrap()),
            http: crate::state::upload_http_client(),
            nwc: tokio::sync::Mutex::new(None),
            worker: tokio::sync::Mutex::new(None),
            startup: std::time::Instant::now(),
        });
        (state, Arc::new(None), loopback_only, allowed_host.into())
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, v.parse().unwrap());
        }
        m
    }

    #[test]
    fn cross_origin_simple_post_denied_curl_allowed() {
        // Regression: /api/shutdown accepted a cross-origin <form> POST with
        // no token configured (Host check alone does not stop simple POSTs).
        let ctx = ctx(true, "127.0.0.1");
        let hostile = headers(&[
            ("host", "127.0.0.1:8932"),
            ("origin", "http://evil.example"),
            ("sec-fetch-site", "cross-site"),
        ]);
        assert_eq!(check_mutation(&ctx, &hostile), StatusCode::FORBIDDEN);
        let hostile_no_sfs = headers(&[
            ("host", "127.0.0.1:8932"),
            ("origin", "http://evil.example"),
        ]);
        assert_eq!(check_mutation(&ctx, &hostile_no_sfs), StatusCode::FORBIDDEN);
        // Same-origin dashboard fetch passes.
        let same = headers(&[
            ("host", "127.0.0.1:8932"),
            ("origin", "http://127.0.0.1:8932"),
            ("sec-fetch-site", "same-origin"),
        ]);
        assert_eq!(check_mutation(&ctx, &same), StatusCode::OK);
        // Plain curl: no Origin/Sec-Fetch-Site at all.
        let curl = headers(&[("host", "127.0.0.1:8932")]);
        assert_eq!(check_mutation(&ctx, &curl), StatusCode::OK);
        // DNS-rebinding: a rebinding hostname is not a loopback Host.
        let rebound = headers(&[("host", "attacker.example:8932")]);
        assert_eq!(check_mutation(&ctx, &rebound), StatusCode::FORBIDDEN);
    }

    #[test]
    fn remote_bind_accepts_own_hostname() {
        // Regression: Host check used to reject the LAN hostname a remote
        // operator legitimately browses to on a 0.0.0.0 bind.
        let ctx = ctx(false, "0.0.0.0");
        let lan = headers(&[
            ("host", "server.lan:8932"),
            ("origin", "http://server.lan:8932"),
        ]);
        assert_eq!(check_mutation(&ctx, &lan), StatusCode::OK);
        // Cross-site still denied remotely (token remains mandatory too).
        let hostile = headers(&[
            ("host", "server.lan:8932"),
            ("sec-fetch-site", "cross-site"),
        ]);
        assert_eq!(check_mutation(&ctx, &hostile), StatusCode::FORBIDDEN);
    }
}
