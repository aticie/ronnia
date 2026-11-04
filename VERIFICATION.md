# Verification record

Current Railpack and grant-activation review checks: 2026-11-05. Repository-split checks: 2026-11-04. Historical integrated checks: 2026-11-01, Linux x86-64, Rust 1.99.1, Python 3.14 harness, Docker 29.9.2.

The repository-split section records current root-layout and separate-Compose checks. Later sections retain historical integrated-source results and do not independently verify the split. No section claims production Twitch/osu! certification. Provider behavior was exercised against protocol-faithful local simulators; real authenticated provider accounts were not used.

## Native Railpack backend (2026-11-05; verified locally)

Read the primary [Railpack index](https://railpack.com/llms.txt), [Rust provider documentation](https://railpack.com/languages/rust), [configuration/schema](https://railpack.com/config/file), and Railway's [Dockerfile precedence](https://docs.railway.com/config-as-code/reference#specify-the-builder), [Docker cache support](https://docs.railway.com/builds/dockerfiles#cache-mounts), and [healthcheck port contract](https://docs.railway.com/deployments/healthchecks#configure-the-healthcheck-port). A root Dockerfile takes precedence over Railpack; the retained Dockerfile is now `Dockerfile.docker`, with Compose/CI and direct-build instructions updated. No Railway service, variables, secrets, or deployment were read or mutated.

The official Railpack 0.43.0 Linux x86-64 release was downloaded from [its release](https://github.com/railwayapp/railpack/releases/tag/v0.43.0); archive SHA-256 `248a8f60f36854aabed6124113631078a802de3461660855fc4de9c26065c1fb` matched the published asset digest. The first real build reported a missing `BUILDKIT_HOST`; starting a disposable local `moby/buildkit` container supplied that prerequisite. No build/runtime prerequisites remained unavailable.

Executed from this backend checkout:

```sh
railpack prepare --plan-out /tmp/ronnia-railpack-plan.json \
  --info-out /tmp/ronnia-railpack-info.json .
BUILDKIT_HOST=docker-container://ronnia-railpack-buildkit-20261105 \
  railpack build --name ronnia:railpack-20261105 \
  --cache-key ronnia-railpack-proof-20261105 --progress plain .
docker compose --env-file /dev/null config --no-env-resolution --format json
```

The actual plan selected the Rust provider, pinned Rust **1.99.1**, retained native `cargo build --release` and `cp target/release/ronnia bin`, and started **`./bin/ronnia`** in `/app`. Its build step mounts the provider-defined registry/git caches plus the default target cache; the configuration only extends the cache list, never replaces Cargo commands. The separate native dummy-source install step was optimized out by BuildKit, which is why extending the real build step's caches is necessary. Runtime includes CA certificates and excludes `target`; `.dockerignore` continues to allow only the manifest, lockfile, source, migrations, example environment, and Railpack configuration.

The initial native image built successfully in **116.53 s**. After adding the registry/git mounts, the final configured image built successfully in **6.04 s**, with Rust/toolchain and runtime-package layers cached and Cargo reporting `Finished release` in **1.41 s** without recompiling crates. Registry downloads occurred while filling the newly mounted cache. These are local observations with retained builder state, **not** a controlled cold/warm benchmark, a Railway timing guarantee, or a runtime-memory improvement. Docker cache mounts are supported on Railway too. Final image manifest: `sha256:d0d5ef93430dc2246874aaffc2f2591bd700bb9d21197b008a7ec8cae88ee40f`.

The final image was run with its **unmodified default command**, `BOT_ENABLED=false`, `PORT=8080`, an automatically assigned loopback host port, and a fresh temporary directory mounted at `/data`. No provider credentials were supplied:

| Check | Actual result |
|---|---|
| `GET /ready` | HTTP 200, `{"status":"ready"}` |
| `GET /health` | HTTP 200, `{"status":"ok"}` |
| `GET /live/users` | HTTP 200, `[]` |
| Default database | `/data/ronnia.sqlite3`, all 16 application tables created |
| SQLite after shutdown | `PRAGMA integrity_check` → `ok` |
| Graceful container stop | Exit code 0 |
| Runtime link dependencies | `libgcc_s`, `libm`, `libc`, dynamic loader; no dynamic SQLite/OpenSSL dependency |
| CA/runtime files | Nonempty `/etc/ssl/certs/ca-certificates.crt`; `ca-certificates` version `20250419+deb13u1`; no `/app/target` |

Runtime evidence: `/tmp/ronnia-railpack-runtime-odl03a_o/proof.json`. The Railpack image uses the provider runtime's default user; the unchanged Docker image continues to use UID/GID 10001. Compose configuration resolved `Dockerfile.docker`, loopback host port 8080, listener `0.0.0.0:8080`, and the persistent `/data` volume without loading a real `.env`. This is local API-only build/start/readiness evidence, not a live Railway deployment or authenticated provider check. Existing Rust unit/smoke results below are historical to this deployment-only change and were not rerun.

The retained Docker path also built successfully with `docker build -f Dockerfile.docker -t ronnia:docker-railpack-cutover-20261105 .` (89.02 s); its Docker manifest list is `sha256:6adebb804d8c433ede9da0196591c8430e9db71335c43ba371fb2e703644e721`. This exercises the renamed Dockerfile's original locked release build; its contents and Compose runtime/data contract are unchanged.

## PR #57 P2 grant-activation regression (2026-11-05)

The runtime now tracks enabled accounts and active grants separately. An already-enabled imported/legacy account receives a targeted stream-state lookup when its verified grant becomes active, including the delayed migration-grace notification. New enable/import lookups, subscription reconciliation, revision guards, and targeted retries remain on their existing paths; unrelated settings notifications do not repeatedly poll Helix.

Before the fix, `cargo test --locked membership_refreshes_existing_enabled_channel -- --nocapture` failed both new regressions: immediate activation and grace expiry each left the stream-call count at 1 instead of the expected 2. Both use the existing fake Helix and WebSocket/state fixtures, complete real runtime startup with an enabled ungranted account offline, then change the provider to online/osu! without a state event or periodic snapshot. They assert one channel-targeted lookup, immediate stored/public live eligibility, no lookup during grace, and no repeated lookup on unrelated settings notifications.

After the fix:

```text
cargo fmt --all                                   PASS
cargo test --locked membership_refreshes_existing_enabled_channel -- --nocapture
                                                 PASS — 2 passed, 0 failed
cargo fmt --all -- --check                        PASS
cargo clippy --locked --all-targets -- -D warnings PASS
cargo test --locked                              PASS — 105 passed, 0 failed
cargo build --locked --release                   PASS
python3 scripts/smoke.py --binary target/release/ronnia --keep
                                                 PASS
```

The updated smoke ran the newly built release executable, completed startup before granting the already-enabled imported account, and verified an offline-to-online/osu! transition immediately after OAuth consent with exactly one stream lookup. A separate restart with two-second migration grace verifies no premature eligibility/polling, activation via the real delayed OAuth notification, and no repeated stream lookups from unrelated settings writes. Both paths keep `RECONCILE_INTERVAL_SECS=600` and emit no stream-state event to supply the missing state. The full existing smoke also passed enable/delete wakeups, stale-snapshot ordering, delivery, transport recovery, and maintenance coverage. Artifact: `/tmp/ronnia-smoke-tlwulf02`. The complete sequential formatting/regression/clippy/test/build/smoke command finished in 169.50 seconds. No live providers, credentials, deployments, or PR #56 files were touched.

## Repository split (2026-11-04; verified)

The Rust bot/API, migrations, and Python verification harness now live at the root of `aticie/ronnia`; the legacy Python application, configuration, tests, assets, and deployment files are removed. Dashboard source and frontend build checks live separately in [aticie/ronnia-web](https://github.com/aticie/ronnia-web). The legacy bridge is independently maintained in [PR #56](https://github.com/aticie/ronnia/pull/56), to be run from a separate checkout; its Rust internal API remains present.

The following checks passed from the Rust-only repository root on 2026-11-04:

```text
cargo fmt --all -- --check                         PASS
cargo clippy --locked --all-targets -- -D warnings PASS
cargo test --locked                              PASS — 103 passed, 0 failed
cargo build --locked --release                   PASS
python3 scripts/smoke.py --binary target/release/ronnia --keep
                                                 PASS
```

The actual root-built release executable passed all 16 original HTTP routes plus `/health` and `/ready`, import/maintenance, both signup orders, encrypted device authorization, EventSub delivery/recovery, hybrid state, ambiguous-delivery safety, and isolated same-process DNS recovery. Artifact: `/tmp/ronnia-smoke-g3q1gr6z`. Rust tests also included zero-test binary and doc-test suites; none failed. The complete sequential check command finished in 220.71 seconds. These checks used this checkout's `target/`, not a borrowed executable.

The root Dockerfile also built successfully as `ronnia:split-web-runtime-20261104` (manifest `sha256:96583bea15970d4bc73ba1e1aa91ecf1e843098655e820deeb6ea1941b8ec8b4`). The frontend verification owner exercised two real, separate Compose projects on the disposable `ronnia-split-proof-20261104` network with synthetic configuration, a fresh backend volume, API-only mode, and loopback ports 28857/23857. Chromium rendered the dashboard and provider links; `/api/ready` returned HTTP 200 with `{"status":"ready"}`, `/api/user/me` returned HTTP 200 `null`, and `/api/internal/` returned Nginx HTTP 404. Both projects, their network, and the disposable database volume were removed afterward. No live credentials, deployments, or provider OAuth calls were used. Browser external fonts were intentionally blocked by the localhost-only test policy.

The dashboard changes and frontend-specific checks are in [ronnia-web PR #24](https://github.com/aticie/ronnia-web/pull/24). This split proof is API-only; historical full-bot and provider-simulator coverage is recorded separately below.

Backend `compose.yaml` now contains only the backend, loopback-published on `BIND_PORT` (default 8080). Start backend Compose first to create `${RONNIA_NETWORK:-ronnia}`; the separate frontend Compose joins that external network and defaults its upstream to `http://ronnia:${BIND_PORT:-8080}`. Both projects need matching `BIND_PORT` and `RONNIA_NETWORK`. The frontend repository owns its browser/build checks; the current cross-project proof above supersedes earlier integrated two-service Compose evidence for this layout.

For memory comparison, defaults now use sibling `../ronnia-legacy` and `../ronnia-backend` source checkouts, not the current Rust tree. Prepare the legacy bot interpreter at `../ronnia-legacy/.venv/bin/python` and backend interpreter at `/tmp/ronnia-backend-baseline/bin/python`, or override the corresponding flags. `scripts/memory.py` starts isolated processes and MongoDB only when explicitly run; no benchmark was rerun for the split.


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
docker build -f Dockerfile.docker -t ronnia-rust:verified .
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

## Historical integrated dashboard verification

- Before the split, imported `aticie/ronnia-web` at commit `ec0ff76ef8e3b06c873f5213c71a0901ef6514f9` into the integrated source tree. That frontend source now belongs only to `aticie/ronnia-web`.
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
