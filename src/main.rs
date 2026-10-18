//! hench-cli: the Hench worker for desktop computers, with a web dashboard.
//!
//! Announces a ContextVM (MCP over Nostr) server with the `download_media`
//! tool, gates calls behind CEP-8 Lightning payments (NWC wallet), runs
//! yt-dlp, uploads results to Blossom — same wire behavior as the Android
//! app. Job records persist so paid replays are answered from cache and
//! never re-invoiced. Settings live in <data_dir>/config.json and are
//! editable at http://127.0.0.1:8932 (see README).
mod config;
mod http;
mod media;
mod nwc;
mod pricing;
mod state;
mod store;

use state::AppState;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

/// Load or create the worker secret key (hex, 0600 from creation).
fn load_key(data_dir: &std::path::Path) -> anyhow::Result<contextvm_sdk::signer::Keys> {
    store::ensure_dir_private(data_dir)?;
    let path = data_dir.join("secret");
    if let Ok(sk) = std::env::var("HENCH_SK") {
        return Ok(contextvm_sdk::signer::from_sk(&sk)?);
    }
    match std::fs::read_to_string(&path) {
        Ok(sk) if sk.trim().len() == 64 => {
            // An existing readable secret gets tightened, not trusted.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(contextvm_sdk::signer::from_sk(sk.trim())?)
        }
        _ => {
            let keys = contextvm_sdk::signer::generate();
            store::write_atomic_private(&path, keys.secret_key().to_secret_hex().as_bytes())?;
            Ok(keys)
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse().unwrap()),
        )
        .init();

    let free = std::env::var("HENCH_FREE").ok().as_deref() == Some("1");
    let data_dir = std::env::var("HENCH_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".hench-cli"));
    let keys = load_key(&data_dir)?;

    let mut cfg = config::load(&data_dir, free);
    if !free && cfg.nwc_uri.as_deref().map(str::trim).unwrap_or("").is_empty() {
        // First run in paid mode without a wallet: hold the worker off until
        // one is configured in the dashboard (or HENCH_NWC_URI on next start).
        cfg.enabled = false;
    }
    let state = Arc::new(AppState {
        data_dir: data_dir.clone(),
        free,
        keys,
        config: Arc::new(RwLock::new(cfg.clone())),
        store: Arc::new(store::JobStore::open(&data_dir)?),
        http: state::upload_http_client(),
        nwc: tokio::sync::Mutex::new(None),
        worker: tokio::sync::Mutex::new(None),
        startup: std::time::Instant::now(),
    });

    println!("hench-cli worker  npub={}  hex={}", state.npub(), state.keys.public_key().to_hex());
    println!("  data dir: {}", data_dir.display());
    println!("  mode:     {}", if free { "FREE (unpaid, testing)".into() } else { "paid via NWC".to_string() });
    let bind = std::env::var("HENCH_DASH_BIND").unwrap_or_else(|_| "127.0.0.1:8932".into());
    let token = std::env::var("HENCH_DASH_TOKEN").ok().filter(|s| !s.is_empty());
    let loopback = http::loopback_bind(&bind);
    if !loopback && token.is_none() {
        anyhow::bail!("HENCH_DASH_BIND={bind} is not loopback — set HENCH_DASH_TOKEN");
    }
    println!("  dashboard: http://{bind}{}", token.as_ref().map(|_| " (token required)").unwrap_or(""));
    // Bind before the worker starts: an occupied port or bad address must
    // fail startup, not leave a silently headless process running.
    use anyhow::Context;
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("dashboard bind failed on {bind}"))?;

    if cfg.enabled {
        if let Err(e) = state.start_worker().await {
            tracing::error!("worker failed to start: {e:#}");
        }
    }

    let dash = tokio::spawn({
        let state = state.clone();
        async move { http::serve(state, listener, &bind, loopback, token).await }
    });

    // Ctrl-C stops the worker cleanly; dashboard runs until killed.
    tokio::signal::ctrl_c().await?;
    state.stop_worker().await;
    dash.abort();
    Ok(())
}
