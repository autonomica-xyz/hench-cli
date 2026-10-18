//! download_media: URL safety, yt-dlp execution, Blossom upload.
//! Ports hench's UrlSafety network contract and HttpBlossomClient.
use crate::config::PLATFORMS;
use nostr_sdk::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

fn ip_is_private(ip: &IpAddr) -> bool {
    let v6_checks = |v6: &std::net::Ipv6Addr| {
        let s = v6.segments();
        v6.is_loopback()
            || v6.is_unspecified()
            || v6.is_multicast()
            || (s[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
            || (s[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
    };
    // Native-v6 checks run BEFORE to_ipv4(): ::1.to_ipv4() == 0.0.0.1, which
    // would slide past every v4 rule and pass as "public".
    if let IpAddr::V6(v6) = ip {
        if v6_checks(v6) {
            return true;
        }
    }
    // ::ffff:a.b.c.d (and deprecated ::a.b.c.d) take the v4 rules — a mapped
    // loopback/private address is still loopback/private (SSRF bypass fix).
    let v4 = match ip {
        IpAddr::V4(v4) => Some(*v4),
        IpAddr::V6(v6) => v6.to_ipv4(),
    };
    if let Some(v4) = v4 {
        let o = v4.octets();
        return v4.is_loopback()
            || v4.is_private()
            || v4.is_link_local() // 169.254/16
            || v4.is_unspecified()
            || v4.is_multicast()
            || v4.is_broadcast()
            || (o[0] == 100 && (o[1] & 0xC0) == 64); // CGNAT 100.64/10
    }
    false
}

fn platform_for_host(host: &str) -> Option<&'static str> {
    PLATFORMS
        .iter()
        .find(|(_, domains)| {
            domains
                .iter()
                .any(|d| host == *d || host.ends_with(&format!(".{d}")))
        })
        .map(|(name, _)| *name)
}

/// Port of WorkerSettings.acceptsUrl: empty allowed = all known, blocked wins.
fn platform_accepted(platform: &str, allowed: &[String], blocked: &[String]) -> bool {
    if blocked.iter().any(|b| b == platform) {
        return false;
    }
    allowed.is_empty() || allowed.iter().any(|a| a == platform)
}

fn host_resolves_public(host: &str) -> Result<(), String> {
    let addrs: Vec<_> = (host, 80)
        .to_socket_addrs()
        .map_err(|_| format!("DNS resolution failed for {host}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("DNS resolution failed for {host}"));
    }
    if addrs.iter().any(|a| ip_is_private(&a.ip())) {
        return Err(format!(
            "host {host} resolves to a private/loopback address"
        ));
    }
    Ok(())
}

/// Network-safety validation of the source URL (deny-by-default, ported from
/// hench UrlSafety.checkSource + WorkerSettings.acceptsUrl): http(s) only,
/// known extractor domain, platform allow/block, public DNS.
pub fn check_source(
    url: &str,
    allowed_platforms: &[String],
    blocked_platforms: &[String],
) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|_| "unparseable URL".to_string())?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("scheme '{scheme}' not allowed"));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "no host".to_string())?
        .to_lowercase();
    let platform =
        platform_for_host(&host).ok_or_else(|| format!("no extractor for host {host}"))?;
    if !platform_accepted(platform, allowed_platforms, blocked_platforms) {
        return Err(format!("platform '{platform}' not accepted by this worker"));
    }
    host_resolves_public(&host)
}

/// Validate a Blossom server against the allowlist (ported from checkBlossom).
pub fn check_blossom(server: &str, allowlist: &[String]) -> Result<(), String> {
    if !allowlist
        .iter()
        .any(|a| a.trim_end_matches('/') == server.trim_end_matches('/'))
    {
        return Err("blossom server not in allowlist".into());
    }
    let host = url::Url::parse(server)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .ok_or_else(|| "blossom server has no host".to_string())?;
    host_resolves_public(&host)
}

pub fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        // translate_segment writes translation.txt; yt-dlp never produces
        // .txt, so download_media is unaffected by this arm.
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// True when `prog` resolves and runs on PATH (or as a path).
fn on_path(prog: &str) -> bool {
    std::process::Command::new(prog)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Pick a JS runtime for yt-dlp's EJS challenges (YouTube): deno is the
/// upstream default-enabled one, node works via --js-runtimes. None found ->
/// omit the flag; platforms that don't need EJS still work.
fn js_runtime_arg() -> Option<&'static str> {
    ["deno", "node"].into_iter().find(|rt| on_path(rt))
}

/// Run yt-dlp for one job into `out_dir`; returns the produced file.
pub async fn run_ytdlp(
    ytdlp: &str,
    url: &str,
    audio: bool,
    out_dir: &Path,
    budget: std::time::Duration,
) -> Result<PathBuf, String> {
    tokio::fs::create_dir_all(out_dir)
        .await
        .map_err(|e| format!("mkdir failed: {e}"))?;
    let output_template = out_dir.join("%(id)s.%(ext)s");
    let mut cmd = tokio::process::Command::new(ytdlp);
    cmd.arg("--no-playlist")
        .arg("--restrict-filenames")
        .arg("--no-update")
        .arg("--socket-timeout")
        .arg("30")
        .arg("-o")
        .arg(&output_template);
    if let Some(rt) = js_runtime_arg() {
        cmd.arg("--js-runtimes").arg(rt);
    }
    if audio {
        cmd.arg("-x").arg("--audio-format").arg("mp3");
    } else {
        // YouTube (and most platforms) now serve adaptive-only streams — no
        // precombined 'best'. Select video+audio and let ffmpeg merge to mp4.
        cmd.arg("--format")
            .arg("bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/bv*+ba/b")
            .arg("--merge-output-format")
            .arg("mp4");
    }
    cmd.arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {ytdlp}: {e} (is yt-dlp installed?)"))?;

    const STDERR_CAP: usize = 1024;
    let stderr = child.stderr.take();
    // Drain the pipe to avoid SIGPIPE, retaining only the final error context.
    let collect = async move {
        let mut buf = std::collections::VecDeque::with_capacity(STDERR_CAP);
        if let Some(mut s) = stderr {
            let mut chunk = [0_u8; 8192];
            while let Ok(n) = s.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                let recent = &chunk[n.saturating_sub(STDERR_CAP)..n];
                buf.drain(..(buf.len() + recent.len()).saturating_sub(STDERR_CAP));
                buf.extend(recent);
            }
        }
        buf
    };
    let (status, mut bytes) =
        match tokio::time::timeout(budget, async { tokio::join!(child.wait(), collect) }).await {
            Ok((Ok(status), bytes)) => (status, bytes),
            Ok((Err(e), _)) => return Err(format!("yt-dlp wait failed: {e}")),
            Err(_) => return Err(format!("execution timed out after {}s", budget.as_secs())),
        };
    if !status.success() {
        let err = String::from_utf8_lossy(bytes.make_contiguous());
        return Err(format!("yt-dlp exited {status}: {err}"));
    }

    let mut files: Vec<PathBuf> = vec![];
    let mut stack = vec![out_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&dir).await.map_err(|e| e.to_string())?;
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|e| e != "part").unwrap_or(true) {
                files.push(p);
            }
        }
    }
    files
        .into_iter()
        .max_by_key(|p| p.metadata().map(|m| m.len()).unwrap_or(0))
        .ok_or_else(|| "yt-dlp produced no output file".to_string())
}

pub struct Upload {
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// NIP-24242 upload authorization header, signed with the worker key.
fn blossom_auth_header(server: &str, sha256_hex: &str, keys: &Keys) -> Result<String, String> {
    let domain = url::Url::parse(server)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
        .ok_or_else(|| format!("blossom server URL must include a host: {server}"))?;
    let event = EventBuilder::new(Kind::Custom(24242), "Upload Blob")
        .tags([
            Tag::custom("t", ["upload"]),
            Tag::expiration(Timestamp::from_secs(
                Timestamp::now().as_secs().saturating_add(600),
            )),
            Tag::custom("server", [domain]),
            Tag::custom("x", [sha256_hex.to_lowercase()]),
        ])
        .finalize(keys)
        .map_err(|e| format!("auth event signing failed: {e}"))?;
    let json = serde_json::to_string(&event).map_err(|e| e.to_string())?;
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    Ok(format!("Nostr {b64}"))
}

/// Stream-upload a file to the first Blossom server that accepts it.
pub async fn blossom_upload(
    client: &reqwest::Client,
    servers: &[String],
    file: &Path,
    keys: &Keys,
) -> Result<Upload, String> {
    let mut hasher = Sha256::new();
    let mut f = tokio::fs::File::open(file)
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let sha256_hex = hex::encode(hasher.finalize());
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mime = mime_for(file);

    let mut last_err = String::from("no blossom servers configured");
    for server in servers {
        let url = format!("{}/upload", server.trim_end_matches('/'));
        let auth = blossom_auth_header(server, &sha256_hex, keys)?;
        // ponytail: re-open and stream the file per server; retries are rare, buffering 500 MB blobs in RAM is worse.
        let f = tokio::fs::File::open(file)
            .await
            .map_err(|e| e.to_string())?;
        let stream = tokio_util::io::ReaderStream::with_capacity(f, 1024 * 1024);
        let body = reqwest::Body::wrap_stream(stream);
        let resp = client
            .put(&url)
            .header("X-SHA-256", &sha256_hex)
            .header(reqwest::header::CONTENT_TYPE, mime)
            // A streamed body is sent chunked unless the length is declared;
            // BUD-02 servers may require Content-Length (seen in e2e).
            .header(reqwest::header::CONTENT_LENGTH, size)
            .header(reqwest::header::AUTHORIZATION, auth)
            .body(body)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let json: serde_json::Value = r
                    .json()
                    .await
                    .map_err(|e| format!("blossom response parse failed: {e}"))?;
                let uploaded = json
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or("blossom response missing url")?
                    .to_string();
                // A malicious/misconfigured server can echo an internal or
                // cleartext URL for our bytes; only return https URLs.
                if url::Url::parse(&uploaded)
                    .map(|u| u.scheme() != "https")
                    .unwrap_or(true)
                {
                    last_err = format!("{server}: unsafe blob url returned");
                    tracing::warn!("blossom server {server} returned a non-https url, skipping");
                    continue;
                }
                return Ok(Upload {
                    url: uploaded,
                    sha256: sha256_hex,
                    size,
                });
            }
            Ok(r) => {
                let reason = r
                    .headers()
                    .get("X-Reason")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("HTTP {}", r.status()));
                last_err = format!("{server}: {reason}");
                tracing::warn!("blossom upload failed at {server}: {reason}");
            }
            Err(e) => {
                last_err = format!("{server}: {e}");
                tracing::warn!("blossom upload failed at {server}: {e}");
            }
        }
    }
    Err(format!("blossom upload failed: {last_err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blossom_upload_declares_content_length() {
        // Regression (seen against a real BUD-02 server in e2e): the streamed
        // body went out chunked with no Content-Length and was refused.
        use axum::http::{header, HeaderMap, StatusCode};
        async fn upload(headers: HeaderMap, body: axum::body::Bytes) -> (StatusCode, String) {
            let declared = headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<usize>().ok());
            if declared != Some(body.len()) || headers.contains_key(header::TRANSFER_ENCODING) {
                return (StatusCode::LENGTH_REQUIRED, "missing Content-Length".into());
            }
            (
                StatusCode::OK,
                serde_json::json!({ "url": "https://cdn.example.com/blob" }).to_string(),
            )
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = axum::Router::new().route("/upload", axum::routing::put(upload));
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let dir = std::env::temp_dir().join(format!("hench-blossom-cl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("blob.txt");
        std::fs::write(&file, b"hello blossom").unwrap();
        let up = blossom_upload(
            &reqwest::Client::new(),
            &[format!("http://{addr}/")],
            &file,
            &Keys::generate(),
        )
        .await
        .expect("upload with Content-Length");
        assert_eq!(up.size, 13);
        let _ = std::fs::remove_dir_all(&dir);
    }
    use std::net::Ipv6Addr;

    #[test]
    fn v6_mapped_v4_loopback_and_private_are_private() {
        // Regression: ::ffff:127.0.0.1 and ::ffff:10.0.0.1 previously passed
        // the v6 arm and bypassed the SSRF DNS check.
        let mapped_loop: IpAddr = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001).into();
        let mapped_priv: IpAddr = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0a00, 0x0001).into();
        assert!(ip_is_private(&mapped_loop));
        assert!(ip_is_private(&mapped_priv));
        // Unmapped publics stay public.
        let public: IpAddr = Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 1).into();
        assert!(!ip_is_private(&public));
        let v4_public: IpAddr = "1.1.1.1".parse().unwrap();
        assert!(!ip_is_private(&v4_public));
    }

    #[test]
    fn well_known_private_ranges_blocked() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
        ] {
            assert!(ip_is_private(&ip.parse().unwrap()), "{ip}");
        }
        for ip in ["::1", "fc00::1", "fe80::1"] {
            assert!(ip_is_private(&ip.parse().unwrap()), "{ip}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verbose_download_drains_stderr_and_keeps_final_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("hench-media-stderr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let executable = dir.join("downloader");
        let output = dir.join("output");
        let prefix = "#!/bin/sh\nset -e\nwhile [ \"$1\" != '-o' ]; do shift; done\nshift\noutdir=${1%/*}\ndd if=/dev/zero bs=4096 count=32 2>/dev/null | tr '\\000' x >&2\n";
        std::fs::write(
            &executable,
            format!("{prefix}printf artifact > \"$outdir/result.mp3\"\n"),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let artifact = run_ytdlp(
            executable.to_str().unwrap(),
            "https://example.invalid/video",
            true,
            &output,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(artifact).unwrap(), b"artifact");

        std::fs::write(
            &executable,
            format!("{prefix}printf '\\nFINAL_DOWNLOAD_ERROR\\n' >&2\nexit 1\n"),
        )
        .unwrap();
        let error = run_ytdlp(
            executable.to_str().unwrap(),
            "https://example.invalid/video",
            true,
            &output,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(error.contains("FINAL_DOWNLOAD_ERROR"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
