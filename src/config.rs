//! Worker settings — the CLI subset of the Android app's WorkerSettings,
//! persisted to <data_dir>/config.json and editable over the web dashboard.
//! Env vars only seed the first-run defaults.
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const PLATFORMS: &[(&str, &[&str])] = &[
    ("youtube", &["youtube.com", "youtu.be", "m.youtube.com", "music.youtube.com"]),
    ("tiktok", &["tiktok.com", "www.tiktok.com", "vm.tiktok.com", "m.tiktok.com"]),
    ("instagram", &["instagram.com", "www.instagram.com"]),
    ("twitter", &["twitter.com", "x.com", "mobile.twitter.com"]),
    ("facebook", &["facebook.com", "www.facebook.com", "m.facebook.com", "fb.watch"]),
    ("reddit", &["reddit.com", "www.reddit.com", "old.reddit.com"]),
    ("vimeo", &["vimeo.com", "player.vimeo.com"]),
    ("soundcloud", &["soundcloud.com", "api.soundcloud.com"]),
    ("bilibili", &["bilibili.com", "www.bilibili.com", "b23.tv"]),
    ("dailymotion", &["dailymotion.com", "www.dailymotion.com"]),
    ("pinterest", &["pinterest.com", "www.pinterest.com", "pin.it"]),
    ("snapchat", &["snapchat.com", "story.snapchat.com", "t.snapchat.com"]),
];

pub fn platform_names() -> Vec<&'static str> {
    PLATFORMS.iter().map(|(n, _)| *n).collect()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Hours {
    Always,
    Daily { start: String, end: String },
}

impl Hours {
    pub fn open_now(&self) -> bool {
        match self {
            Hours::Always => true,
            Hours::Daily { start, end } => {
                let (Ok(start), Ok(end)) = (
                    chrono::NaiveTime::parse_from_str(start, "%H:%M"),
                    chrono::NaiveTime::parse_from_str(end, "%H:%M"),
                ) else {
                    return true; // malformed window configured: never lock the worker out
                };
                let now = chrono::Local::now().time();
                if start == end {
                    return true;
                }
                if start < end {
                    now >= start && now < end
                } else {
                    now >= start || now < end
                }
            }
        }
    }
}

/// Full worker configuration (camelCase wire format for the dashboard).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub enabled: bool,
    pub name: String,
    pub relays: Vec<String>,
    pub blossom: Vec<String>,
    pub accept_audio: bool,
    pub accept_video: bool,
    pub base_msat: i64,
    pub audio_msat_per_hour: i64,
    pub video_msat_per_hour: i64,
    pub min_billable_secs: i64,
    /// Empty = accept all known platforms.
    pub allowed_platforms: Vec<String>,
    pub blocked_platforms: Vec<String>,
    pub hours: Hours,
    pub nwc_uri: Option<String>,
    pub ytdlp: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            name: default_name(),
            relays: vec![
                "wss://relay.damus.io".into(),
                "wss://nos.lol".into(),
                "wss://relay.primal.net".into(),
                "wss://relay.nostr.net".into(),
            ],
            blossom: vec![
                "https://blossom.yakihonne.com/".into(),
                "https://24242.io/".into(),
                "https://blossom.primal.net/".into(),
                "https://nostr.download/".into(),
                "https://cdn.sovbit.host/".into(),
                "https://cdn.nostrcheck.me/".into(),
            ],
            accept_audio: true,
            accept_video: true,
            base_msat: 9_000,
            audio_msat_per_hour: 60_000,
            video_msat_per_hour: 60_000,
            min_billable_secs: 60,
            allowed_platforms: vec![],
            blocked_platforms: vec![],
            hours: Hours::Always,
            nwc_uri: None,
            ytdlp: "yt-dlp".into(),
        }
    }
}

fn default_name() -> String {
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_lowercase().replace(['.', '_'], "-"))
        .unwrap_or_else(|| "desktop".into());
    format!("hench-cli-{host}")
}

fn env_list(key: &str) -> Option<Vec<String>> {
    std::env::var(key)
        .ok()
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

/// Load config.json, seeding from env/HENCH_* on first run. Never fails: bad
/// file falls back to defaults (the file is rewritten on next dashboard save).
pub fn load(data_dir: &Path, free: bool) -> Config {
    let path = data_dir.join("config.json");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(mut cfg) = serde_json::from_str::<Config>(&text) {
            if free {
                cfg.nwc_uri = None; // free mode ignores any stored wallet
            }
            return cfg;
        }
        tracing::warn!("config.json unreadable, using defaults");
    }
    let mut cfg = Config {
        nwc_uri: std::env::var("HENCH_NWC_URI").ok().filter(|s| !s.is_empty()),
        name: std::env::var("HENCH_NAME").unwrap_or_else(|_| default_name()),
        relays: env_list("HENCH_RELAYS").filter(|l| !l.is_empty()).unwrap_or_default(),
        blossom: env_list("HENCH_BLOSSOM").filter(|l| !l.is_empty()).unwrap_or_default(),
        ytdlp: std::env::var("HENCH_YTDLP").unwrap_or_else(|_| "yt-dlp".into()),
        ..Config::default()
    };
    if let Some(v) = env_parse("HENCH_BASE_MSAT") { cfg.base_msat = v; }
    if let Some(v) = env_parse("HENCH_AUDIO_MSAT_HR") { cfg.audio_msat_per_hour = v; }
    if let Some(v) = env_parse("HENCH_VIDEO_MSAT_HR") { cfg.video_msat_per_hour = v; }
    if let Some(v) = env_parse("HENCH_MIN_BILLABLE_SECS") { cfg.min_billable_secs = v; }
    cfg
}

pub fn save(data_dir: &Path, cfg: &Config) -> anyhow::Result<()> {
    // config.json holds the NWC URI: 0600 tmp file + fsync + rename so the
    // secret is never world-readable nor torn by a crash.
    let path = data_dir.join("config.json");
    crate::store::write_atomic_private(&path, serde_json::to_string_pretty(cfg)?.as_bytes())
        .map_err(|e| anyhow::anyhow!("config write failed: {e}"))
}

/// Port of WorkerSettings.validate() (the CLI-relevant issues).
pub fn validate(cfg: &Config, free: bool) -> Vec<String> {
    let mut issues = vec![];
    if cfg.enabled {
        if !cfg.relays.iter().any(|r| !r.trim().is_empty()) {
            issues.push("at least one relay URL is required".into());
        }
        if !cfg.blossom.iter().any(|b| !b.trim().is_empty()) {
            issues.push("at least one Blossom server is required".into());
        }
        if !free && cfg.nwc_uri.as_deref().map(str::trim).unwrap_or("").is_empty() {
            issues.push("an NWC wallet URI is required (or run with HENCH_FREE=1)".into());
        }
    }
    if !cfg.accept_audio && !cfg.accept_video {
        issues.push("at least one of accept audio / accept video must be on".into());
    }
    if cfg.accept_audio && cfg.audio_msat_per_hour <= 0 {
        issues.push("audio price per hour must be > 0".into());
    }
    if cfg.accept_video && cfg.video_msat_per_hour <= 0 {
        issues.push("video price per hour must be > 0".into());
    }
    if cfg.base_msat < 0 {
        issues.push("base price must be >= 0".into());
    }
    if cfg.min_billable_secs < 1 {
        issues.push("minimum billable seconds must be >= 1".into());
    }
    if !free {
        if let Some(uri) = cfg.nwc_uri.as_deref().filter(|s| !s.trim().is_empty()) {
            if nostr::nips::nip47::NostrWalletConnectUri::parse(uri).is_err() {
                issues.push("NWC connection URI is invalid".into());
            }
        }
    }
    if let Hours::Daily { start, end } = &cfg.hours {
        for t in [start, end] {
            if chrono::NaiveTime::parse_from_str(t, "%H:%M").is_err() {
                issues.push(format!("hours of operation time '{t}' must be HH:MM"));
            }
        }
    }
    let known = platform_names();
    for p in cfg.allowed_platforms.iter().chain(cfg.blocked_platforms.iter()) {
        if !known.contains(&p.as_str()) {
            issues.push(format!("unknown platform '{p}'"));
        }
    }
    issues
}
