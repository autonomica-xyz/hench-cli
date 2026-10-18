# hench-cli

**Turn any computer into a Hench worker that earns sats.**

The desktop version of [Hench](../hench): a ContextVM worker that offers paid
media downloads (yt-dlp → Blossom) over Nostr. Same wire behavior as the
Android app — announcements with the `agent=hench` tag (so the
[hench-web](../hench-web) dashboard discovers it), CEP-8 payment gating,
`payment_required` → pay → `payment_accepted` → job runs — but driven by the
native [rs-sdk](../rs-sdk) instead of the uniffi FFI, with your own wallet
over NWC instead of an on-device Spark wallet, plus a built-in web dashboard
for management.

## Run

```bash
# needs: rust toolchain, yt-dlp on PATH
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

Settings persist to `~/.hench-cli/config.json` (0600) and take effect on
save — the worker is rebuilt when running. Env vars (`HENCH_*`) only seed
first-run defaults.

## No double charging

Paid calls are keyed by CEP-8 canonical invocation id (client + method +
args) and recorded durably **before** any invoice exists:

- replay of a finished call → the payment gate is **waived** and the cached
  result is returned (no second invoice, ever)
- a request that was interrupted by a restart → **rejected** with a clear
  message, never re-invoiced (`jobs.json` startup sweep)
- duplicate delivery while a job is executing → rejected as busy

Known ceiling: a crash in the seconds between *invoice issued* and
*settlement verified* can leave a paid-but-unrecorded invoice; the retry
gets a rejection, not a second charge.

## Tool

`download_media { url, mode: "audio"|"full", blossom_server? }` — priced
(base + hourly rate; the app's cookie-gated platform risk premium is dropped
on CLI), URL-safety checked (scheme, platform allow/block, DNS-rebinding
defense — ported from the app), downloaded with system yt-dlp under a 600 s
budget, uploaded to the first Blossom server that accepts it, result returned
as text + `resource_link`.
