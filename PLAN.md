# shitmail-jmap-filter — Implementation Plan

A Node.js worker that watches a Fastmail inbox over JMAP push, looks up the
age of each sender's DKIM-verified domain, and moves mail from domains less
than one year old into a `quarantine` folder. Packaged as a container and
deployed to fly.io.

## 1. Constraints recap

- Runs as a container.
- Runs on fly.io as a long-lived worker (single machine, always-on).
- Uses [`jmap-js`](https://github.com/jmapio/jmap-js) for JMAP interactions.
- Minimal dependencies beyond `jmap-js`.
- Inputs: a Fastmail JMAP API token.
- Trigger: JMAP push notifications for `Email` state changes in Inbox.
- Decision: domain age of the *true DKIM-verified* signing domain (`d=`
  from a passing DKIM signature in `Authentication-Results`), not the
  `From:` header domain.
- Action: if age < 365 days, move the message to `quarantine` (create the
  mailbox if missing).
- Logs all activity to stdout (structured JSON lines).

## 2. Risks / decisions to confirm before coding

These are the points where I'd like a quick yes/no before writing code,
because they change the dependency footprint or the failure semantics.

1. **`jmap-js` fit.** `jmap-js` is built on Overture and targets the
   browser. It works under Node with a `fetch`/`XMLHttpRequest` shim, but
   it is heavier than a hand-rolled JMAP client and pulls Overture as a
   transitive dependency. The requirement is explicit, so the plan
   assumes we use it — but flagging in case "use jmap-js" was shorthand
   for "any JMAP client" and a thin hand-written client would be
   preferable.
2. **Push transport.** JMAP defines two push mechanisms: EventSource
   (SSE, RFC 8620 §7.3) and WebPush (§7.2). EventSource is the only one
   that works without a public HTTPS endpoint registered with the
   server, so the plan uses EventSource. (Fastmail supports both.)
3. **Domain-age source.** Plan uses RDAP over HTTPS (JSON, standardized,
   no whois-text parsing, no extra dependency beyond `fetch`).
   Bootstrap from the IANA RDAP bootstrap file
   (`https://data.iana.org/rdap/dns.json`) to pick the right registry
   server per TLD. Whois (port 43) is the fallback but requires a parser
   library, which conflicts with "minimal dependencies".
4. **Registrable domain extraction.** RDAP queries the registrable
   domain (eTLD+1), not arbitrary subdomains. To compute eTLD+1
   correctly for things like `foo.co.uk` we need the Public Suffix List.
   Options: (a) vendor a small PSL snapshot into the repo and refresh
   periodically — zero runtime deps; (b) add `tldts` or `psl` as a
   dependency. Plan uses option (a) to keep the deps list to
   `jmap-js` + `eventsource` only.
5. **Fail-open vs fail-closed.** When DKIM is absent, or RDAP times out,
   or the TLD has no RDAP server, what do we do? Plan defaults to
   **fail-open** (leave in Inbox, log a warning) so a flaky RDAP server
   can't bury legitimate mail. Configurable via env var.
6. **What counts as "in Inbox"?** Plan scopes to messages whose
   `mailboxIds` contain the Inbox mailbox ID *and* that are unread on
   first arrival (so we don't re-process mail the user has already
   read/moved). State cursor below makes this idempotent.

## 3. Dependencies

Runtime:
- `jmap-js` (per requirement).
- `eventsource` — small, well-maintained Node SSE client. Native
  EventSource is not in Node yet.
- Node 20+ for native `fetch`, `AbortController`, `node:test`.

Dev:
- `node:test` + `node --test` for tests (no Jest/Mocha).
- TypeScript is **not** in the plan — keeping the toolchain to plain
  JS to honor "minimal dependencies". Can revisit.

That's it: two runtime deps.

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
|  /data (fly volume): jmap-state.json, rdap-cache.json
+----------------------+
```

Single process. One in-flight EventSource. A bounded async work queue
processes new emails one at a time (concurrency=1 keeps logs readable
and avoids burst RDAP load; configurable).

## 5. Module layout

```
src/
  index.js              entry point, wiring, signal handling
  config.js             env parsing, defaults, validation
  log.js                JSON-line logger to stdout
  jmap/
    client.js           jmap-js bootstrap + Node shim (fetch + EventSource)
    session.js          /.well-known/jmap discovery, account id
    mailboxes.js        find/create Inbox + quarantine mailbox
    push.js             EventSource subscribe, reconnect, debounce
    emails.js           Email/changes + Email/get + Email/set move
  dkim.js               parse Authentication-Results, pick verified d=
  domain/
    psl.js              eTLD+1 from vendored Public Suffix List
    rdap.js             RDAP bootstrap + lookup + age computation
    cache.js            on-disk JSON cache w/ TTL (negative + positive)
  state.js              persist last JMAP state string to /data
  policy.js             "younger than N days?" decision
data/
  public_suffix_list.dat   vendored PSL snapshot
test/
  dkim.test.js
  psl.test.js
  policy.test.js
  rdap.test.js          (mocked fetch)
Dockerfile
fly.toml
package.json
README.md
```

## 6. Runtime flow

1. **Boot.**
   - Read `FASTMAIL_API_TOKEN`, optional `QUARANTINE_MAILBOX_NAME`
     (default `quarantine`), `MAX_DOMAIN_AGE_DAYS` (default `365`),
     `FAIL_OPEN` (default `true`), `STATE_DIR` (default `/data`).
   - Discover JMAP session: `GET https://api.fastmail.com/jmap/session`
     with `Authorization: Bearer <token>`. Extract `apiUrl`,
     `eventSourceUrl`, primary `urn:ietf:params:jmap:mail` account id.
   - Look up Inbox role=`inbox` mailbox id; look up quarantine mailbox
     by name, create via `Mailbox/set` if missing.
   - Load last persisted JMAP state string (if any) from
     `/data/jmap-state.json`.

2. **Catch-up sweep.**
   - If we have a persisted state, call `Email/changes` with
     `sinceState`; process each `created` id that is in Inbox.
   - If no persisted state, call `Email/query` for
     `{ inMailbox: <inbox>, after: <bootTime - 5 min> }` to seed
     without flooding ourselves on cold start.
   - Persist new state.

3. **Subscribe to push.**
   - Open EventSource to `eventSourceUrl` with `types=Email` and
     `closeafter=no`, `ping=300`.
   - On each `state` event, run the same `Email/changes`-driven sweep
     as step 2, then persist new state.
   - On disconnect: exponential backoff (1s → 30s cap) with jitter,
     reconnect, resume from persisted state.

4. **Per-email processing** (`processEmail(id)`):
   1. `Email/get` with properties:
      `["id","mailboxIds","from","header:Authentication-Results:asRaw","receivedAt"]`.
   2. If message no longer in Inbox (user already moved it), skip + log.
   3. Parse `Authentication-Results` headers (there can be multiple).
      Pick DKIM signatures with `dkim=pass` and an aligned or
      explicit `header.d=` value. If multiple pass, prefer one
      aligned with `From:` domain; otherwise take the first.
   4. If no passing DKIM → policy: fail-open log + leave; or quarantine
      if `QUARANTINE_UNSIGNED=true`.
   5. Compute eTLD+1 of `header.d` via PSL.
   6. Look up domain age via cache → RDAP.
   7. If age < `MAX_DOMAIN_AGE_DAYS` → `Email/set` with
      `update: { <id>: { mailboxIds: { <quarantineId>: true, <inboxId>: null } } }`.
   8. Log a structured line with: message id, from, dkim domain,
      registrable domain, age days, action taken, latency.

5. **Shutdown.**
   - SIGTERM/SIGINT: close EventSource, flush state + cache, exit 0.

## 7. DKIM-verified-domain extraction (`dkim.js`)

- JMAP exposes raw headers via the
  `header:Authentication-Results:asRaw` property; this returns the
  *server-added* Authentication-Results (Fastmail's, which is the
  trusted one). Sender-supplied A-R headers must be ignored — the
  trusted A-R is the one whose `authserv-id` matches the server
  (`fastmail.com` / `mx*.messagingengine.com`). We hard-allow that set
  and ignore the rest.
- Parse per RFC 8601 §2.2. For each method `dkim`, capture `result` and
  the `header.d` / `header.i` properties. Keep entries where
  `result == "pass"`.
- Among passing entries, prefer one whose `header.d` matches (or is a
  parent of) the `From:` domain (DMARC-aligned). If none aligned, pick
  the first passing entry.
- If no passing entry, return `null`.

Unit-tested against a fixture file of real-looking A-R headers
(aligned pass, unaligned pass, mixed pass+fail, no DKIM, multiple
servers' A-R chained).

## 8. Domain-age lookup (`domain/rdap.js`)

- Cache layer first (`domain/cache.js`):
  - In-memory `Map<string, { registered: ISO, fetchedAt, ttl }>`.
  - Periodically flushed to `/data/rdap-cache.json` (debounced 5s
    after writes).
  - Positive TTL: 30 days (domain creation dates don't change).
  - Negative TTL: 1 hour (so a transient RDAP failure isn't sticky).
- RDAP query:
  - On first use, fetch `https://data.iana.org/rdap/dns.json` and
    build TLD → RDAP base URL map. Cached on disk; refreshed daily.
  - For `example.com`: `GET <base>domain/example.com` with
    `Accept: application/rdap+json`, 10s timeout.
  - Pull `events[]` where `eventAction == "registration"` →
    `eventDate`. Compute age in days vs `Date.now()`.
- TLDs with no RDAP base (rare, mostly obscure ccTLDs) → return
  `unknown`. Policy treats `unknown` per `FAIL_OPEN`.

## 9. Policy (`policy.js`)

Single pure function:

```js
shouldQuarantine({ ageDays, hasVerifiedDkim }, { maxAgeDays, failOpen, quarantineUnsigned }) -> boolean
```

- No verified DKIM → `quarantineUnsigned`.
- Age unknown → `!failOpen`.
- Age < maxAgeDays → `true`.
- Else `false`.

Unit-tested exhaustively. All other code stays free of policy
branching.

## 10. Logging (`log.js`)

- JSON line per event: `{ ts, level, event, ...fields }`.
- Events: `boot`, `mailbox.created`, `push.connected`,
  `push.disconnected`, `push.event`, `email.skip`,
  `email.evaluated`, `email.moved`, `rdap.lookup`, `rdap.error`,
  `state.persisted`, `shutdown`.
- No log library; ~30 lines of code.

## 11. Containerization

`Dockerfile` (multi-stage, ~25 lines):

```dockerfile
FROM node:20-alpine AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --omit=dev
COPY src ./src
COPY data ./data

FROM node:20-alpine
WORKDIR /app
COPY --from=build /app /app
USER node
ENV NODE_ENV=production STATE_DIR=/data
VOLUME ["/data"]
CMD ["node", "src/index.js"]
```

`.dockerignore` excludes `test/`, `.git/`, `node_modules/`.

## 12. fly.io deploy

`fly.toml`:

```toml
app = "shitmail-jmap-filter"
primary_region = "sea"

[build]

[[mounts]]
  source = "data"
  destination = "/data"

[[vm]]
  size = "shared-cpu-1x"
  memory = "256mb"

[deploy]
  strategy = "immediate"

[processes]
  worker = "node src/index.js"

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

- Tiny HTTP server on :8080 in `index.js` (`/healthz` → 200) so fly's
  healthcheck has something to hit. Twenty lines, no dep.
- `fly secrets set FASTMAIL_API_TOKEN=...` for the token.
- `fly volumes create data --size 1 --region sea` for the state volume.
- `fly deploy`.

## 13. Test plan

Unit:
- `dkim.js` against fixture A-R headers.
- `psl.js` for `co.uk`, `s3.amazonaws.com`, IDN, single-label.
- `policy.js` truth table.
- `rdap.js` with `fetch` mocked (success, 404, timeout,
  bootstrap-missing TLD).
- `cache.js` TTL + persistence round-trip.

Integration (local, no Fastmail required):
- Stub JMAP server (small Express-like handler using Node's `http`
  module — no Express dep) that serves a session doc, `Email/get`,
  `Email/set`, and an EventSource stream. Drives the full pipeline
  against canned messages with various DKIM/A-R combos.

Manual smoke test against Fastmail:
- Run locally with `STATE_DIR=./tmp` and a real token, send a test
  message from a freshly-registered domain, verify it lands in
  quarantine and the log line shows the right age.

## 14. Milestones

1. **M1 — scaffolding (½ day).** Repo layout, `package.json`,
   logger, config loader, Dockerfile, fly.toml, healthz.
2. **M2 — JMAP plumbing (1 day).** Session discovery, mailbox
   lookup/create, `Email/get` + `Email/set` move, state persistence.
   Verified end-to-end against Fastmail by manually moving a chosen
   message id from CLI.
3. **M3 — push loop (½ day).** EventSource subscribe, change sweep,
   reconnect/backoff.
4. **M4 — DKIM parsing (½ day).** Module + fixtures + tests.
5. **M5 — domain age (1 day).** PSL vendor, RDAP bootstrap, lookup,
   cache, tests.
6. **M6 — policy + wiring (½ day).** Glue, logs, integration test
   against stub JMAP server.
7. **M7 — deploy (½ day).** Fly secrets, volume, deploy, watch logs
   for a day on real inbox, tune `FAIL_OPEN` / `QUARANTINE_UNSIGNED`
   defaults.

Total: ~4–5 working days.

## 15. Open questions for the user

1. OK to keep `eventsource` as a second runtime dep, or do you want me
   to hand-roll an SSE reader on `fetch`'s ReadableStream?
2. What's your preferred behavior for **unsigned / DKIM-failing
   senders** — leave in Inbox (current default) or quarantine?
3. RDAP can't answer for every TLD. For an `unknown` age, fail-open
   (leave in Inbox) or fail-closed (quarantine)?
4. Should the worker also act on **existing** mail in the Inbox at
   first boot, or only on mail arriving *after* deploy?
