//! Persistent job store — the CLI port of the app's PaymentSessionStore
//! replay/idempotency contract, minus the Android crash-recovery invoice
//! rebinding (the SDK middleware owns invoicing, so an interrupted payment
//! window rejects instead of rebinding).
//!
//! Canonical invocation id -> job record. Terminal records (COMPLETED/FAILED)
//! cache the MCP result; replays are waived through the payment gate and
//! answered from this cache, so a client retry never triggers a second
//! invoice. File: <data_dir>/jobs.json, write-fsync-tmp+rename+dir-fsync,
//! 0600 from creation.
//!
//! Durability contract: PAID records are never evicted — a paid job's record
//! is the idempotency promise ("no second invoice, ever"), so unpaid request
//! churn cannot erase it. Only stale unpaid WAITING quotes (older than
//! UNPAID_TTL_SECS, far beyond the 600 s invoice lifetime) are pruned, and
//! writes propagate errors so callers can refuse to issue invoices without
//! durable state.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

pub const WAITING: &str = "WAITING";
pub const COMPLETED: &str = "COMPLETED";
pub const FAILED: &str = "FAILED";

/// Unpaid WAITING quotes expire with their invoice (600 s) long before this.
const UNPAID_TTL_SECS: u64 = 3600;
/// Refuse new invoices at capacity; never evict a live quote to admit one.
const MAX_FRESH_UNPAID: usize = 10_000;

/// Create `dir` (and parents) with 0700 from the start — the data dir holds
/// the worker secret and wallet-bearing config.
pub(crate) fn ensure_dir_private(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // Pre-existing dirs (or shared umask corners) get tightened too.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// Crash-safe write for secret-bearing files: tmp file created 0600 (before
/// any bytes exist), written, fsynced, renamed, then the directory is fsynced
/// so the rename survives power loss.
pub(crate) fn write_atomic_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("data")
    ));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        use std::io::Write;
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub canonical_id: String,
    pub status: String,
    pub amount_msat: i64,
    pub url: String,
    pub mode: String,
    pub client: String,
    /// Cached terminal MCP result (`{content, isError}`) for replays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    pub updated_at: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Data {
    jobs: Vec<JobRecord>, // newest first
}

pub struct JobStore {
    path: PathBuf,
    data: Mutex<Data>,
    /// Canonical ids currently executing (in-memory only; process-local).
    pub inflight: Mutex<HashSet<String>>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl JobStore {
    pub fn open(data_dir: &std::path::Path) -> anyhow::Result<Self> {
        ensure_dir_private(data_dir)?;
        let path = data_dir.join("jobs.json");
        let mut data: Data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // Sweep: anything left WAITING by a previous process died with it.
        let swept = data.jobs.iter().any(|j| j.status == WAITING);
        for job in data.jobs.iter_mut() {
            if job.status == WAITING {
                job.status = FAILED.into();
                job.result = Some(error_result(
                    "INTERRUPTED",
                    "worker restarted during execution",
                ));
                job.updated_at = now();
            }
        }
        let store = Self {
            path,
            data: Mutex::new(data),
            inflight: Mutex::new(HashSet::new()),
        };
        if swept {
            store.persist(&store.data.lock().unwrap())?;
            tracing::info!("startup sweep: interrupted WAITING jobs marked FAILED");
        }
        Ok(store)
    }

    /// Commit the job ledger to disk (0600 tmp + fsync + rename + dir fsync).
    /// Errors propagate — payment state must never be silently memory-only.
    fn persist(&self, data: &Data) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(data)?;
        write_atomic_private(&self.path, json.as_bytes())
            .map_err(|e| anyhow::anyhow!("job store write failed ({}): {e}", self.path.display()))
    }

    pub fn get(&self, canonical_id: &str) -> Option<JobRecord> {
        self.data
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|j| j.canonical_id == canonical_id)
            .cloned()
    }

    #[cfg(test)]
    pub fn is_terminal(&self, canonical_id: &str) -> bool {
        self.get(canonical_id)
            .map(|j| j.status == COMPLETED || j.status == FAILED)
            .unwrap_or(false)
    }

    /// Admit a new WAITING record and commit it durably. Must succeed
    /// before an invoice may be issued — callers refuse otherwise.
    pub fn mark_waiting(&self, mut rec: JobRecord) -> anyhow::Result<()> {
        if rec.updated_at == 0 {
            rec.updated_at = now();
        }
        let canonical = rec.canonical_id.clone();
        let mut data = self.data.lock().unwrap();
        anyhow::ensure!(
            !data.jobs.iter().any(|j| j.canonical_id == canonical),
            "job record already exists for {canonical}"
        );
        data.jobs.insert(0, rec);
        // Evict only unpaid quotes whose invoice window is long dead; paid
        // and in-flight records are the durable idempotency ledger.
        let cutoff = now().saturating_sub(UNPAID_TTL_SECS);
        let inflight = self.inflight.lock().unwrap();
        data.jobs.retain(|j| {
            j.status != WAITING || j.updated_at > cutoff || inflight.contains(&j.canonical_id)
        });
        drop(inflight);
        if data
            .jobs
            .iter()
            .filter(|j| j.status == WAITING)
            .take(MAX_FRESH_UNPAID + 1)
            .count()
            > MAX_FRESH_UNPAID
        {
            data.jobs.retain(|j| j.canonical_id != canonical);
            anyhow::bail!("too many pending invoices");
        }
        if let Err(e) = self.persist(&data) {
            // No durable record => the request never existed: a memory-only
            // entry would let the quoter route a replay into an invoiced path
            // the disk will not remember.
            data.jobs.retain(|j| j.canonical_id != canonical);
            return Err(e);
        }
        Ok(())
    }

    /// Transition to a terminal state and commit durably. On failure the
    /// in-memory record still lands (the live request deserves its answer)
    /// but the error is reported so data loss is never silent.
    pub fn mark_terminal(
        &self,
        canonical_id: &str,
        status: &str,
        result: Option<Value>,
    ) -> anyhow::Result<()> {
        let mut data = self.data.lock().unwrap();
        if let Some(job) = data
            .jobs
            .iter_mut()
            .find(|j| j.canonical_id == canonical_id)
        {
            job.status = status.to_string();
            job.result = result;
            job.updated_at = now();
            self.persist(&data)
        } else {
            anyhow::bail!("no job record for {canonical_id}")
        }
    }

    /// Latest N jobs for the dashboard.
    pub fn recent(&self, n: usize) -> Vec<JobRecord> {
        self.data
            .lock()
            .unwrap()
            .jobs
            .iter()
            .take(n)
            .cloned()
            .collect()
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        // (earned_msat, completed, failed)
        self.data
            .lock()
            .unwrap()
            .jobs
            .iter()
            .fold((0, 0, 0), |(msat, c, f), j| {
                if j.status == COMPLETED {
                    (msat.saturating_add(j.amount_msat.max(0) as u64), c + 1, f)
                } else if j.status == FAILED {
                    (msat, c, f + 1)
                } else {
                    (msat, c, f)
                }
            })
    }

    pub fn inflight(&self, canonical_id: &str) -> bool {
        self.inflight.lock().unwrap().contains(canonical_id)
    }

    pub fn inflight_add(&self, canonical_id: &str) -> bool {
        self.inflight
            .lock()
            .unwrap()
            .insert(canonical_id.to_string())
    }

    pub fn inflight_remove(&self, canonical_id: &str) {
        self.inflight.lock().unwrap().remove(canonical_id);
    }
}

/// Error tool result in the MCP CallToolResult shape the handler replies with.
pub fn error_result(code: &str, message: &str) -> Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": format!("{{\"code\":\"{code}\",\"message\":{}}}", serde_json::to_string(message).unwrap_or_default()) }],
        "isError": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(canonical: &str, status: &str) -> JobRecord {
        JobRecord {
            canonical_id: canonical.into(),
            status: status.into(),
            amount_msat: 10_000,
            url: "https://x.com/x".into(),
            mode: "audio".into(),
            client: "aa".repeat(32),
            result: None,
            updated_at: 0,
        }
    }

    #[test]
    fn startup_sweep_fails_waiting_jobs() {
        let dir = std::env::temp_dir().join(format!("hench-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("jobs.json"),
            serde_json::to_string(&Data {
                jobs: vec![rec("c1", WAITING), rec("c2", COMPLETED)],
            })
            .unwrap(),
        )
        .unwrap();
        let store = JobStore::open(&dir).unwrap();
        assert_eq!(store.get("c1").unwrap().status, FAILED);
        assert_eq!(store.get("c2").unwrap().status, COMPLETED);
        // reopened store keeps the sweep (persisted)
        drop(store);
        let store = JobStore::open(&dir).unwrap();
        assert_eq!(store.get("c1").unwrap().status, FAILED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminal_records_cache_results_for_replays() {
        let dir = std::env::temp_dir().join(format!("hench-store-test2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = JobStore::open(&dir).unwrap();
        store.mark_waiting(rec("c3", WAITING)).unwrap();
        assert!(!store.is_terminal("c3"));
        store
            .mark_terminal(
                "c3",
                COMPLETED,
                Some(serde_json::json!({"content":[],"isError":false})),
            )
            .unwrap();
        assert!(store.is_terminal("c3"));
        let (earned, done, failed) = store.stats();
        assert_eq!((earned, done, failed), (10_000, 1, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paid_ledger_survives_unpaid_churn() {
        // Regression: a flat 200-record cap let unpaid requests evict the paid
        // record, so a replay could be re-invoiced. Paid/interrupted records
        // are the durable ledger; only stale unpaid quotes are pruned.
        let dir = std::env::temp_dir().join(format!("hench-store-test3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = JobStore::open(&dir).unwrap();
        store.mark_waiting(rec("paid", WAITING)).unwrap();
        store.mark_terminal("paid", COMPLETED, None).unwrap();
        for i in 0..250 {
            // > the old 200-record cap that used to evict this paid record
            store.mark_waiting(rec(&format!("u{i}"), WAITING)).unwrap();
        }
        assert!(store.is_terminal("paid"));
        drop(store);
        let store = JobStore::open(&dir).unwrap();
        assert!(store.is_terminal("paid"));
        // Stale unpaid quotes (their invoice window is long dead) are pruned.
        let mut stale = rec("stale", WAITING);
        stale.updated_at = 1;
        store.mark_waiting(stale).unwrap();
        assert!(store.get("stale").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invoice_admission_preserves_live_quotes_and_running_jobs() {
        let dir =
            std::env::temp_dir().join(format!("hench-store-admission-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = JobStore::open(&dir).unwrap();
        {
            let mut data = store.data.lock().unwrap();
            data.jobs = (0..MAX_FRESH_UNPAID)
                .map(|i| {
                    let mut job = rec(&format!("live-{i}"), WAITING);
                    job.updated_at = now();
                    job
                })
                .collect();
            data.jobs[0].updated_at = 1;
            store.persist(&data).unwrap();
        }
        store.inflight_add("live-0");
        assert!(store.mark_waiting(rec("overflow", WAITING)).is_err());
        assert_eq!(store.get("live-0").unwrap().status, WAITING);
        assert_eq!(
            store
                .get(&format!("live-{}", MAX_FRESH_UNPAID - 1))
                .unwrap()
                .status,
            WAITING
        );
        assert!(store.get("overflow").is_none());
        drop(store);
        let store = JobStore::open(&dir).unwrap();
        assert_eq!(store.get("live-0").unwrap().status, FAILED);
        assert!(store.get("overflow").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_persistence_blocks_waiting() {
        // Regression: persist errors only logged; quoting proceeded anyway and
        // a restart could re-invoice a paid job. mark_waiting must fail AND
        // leave no memory-only record the quoter could misread.
        let dir = std::env::temp_dir().join(format!("hench-store-test4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("jobs.json")).unwrap(); // rename target blocked
        let store = JobStore::open(&dir).unwrap();
        assert!(store.mark_waiting(rec("c9", WAITING)).is_err());
        assert!(store.get("c9").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
