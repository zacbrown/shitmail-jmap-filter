# shitmail-jmap-filter

A small Rust worker that watches a Fastmail inbox over JMAP push,
looks up the registration age of every sender's *DKIM-verified*
domain, and quarantines mail from domains less than a year old.

See [`PLAN.md`](./PLAN.md) for the design and rationale.

## How it works

1. Connects to Fastmail JMAP using a bearer API token.
2. Subscribes to JMAP EventSource for `Email` / `Mailbox` state
   changes.
3. For each new message in the Inbox, parses the *server-added*
   `Authentication-Results` header and extracts the `header.d=`
   from a passing DKIM verdict (preferring DMARC-aligned).
4. Resolves the registrable domain (eTLD+1) via a vendored Public
   Suffix List.
5. Looks up the domain's registration date via RDAP, with a
   persistent positive/negative cache on disk.
6. If the domain is less than `MAX_DOMAIN_AGE_DAYS` (default 365)
   old, moves the message into a `quarantine` mailbox (created on
   first run if missing).
7. Messages without a passing DKIM signature are quarantined
   immediately.
8. If RDAP can't answer (timeout, missing TLD, malformed response),
   the message is left in the Inbox and enqueued in a persistent
   retry queue with exponential backoff; the decision is made later
   once RDAP succeeds, or logged as `retry.exhausted` after the cap.

All activity is logged to stdout as JSON lines.

## Build

```sh
cargo build --release
```

Requires Rust 1.74+.

## Configuration

| Env var                   | Default                                | Notes |
| ------------------------- | -------------------------------------- | ----- |
| `FASTMAIL_API_TOKEN`      | *(required)*                           | Bearer token. |
| `JMAP_SESSION_URL`        | `https://api.fastmail.com/jmap/session` | |
| `QUARANTINE_MAILBOX_NAME` | `quarantine`                           | Created if missing. |
| `MAX_DOMAIN_AGE_DAYS`     | `365`                                  | Quarantine cutoff. |
| `RETRY_INTERVAL_MIN`      | `15`                                   | Retry poller tick. |
| `RETRY_MAX_ATTEMPTS`      | `32`                                   | Before `retry.exhausted`. |
| `STATE_DIR`               | `/data`                                | Persistent state dir. |
| `PSL_PATH`                | `data/public_suffix_list.dat`          | Vendored PSL. |
| `TRUSTED_AUTHSERV_IDS`    | `fastmail.com,messagingengine.com`     | Comma-separated. |

## Run locally

```sh
export FASTMAIL_API_TOKEN=...
export STATE_DIR=./tmp
mkdir -p ./tmp
cargo run --release
```

## Deploy on fly.io

```sh
fly launch --no-deploy
fly volumes create data --size 1 --region sea
fly secrets set FASTMAIL_API_TOKEN=...
fly deploy
```

## Dependencies

- [`jmap-client`](https://github.com/stalwartlabs/jmap-client) — JMAP.
- `tokio`, `reqwest`, `serde`, `serde_json`, `futures-util`,
  `chrono`, `anyhow` — async runtime, HTTP for RDAP, JSON, dates,
  errors.

No DKIM, WHOIS, PSL, or logging crate — those are hand-rolled in
~250 LOC total to keep the surface small.
