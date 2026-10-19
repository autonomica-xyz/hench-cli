# hench-cli

**Turn any computer into a Hench worker that earns sats.**

The desktop version of [Hench](https://github.com/autonomica-xyz/hench) — part
of [autonomica.xyz](https://autonomica.xyz): a ContextVM worker that offers
paid media downloads (yt-dlp → Blossom) over Nostr. Same wire behavior as the
Android app — announcements with the `agent=hench` tag (so the
[hench-web](https://github.com/autonomica-xyz/hench-web) dashboard discovers
it), CEP-8 payment gating, `payment_required` → pay → `payment_accepted` →
job runs — but driven by the native rs-sdk instead of the uniffi FFI, with
your own wallet over NWC instead of an on-device Spark wallet, plus a
built-in web dashboard for management.

The SDK is vendored under `vendor/contextvm-sdk` (from
[nostr-net/rs-sdk](https://github.com/nostr-net/rs-sdk) @ `2cf7e6c`, MIT):
the crates.io `contextvm-sdk` 0.2.2 has no payments module and the repo's
submodule layout breaks `cargo`'s git-fetch, so a vendored path dep is the
only reproducible public checkout. Vendored changes update lru to 0.18 and
nostr to 0.45, preserve the upstream canonical invocation vectors, and retain
the SDK's regression tests.

## Run

```bash
# needs: rust toolchain, yt-dlp, ffmpeg (audio transcode + video merge), deno or node (YouTube)
export HENCH_NWC_URI="nostr+walletconnect://…"   # from Alby Hub, nwc.link, Zeus, …
cargo run --release
```

Open **http://127.0.0.1:8932** — configure settings, paste the NWC URI (it is
never echoed back), flip the worker on. First run generates the worker key
(`~/.hench-cli/secret`, or `HENCH_SK` to pin one). Payments settle straight to
whatever wallet the NWC URI points at.

Local unpaid testing: `HENCH_FREE=1 cargo run`, then hit it with the SDK's
`paid_call` example (or hench-web) against the printed pubkey.

## Web dashboard

`http://127.0.0.1:8932` by default. To expose it remotely:

```bash
export HENCH_DASH_BIND=0.0.0.0:8932
export HENCH_DASH_TOKEN=choose-a-long-token   # required for non-loopback binds
# open http://host:8932/?token=choose-a-long-token
```

Shows: worker on/off, sats earned, jobs completed/failed, uptime, recent
jobs. Settings — the phone app's set, minus the Android-only ones (Spark
wallet, cookie sessions, browser/script tools):

- worker name, Nostr relays, Blossom servers, yt-dlp binary
- accept audio / accept video
- prices: base per job, audio/video per hour, minimum billable seconds
- hours of operation (always / daily window)
- allowed & blocked platforms
- NWC wallet URI (write-only)

Polling preserves unsaved settings and recovers after a failed initial state
request. Job fields are rendered as text, never executable HTML.

Settings persist to `~/.hench-cli/config.json` (0600, created with private
permissions before any bytes) and take effect on save — the worker is rebuilt
when running. Env vars (`HENCH_*`) only seed first-run defaults. On loopback
the dashboard only serves requests addressed to its loopback Host
(DNS-rebinding defense); mutating routes additionally require a matching
Origin/Sec-Fetch-Site when a browser sends them (plain curl unaffected), and
non-loopback binds require the token.

## No double charging

Paid calls are keyed by CEP-8 canonical invocation id (client + method +
args) and recorded **before** any invoice exists — durably: `jobs.json` is
written 0600 + fsynced + atomically renamed, and if that write fails no
invoice is issued at all. Payment-initiation failures return an immediate,
correlated error; private wallet and filesystem diagnostics stay in server logs.

- replay of a finished call → the payment gate is **waived** and the cached
  result is returned (no second invoice, ever) — paid records are never
  evicted, only stale unpaid quotes are pruned
- a request that was interrupted by a restart → **rejected** with a clear
  message, never re-invoiced (`jobs.json` startup sweep)
- duplicate delivery while a job is executing → rejected as busy

Known ceiling: a crash in the seconds between *invoice issued* and
*settlement verified* can leave a paid-but-unrecorded invoice; the retry
gets a rejection, not a second charge.

## Tool

`download_media { url, mode: "audio"|"full", blossom_server? }` — priced
(base + hourly rate; the app's cookie-gated platform risk premium is dropped
on CLI), URL-safety checked at quote time (http(s) only, known-platform host,
platform allow/block, and the host must not resolve to a private/loopback
address — a check done once before download, so a hostile DNS rebind during
the fetch itself is out of scope), downloaded with system yt-dlp under a
600 s budget (full mode selects adaptive streams and ffmpeg merges to mp4).
The stderr pipe is drained throughout execution while retaining only its
last 1 KiB for errors. The artifact is uploaded to the first Blossom server
that accepts it and returned as text + `resource_link`.

### translate_segment (optional, off by default)

`translate_segment { text, source, target }` — one text segment (up to
20,000 characters) translated by an external Sparehour workload queue
(`translate-segment` / `libretranslate-v1`); the translated file is uploaded
to Blossom and returned like any other artifact. It registers only when the
`HENCH_SPAREHOUR_*` environment is complete, and is never enabled by a
partial configuration:

```sh
export HENCH_SPAREHOUR_URL="https://sparehour.example/"   # http(s) API root
export HENCH_SPAREHOUR_TOKEN="..."                        # or HENCH_SPAREHOUR_TOKEN_FILE=/path
export HENCH_SPAREHOUR_PROJECT="my-project"
export HENCH_SPAREHOUR_PRICE_SATS="21"                    # fixed price per segment
# optional: HENCH_SPAREHOUR_ROUTE (default libretranslate-v1),
#           HENCH_SPAREHOUR_MAX_PENDING (default 20),
#           HENCH_SPAREHOUR_WAIT_SECS (default 240, clamped to 540)
```

The token needs client role scoped to the project/workload/route **and**
`perms=dynamic` — the queue depth (`pending_jobs`) is only served to such
tokens, and the worker refuses to sell work whose queue it cannot see.
Payment semantics mirror the app's paid jobs: work is quoted only when the
route is available and the queue is not busy; the paid marker plus the
backend job id are persisted before/after submission, so a restart resumes
the same job (a repeat call is waived and never re-invoiced — sparehour's
idempotency key is the call's canonical id); a definitive backend failure
after payment returns a cached error result (no refund), while a slow or
briefly unavailable backend leaves the job collectable by repeating the
exact same call. The token is env/file-only, never logged, and only ever
sent to the configured base URL.

## License

MIT for Hench's own code (see [LICENSE](LICENSE)); the vendored ContextVM SDK
in `vendor/contextvm-sdk` keeps its own MIT license (Kosti Rytkönen).
