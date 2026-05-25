# shitmail-jmap-filter — Implementation Plan

A Rust worker that watches a Fastmail inbox over JMAP push, looks up the
age of each sender's DKIM-verified domain, and moves mail from domains
less than one year old into a `quarantine` folder. Packaged as a
container and deployed to fly.io.

## 1. Constraints recap

- Runs as a container.
- Runs on fly.io as a long-lived worker (single machine, always-on).
- Implemented in Rust using the
  [`jmap-client`](https://github.com/stalwartlabs/jmap-client) crate
  for all JMAP interactions.
- Minimal dependencies beyond `jmap-client` (everything else is either
  already a transitive of `jmap-client` or a small standard crate).
- Inputs: a Fastmail JMAP API token (bearer).
- Trigger: JMAP push notifications (EventSource) for `Email`/`Mailbox`
  state changes.
- Decision: domain age of the *true DKIM-verified* signing domain
  (`d=` from a passing DKIM signature in the server-added
  `Authentication-Results` header), not the `From:` header domain.
- Action: if age < 365 days, move the message to `quarantine` (create
  the mailbox if missing).
- Logs all activity to stdout as JSON lines.

## 2. Why Rust + `jmap-client` (history)

The original plan targeted Node.js with `jmapio/jmap-js`. That crate
turned out to be browser-only (built on Fastmail's Overture MVC
framework, no `package.json`, requires the full Overture data store /
run loop to function). Switching the implementation language to Rust
and the JMAP library to `stalwartlabs/jmap-client` gets us:

- A proper async client with bearer auth, mailbox/email/event-source
  helpers, and request batching.
- A small dependency footprint (the additional crates we pull on top
  of `jmap-client` are either already its transitives — `reqwest`,
  `tokio`, `serde`, `chrono`, `futures-util` — or single-purpose
  utilities).
- A single statically-linked binary in the container — no runtime
  toolchain.

## 3. Dependencies

Runtime crates:
- `jmap-client` — JMAP (per requirement).
- `tokio` — async runtime (`rt-multi-thread`, `macros`, `signal`,
  `fs`, `sync`, `time`).
- `reqwest` — HTTPS client for RDAP. Already a transitive of
  `jmap-client`; declared directly so we control features.
- `serde` + `serde_json` — JSON for RDAP responses, state files,
  and structured log lines.
- `futures-util` — `StreamExt` to consume the EventSource stream.
- `chrono` — date math for domain age.
- `anyhow` — error plumbing.

That's the full list. We deliberately avoid:
- A logging framework (a ~40-line `log.rs` writes JSON lines).
- A whois library (RDAP is JSON over HTTPS — `reqwest` is enough).
- A PSL crate (we vendor the Public Suffix List and parse it in
  ~80 lines).
- A DKIM / mail-auth crate (we hand-parse `Authentication-Results`).

## 4. Architecture

```
+----------------------+      EventSource (SSE)      +-------------+
| fly.io machine       | <-------------------------- | Fastmail    |
|                      |                             | JMAP server |
|  +---------------+   |  ---- JMAP HTTP calls ----> |             |
|  | push loop     |   |                             +-------------+
|  +-------+-------+   |
|          |           |
|          v           |       HTTPS (RDAP)          +-------------+
|  +---------------+   | --------------------------> | RDAP        |
|  | filter worker |   |                             | registries  |
|  +-------+-------+   |                             +-------------+
|          |           |
|          v           |
|  /data: jmap-state.json, rdap-cache.json, retry-queue.json
+----------------------+
```

Single tokio process. One in-flight EventSource. A bounded
`mpsc::channel` queue feeds the worker so the push handler and the
retry poller never race on the same `Email/set` call (concurrency=1).

## 5. Module layout

```
Cargo.toml
src/
  main.rs              entry point, wiring, signal handling
  config.rs            env parsing, defaults, validation
  log.rs               JSON-line logger to stdout
  jmap.rs              jmap-client setup, mailbox ensure, email ops
  push.rs              EventSource subscribe, reconnect, delta sweep
  dkim.rs              parse Authentication-Results, pick verified d=
  domain/
    mod.rs
    psl.rs             eTLD+1 from vendored Public Suffix List
    rdap.rs            RDAP bootstrap + lookup + age computation
    cache.rs           on-disk JSON cache w/ TTL (positive + negative)
  retry.rs             persistent deferred-validation queue + poller
  state.rs             persist last JMAP state strings to /data
  policy.rs            "younger than N days?" decision
data/
  public_suffix_list.dat   vendored PSL snapshot
tests/
  dkim.rs
  psl.rs
  policy.rs
  rdap.rs              (mockito or local http test server)
Dockerfile
fly.toml
```

## 6. Runtime flow

1. **Boot.**
   - Read env: `FASTMAIL_API_TOKEN` (required), `JMAP_SESSION_URL`
     (default `https://api.fastmail.com/jmap/session`),
     `QUARANTINE_MAILBOX_NAME` (default `quarantine`),
     `MAX_DOMAIN_AGE_DAYS` (default `365`),
     `RETRY_INTERVAL_MIN` (default `15`),
     `RETRY_MAX_ATTEMPTS` (default `32`),
     `STATE_DIR` (default `/data`).
   - Build `Client` via
     `Client::new().credentials(token).connect(session_url).await?`.
   - Resolve Inbox via `mailbox_query(Filter::role(Role::Inbox))`.
     Resolve quarantine by name; create with `mailbox_create` if
     missing.
   - Load last persisted JMAP state from `/data/jmap-state.json` (if
     any).

2. **Catch-up sweep.**
   - If we have a persisted state, call `email_changes(since_state)`
     and process each created id whose mailbox set still includes
     Inbox.
   - If no persisted state (cold start), record `state = "now"` from
     the first push event onward — do **not** sweep existing Inbox
     mail (per user direction).
   - Persist new state on every successful round.

3. **Subscribe to push.**
   - `client.event_source([DataType::Email, DataType::Mailbox], false, 60, None)`
     yields a stream of `PushNotification::StateChange`.
   - For each event whose primary account has an `Email` state newer
     than what we have, run an `email_changes`-driven sweep.
   - On disconnect/error: exponential backoff (1s → 30s cap, jitter),
     reopen the EventSource, resume from persisted state.

4. **Per-email processing** (`process_email(id)`):
   1. `email_get(&id, [Subject, From, MailboxIds, ReceivedAt, Header(Authentication-Results, Raw, all=true)])`.
   2. If the message no longer has Inbox in `mailboxIds`, skip + log.
   3. Parse the raw `Authentication-Results` header(s). Keep only
      those whose `authserv-id` matches Fastmail's trusted IDs
      (config-driven allow-list, default
      `["fastmail.com", "messagingengine.com"]`). Within those, pick
      DKIM verdicts with `dkim=pass` and extract `header.d`.
   4. **No passing DKIM → quarantine immediately**
      (`reason = unsigned`), log `email.moved`, done.
   5. Resolve eTLD+1 of `header.d` via PSL.
   6. Look up domain age via cache → RDAP:
      - On success: continue.
      - On failure / unknown TLD: enqueue in the retry queue with
        `attempts=0, next_attempt_at = now + RETRY_INTERVAL_MIN`,
        log `email.deferred`. Leave the message in Inbox. Done.
   7. If age < `MAX_DOMAIN_AGE_DAYS` →
      `email_set_mailboxes(&id, [&quarantine_id])`.
   8. Log a structured line: id, from, dkim domain, registrable
      domain, age_days, action, latency_ms.

5. **Shutdown.**
   - SIGTERM/SIGINT: close EventSource, flush state + cache, exit 0.

## 7. DKIM-verified-domain extraction (`dkim.rs`)

- Pull all `Authentication-Results` headers via
  `Property::Header(Header { name: "Authentication-Results", form: Raw, all: true })`.
- Filter to A-R lines whose `authserv-id` matches the configured
  trusted set (sender-supplied A-R must be ignored).
- Parse per RFC 8601 §2.2: split entries on `;`, then per entry
  extract method, result, and `header.d` / `header.i` properties.
- Keep entries where `method == "dkim" && result == "pass"`.
- Prefer the entry whose `header.d` aligns (equals or is a parent of)
  the `From:` registrable domain. Fall back to the first passing
  entry.
- Returns `Option<String>` (the verified signing domain).

Tested with fixture files: aligned pass, unaligned pass, mixed
pass+fail, no DKIM, multiple chained A-R headers, sender-forged A-R.

## 8. Domain-age lookup (`domain/rdap.rs`)

- `domain/cache.rs`:
  - In-memory `HashMap<String, CacheEntry>` behind `tokio::sync::Mutex`.
  - Persisted to `/data/rdap-cache.json`, debounced 5s after writes.
  - Positive TTL: 30 days. Negative TTL: 1 hour.
- `domain/rdap.rs`:
  - On first use, fetch the IANA RDAP bootstrap
    `https://data.iana.org/rdap/dns.json`; build TLD → base URL map.
    Cached to disk; refreshed daily.
  - For `example.com`: `GET <base>domain/example.com` with
    `Accept: application/rdap+json`, 10s timeout.
  - Parse `events[]` for `eventAction == "registration"`; the
    `eventDate` (RFC 3339) is the registration date.
  - Errors:
    - `RdapError::UnknownTld` — TLD not in bootstrap.
    - `RdapError::Lookup(source)` — timeout / 5xx / malformed JSON.
    Both cause the caller to enqueue for deferred retry (§10).

## 9. Policy (`policy.rs`)

A single pure function:

```rust
pub fn should_quarantine(age_days: i64, max_age_days: i64) -> bool {
    age_days < max_age_days
}
```

Unsigned-quarantine and unknown-age-defers-decision are both handled
upstream (in step 4 and step 6 of §6 respectively), keeping this
function trivial and exhaustively testable.

## 10. Deferred validation queue (`retry.rs`)

Purpose: when RDAP can't answer right now, we don't lose the message
or guess. The decision is deferred and retried.

Storage:
- `/data/retry-queue.json`, written atomically (`write to .tmp` →
  `rename`).
- Schema:
  ```json
  [{ "id": "...", "registrable_domain": "...",
     "first_seen_at": "...", "attempts": 0,
     "next_attempt_at": "...", "last_error": "..." }]
  ```

Scheduler:
- A `tokio::time::interval(Duration::from_secs(RETRY_INTERVAL_MIN * 60))`
  ticks the poller. On each tick, for every entry whose
  `next_attempt_at <= now`:
  1. `email_get(&id, [MailboxIds])`. If Inbox is no longer in the
     set (user moved/deleted), drop entry, log
     `retry.dropped reason=moved`.
  2. Re-attempt RDAP (cache may have warmed in the meantime).
  3. On success: run the policy, move or keep, drop entry, log
     `retry.resolved`.
  4. On failure: `attempts += 1`, set
     `next_attempt_at = now + min(RETRY_INTERVAL_MIN * 2^attempts, 6h)`,
     save. Log `retry.backoff`.
  5. If `attempts >= RETRY_MAX_ATTEMPTS`: drop entry, log
     `retry.exhausted` at WARN. The message stays in Inbox; the
     operator greps stdout for `retry.exhausted` to find undecidable
     mail.

Concurrency:
- The retry tick and the push handler both push work items into the
  same `mpsc` worker, so we never run two `email_set` calls on the
  same id concurrently.

## 11. Logging (`log.rs`)

- One JSON line per event:
  `{"ts":"…","level":"info","event":"email.moved","id":"…","reason":"…","age_days":42}`.
- Events: `boot`, `mailbox.created`, `push.connected`,
  `push.disconnected`, `push.event`, `email.skip`,
  `email.evaluated`, `email.moved`, `email.deferred`,
  `rdap.lookup`, `rdap.error`, `retry.resolved`,
  `retry.backoff`, `retry.dropped`, `retry.exhausted`,
  `state.persisted`, `shutdown`.
- ~40 LOC, just `println!` of a `serde_json::Value`.

## 12. Containerization

Multi-stage Dockerfile producing a small statically-linked image:

```dockerfile
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY data ./data
RUN cargo build --release --locked && strip target/release/shitmail-jmap-filter

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/shitmail-jmap-filter /usr/local/bin/
COPY --from=build /src/data /opt/shitmail-jmap-filter/data
ENV STATE_DIR=/data PSL_PATH=/opt/shitmail-jmap-filter/data/public_suffix_list.dat
VOLUME ["/data"]
USER nobody
CMD ["shitmail-jmap-filter"]
```

`.dockerignore` excludes `target/`, `.git/`, `tests/`.

## 13. fly.io deploy

`fly.toml`:

```toml
app = "shitmail-jmap-filter"
primary_region = "sea"

[build]

[env]
  STATE_DIR = "/data"

[[mounts]]
  source = "data"
  destination = "/data"

[[vm]]
  size = "shared-cpu-1x"
  memory = "256mb"

[deploy]
  strategy = "immediate"

[[services]]
  internal_port = 8080
  protocol = "tcp"
  auto_stop_machines = false
  auto_start_machines = true
  min_machines_running = 1

  [[services.tcp_checks]]
    interval = "30s"
    timeout = "5s"
```

- Tiny `hyper`-free TCP listener on :8080 inside `main.rs` (`accept`
  and immediately close) so fly's TCP healthcheck has something to
  hit. No extra dep.
- `fly secrets set FASTMAIL_API_TOKEN=...`
- `fly volumes create data --size 1 --region sea`
- `fly deploy`

## 14. Test plan

Unit:
- `dkim.rs` against fixture A-R headers.
- `psl.rs` for `co.uk`, `s3.amazonaws.com`, IDN, single-label.
- `policy.rs` truth table.
- `rdap.rs` with a local `tokio` HTTP test server (success,
  404, timeout, missing-TLD).
- `cache.rs` TTL + persistence round-trip.
- `retry.rs` enqueue, backoff growth, cap, drop-on-moved,
  exhaustion, persistence round-trip.

Integration (local, no Fastmail required):
- Stub JMAP server using `tokio` + manual `serde_json` request
  parsing serves a session doc, `Email/get`, `Email/set`,
  `Email/changes`, and an EventSource stream. Drives the full
  pipeline against canned messages with various DKIM/A-R combos.

Manual smoke test against Fastmail:
- Run locally with `STATE_DIR=./tmp` and a real token, send a test
  message from a freshly-registered domain, verify it lands in
  quarantine and the log line shows the right age.

## 15. Milestones

1. **M1 — scaffolding (½ day).** Cargo workspace, `main.rs`, config
   loader, logger, Dockerfile, fly.toml, healthz listener.
2. **M2 — JMAP plumbing (1 day).** Connect, mailbox lookup/create,
   `email_get` + `email_set_mailboxes`, state persistence.
3. **M3 — push loop (½ day).** EventSource consumer, reconnect /
   backoff, change sweep.
4. **M4 — DKIM parsing (½ day).** Module + fixtures + tests.
5. **M5 — domain age (1 day).** PSL vendor, RDAP bootstrap, lookup,
   cache, tests.
6. **M6 — retry queue (½ day).** Persistent queue, poller, backoff,
   tests.
7. **M7 — policy + wiring (½ day).** Glue, logs, integration test
   against stub JMAP server.
8. **M8 — deploy (½ day).** Fly secrets, volume, deploy, watch logs
   for a day on real inbox.

Total: ~5 working days.

## 16. Resolved decisions

- **Language / JMAP library:** Rust + `stalwartlabs/jmap-client`
  (replaces original Node + `jmapio/jmap-js` decision after the
  latter was found to be browser-only).
- **Unsigned / no passing DKIM:** quarantine immediately.
- **RDAP age unknown:** never guess — defer via the persistent retry
  queue (§10); after `RETRY_MAX_ATTEMPTS`, log `retry.exhausted` and
  leave in Inbox.
- **Cold start:** only act on mail arriving after the worker starts.

## 17. Still open

1. Defaults for `RETRY_INTERVAL_MIN=15` and
   `RETRY_MAX_ATTEMPTS=32` (≈8h with backoff cap of 6h) — happy
   with those, or want them tuned?
