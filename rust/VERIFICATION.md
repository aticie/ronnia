# Verification record

Date: 2026-11-01. Environment: Linux x86-64, Rust 1.99.1, Python 3.14 harness, Docker 29.9.2.

These results describe the current Rust tree. They do not claim production Twitch/osu! certification. Provider behavior was exercised against protocol-faithful local simulators; real TLS/provider accounts were not used.

## Static and unit verification

```text
cargo fmt --all -- --check                         PASS
cargo clippy --locked --all-targets -- -D warnings PASS
cargo test --locked                                 PASS — 103 tests
cargo build --release --locked                      PASS
```

The 103 tests include: request parsing and mods; policy and formatting; EventSub frame/timeout/handoff and loopback URL validation; conduit reuse and owned-only subscription reconciliation; hybrid stream-state ordering and stale-snapshot races; targeted category lookup; account grants and consent; encrypted credential rotation/tamper/revocation; OAuth both-order signup and CSRF/session checks; all HTTP route contracts; OpenAPI generation; SQLite v1→v4 upgrade; mongodump JSON/NDJSON/BSON import validation and replay protection; cache freshness and deterministic set mapping; delivery deduplication and ambiguous outcomes; and shutdown/cancellation behavior.

## End-to-end executable smoke

Command:

```sh
python3 scripts/smoke.py --binary target/release/ronnia --keep
```

Latest result: **PASS** (97.43 s), artifact `/tmp/ronnia-smoke-6gu81j80`. The harness had also passed twice before the final loopback-validation adjustment.

Exercised through the actual binary, real HTTP/WebSocket servers, and SQLite:

- Mongo export import with all four collections, normalization, raw provenance, and no fabricated chat consent.
- All 16 original routes plus `/health` and `/ready`; authenticated and anonymous shapes; debug `/openapi.json`, `/docs`, `/redoc` gated by `DEBUG_MODE`.
- Both signup orders with verified Twitch `channel:bot` consent; imported users inactive until reauthorization; same/different identity relinking rules; session rotation/revocation and CSRF/origin checks.
- Real device-code bot authorization: pending → browser activation → success; encrypted-only database evidence; invalid scope/denial/different-bot refusal; token refresh after 401.
- EventSub conduit creation/reuse, WebSocket welcome/shard attach, owned subscription reconciliation, `channel.chat.message` → osu! PM → statistics → Twitch acknowledgement.
- Hybrid state: online with targeted category lookup, immediate offline/category deactivation, out-of-order event rejection, slow snapshot versus newer offline race, grant/enable/delete wakeups, short reconciliation repair.
- Same-process DNS recovery: two actual `EAI_NONAME` failures followed by resolution, refusal, reconnect, and delivery. Injected with a process-local `LD_PRELOAD` resolver shim; no `/etc/hosts`, host DNS, or global configuration changes.
- Connection loss and reattachment without duplicating owned subscriptions; server `session_reconnect` make-before-break with old-connection delivery and close ordering; keepalive-timeout recovery where Ping/Pong frames do not extend the deadline.
- Authorization revocation without deleting the account or entering a resubscribe hot loop; `is_sent=false`, 429 `Retry-After`, and missing-success acknowledgement handling.
- Ambiguous osu! POST retained and not replayed, including after restart; conduit/subscription reuse on restart.
- Dry-run behavior; maintenance `import --check` commit safety, integrity check, backup/restore, delivery inspection, and backfill through the real CLI.

A temporary ignored integration probe using the same `tokio-tungstenite` dependency connected to the real `wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30`, completed the TLS/WebSocket upgrade, and received a `session_welcome`. It passed in 0.33 s; the throwaway probe was then removed.

Not covered: authenticated live Twitch/osu! provider calls, real EventSub outage retention behavior, provider account restrictions, and production load.

## Memory comparison

The harness ran the unchanged original Python bot and backend from the two source trees, with an isolated real `mongo:7` container, against the Rust binary with in-process SQLite on the same host. Original code was not edited; only transport destination URLs were redirected to local simulators. The same synthetic workload, user settings, exports, and chat schedule were used (30 requests at 2.25 s intervals over 75 s).

```sh
python3 scripts/memory.py --binary target/release/ronnia \
  --requests 30 --seconds 75 --interval 2.25 \
  --output verification/memory.json --keep
```

Status: **complete**. Artifact: `/tmp/ronnia-memory-liv6pj0v`; samples in `verification/memory.json`.

| Measurement | Python bot + backend + MongoDB | Rust + SQLite |
|---|---:|---:|
| Median service RSS | 323,252,224 B (308.28 MiB) | 13,754,368 B (13.12 MiB) |
| Minimum RSS | 314,404,864 B | 13,619,200 B |
| Maximum RSS | 327,655,424 B | 13,795,328 B |
| Successful osu! PMs | 30 | 30 |
| Twitch success echoes | 30 | 30 |
| Imported + workload request statistics | 34 | 34 |

Rust service RSS is **4.25%** of the Python service RSS in this workload (≈23.5× lower). The service figure includes descendants and the dedicated MongoDB host, not the common provider simulators. Sum of RSS can double-count shared pages; PSS samples are included when `/proc` permits. SQLite resides in the Rust process. This is a small local workload, not a production peak or capacity benchmark. The modern EventSub transport replaces IRC, so transport overhead is an intentional implementation difference; the chat count, delivery, echo, and persistence work are matched.

## Container

```sh
docker build -t ronnia-rust:verified .
docker run --rm -e BOT_ENABLED=false -p 127.0.0.1:38081:8080 ronnia-rust:verified serve --api-only
```

The image builds and runs as UID/GID 10001; `/ready` returned `{"status":"ready"}` and `/live/users` returned `[]`; graceful shutdown exited with code 0. The final image was rebuilt after the EventSub cutover. Containerized provider/TLS integration was not exercised; TLS was separately verified with the release's WebSocket stack outside the container.

The debug documentation surface was opened in a real headless Chromium against `serve --api-only` with `DEBUG_MODE=true`: Swagger UI rendered with title `Ronnia API - Swagger UI`, 18 operations across 14 paths (16 original routes plus `/health` and `/ready`), and a valid OpenAPI 3.1 document. The same routes returned 404 with debug mode disabled in the smoke test.

## Split listener configuration

- `BIND_IP` and `BIND_PORT` were exercised through the rebuilt executable on IPv4 (`127.0.0.1`) and IPv6 (`::1`) using nondefault ports; both returned `{"status":"ready"}` and shut down cleanly.
- A combined `IP:port` in `BIND_IP`, an out-of-range port (`65536`), and a nonnumeric port were rejected with diagnostics naming the relevant setting.
- `docker compose config --format json`, projected to binding fields only, read the local `.env` port 4343 and produced host `127.0.0.1:4343`, container target 4343, listener `BIND_IP=0.0.0.0`, and `BIND_PORT=4343`.
- Formatting, Clippy with warnings denied, all 89 Rust tests, and the release build passed after the split.
- The updated executable smoke harness passed all route, import/maintenance, authorization, EventSub recovery, and hybrid-state scenarios using separate IP and port variables (96.90 seconds). Compose's missing-port fallback was also checked: listener, target, and published port all default to 8080.

## Full-stack Compose startup

- Built and started the real Compose service in an isolated project with synthetic credentials, an empty named volume, and an unused host port; no user credentials or existing database were used.
- Confirmed the explicit `serve` command runs both the API (`/ready` returned ready) and bot worker (logs report waiting for first Twitch bot authorization).
- Tested with conflicting synthetic `.env` values (`BOT_ENABLED=false` and an unmounted database path): Compose still enables the bot and stores SQLite at `/data/ronnia.sqlite3`.
- SQLite integrity/foreign-key checks passed. Created a consistent backup in `/data`, force-recreated the container, and confirmed the backup remained present and passed integrity checks in the persistent volume.
- Removed only the isolated test project's containers, network, and test volume afterward. No authenticated external provider calls were made.

## Local dashboard integration

- Imported `aticie/ronnia-web` at commit `ec0ff76ef8e3b06c873f5213c71a0901ef6514f9` into the single repository, without a nested Git repository.
- `npm ci --ignore-scripts`, `npm run build` (Vue/TypeScript check plus Vite), and `npm audit` passed; audit reports zero vulnerabilities after compatible transitive build dependency updates.
- Built the Node-24/static-Nginx frontend image and started the real two-service Compose stack. A real headless Chromium rendered the production dashboard at `http://localhost:3000/login`; `/api/ready` passed through Nginx, both relative login links were present, no development router was exposed, and no browser errors were reported.
- Browser registration tests used the actual Vue app and Rust API with an isolated SQLite database and local HTTP OAuth token/profile fixtures. Only the third-party authorization-page redirects were simulated; no real provider account consent was submitted.
- Both provider orders passed: the first callback displayed only the missing provider, the second reached settings with the correct linked IDs and verified Twitch grant. A settings-page reload retained the session, and cookies were HttpOnly.
- Browser checks passed for logout, exclusion add/remove with server normalization, Twitch re-consent without changing identity, expired-session handling, login after expiration, and account deletion.
- A delayed first settings POST held longer than the debounce interval while a second setting changed. There was at most one active POST; the latest complete seven-field snapshot was flushed afterward, both edits persisted, and `[0,-1]` remained an unlimited star range.
- Local deployment origins and HTTP cookie settings were corrected without changing provider credentials, the encryption key, or existing localhost:4343 callbacks. The running stack uses its existing named SQLite volume.
- The memory comparison above measures the backend stacks only; it does not include the newly integrated static Nginx frontend and is not a new total-memory measurement for both services.

## Offline chat subscriptions and test mode

- The executable smoke harness verified that chat and state subscriptions remain present for offline and non-osu channels, while `/live/users` and ordinary request acceptance remain inactive.
- Offline, category-away, and stale-snapshot requests produced no osu! PM, statistics, or Twitch feedback.
- With user `test` mode enabled, an offline request produced a real osu! PM, history, and success feedback while public `isLive` remained false and `/live/users` stayed empty.
- Clearing `test` restored normal offline rejection. `enable=false` removed all subscriptions even with test mode on, confirming enable remains the master switch.
- A newly granted offline channel received chat immediately, and short reconciliation repaired missed offline state without dropping chat.
- Formatting, Clippy with warnings denied, 89 tests, release build, and the full smoke harness passed.
- After redeployment of the real local Compose stack, the bot created a `channel.chat.message` subscription for the authorized channel while `/api/live/users` remained empty, confirming chat is subscribed independently of live state.

## Coexistence migration bridge

- Rust: 103 tests, Clippy with warnings denied, release build, and the full smoke harness passed after the coexistence changes.
- Legacy Python bot: 61 unit tests passed, including 38 new bridge/API tests, using an in-process aiohttp fixture; `py_compile` passed. The pre-existing beatmap parser test import was corrected so the suite runs under package-qualified imports.
- Real wire integration: a fresh Rust service with the internal token enabled and an imported fixture was exercised through the patched Python `LegacyAPI`. Pagination, ID/login lookup, claim, duplicate-claim rejection, delivery reporting, idempotent statistic replay, public migration status, and wrong-token rejection all passed.
- The persisted delivery was `sent` with `source=legacy`; exactly one statistics row with `source_id=twitch:<message>` and the original beatmap ID existed. Inserting a grant immediately removed the channel from the legacy list and caused a new claim to be rejected as migrated.
- The patched `LegacyAPI.report_live` endpoint was exercised against a real isolated service: it updated the legacy login, exposed the channel through the public live-users union, and an empty live report cleared it. The legacy by-login lookup then resolved the updated name.
- This verifies the anti-duplication contract at the API boundary; real Twitch IRC and osu! delivery were not used.
- A consistent backup of the live Compose SQLite database was created before applying the coexistence migrations; post-upgrade integrity and foreign-key checks passed.
- The deployed local stack returned public migration status `active:true` with cutoff `2026-12-04T00:00:00Z`; an authenticated internal request returned legacy channels, no-auth returned 404, and a wrong bearer token returned 401. The dashboard proxy blocked `/api/internal/*` with 404.
- Chromium rendered the real dashboard banner with `role=status`, the localized cutoff, and the settings action. A scroll-position check exposed the banner initially above the viewport; it was changed to sticky and reverified at maximum scroll (`y=0`, visible) before the final deployment.

## Limits

- No real Twitch or osu! account, chat message, OAuth consent, or EventSub production stream was used. The production EventSub endpoint was reached only for an unauthenticated TLS handshake/welcome.
- The patched old Python bot was tested against local fake Twitch/osu/new-API fixtures, not a live IRC connection or real users; its live Twitch/osu announcement delivery remains to be observed during rollout.
- No production-scale channel count, message throughput, rate-limit behavior, or long-duration memory measurement.
- Binary/agent results are only claimed where command output above confirms them.
