# Verification record

Current single-replica scale, Railpack, and grant-activation checks: 2026-11-05. Repository-split checks: 2026-11-04. Historical integrated checks: 2026-11-01. Linux x86-64, Rust 1.99.1, Python 3.14.8, Docker 29.9.2.

The repository-split section records current root-layout and separate-Compose checks. Later sections retain historical integrated-source results and do not independently verify the split. No section claims production Twitch/osu! certification. Provider behavior was exercised against protocol-faithful local simulators; real authenticated provider accounts were not used.

## Chat-ban visibility and recovery (2026-11-14)

Schema version 6 persists `chat_banned_at` and `chat_retry_at` on each Twitch channel grant. A `chat_user_banned` revocation or 403 chat-subscription response pauses only that channel's chat creation for one hour; consent, live eligibility, and the three state subscriptions remain intact. A later successful creation clears the persisted ban. `/user/me` exposes the state as `twitchBotBanned` independently of `twitchBotAuthorized`.

`cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked` passed in the `rust:1-bookworm` container: **157 passed, 0 failed**. New regressions cover the schema upgrade, ban/clear transitions without grant revocation, chat-only desired-set filtering, 403 → no repeated creation POSTs → successful unban probe, provider error-body diagnostics, and the `/user/me` projection.

The separate dashboard passed `npm run build` (`vue-tsc --noEmit` plus Vite). A real browser rendered `/settings` against a mocked authorized-and-banned `/api/user/me`; `[data-testid="twitch-bot-banned"]` displayed the amber warning and `/unban ronniabot` instruction. No production deployment, live unban, or provider-state mutation was performed.

After merging the updated base, `cargo build --locked --release` and `python3 scripts/smoke.py --binary /target/release/ronnia` also passed: all 16 HTTP routes, both signup orders, encrypted device authorization, EventSub lifecycle, reconnect/revocation safety, DNS recovery, dry-run, and maintenance CLI checks. The smoke contract includes `twitchBotBanned` in `/user/me` and OpenAPI.

## Review fixes (2026-11-14)

Verified the fixes following review of PR #57, based on `migration/rust-port` at `50c2e4a`. Rust checks ran with Rust 1.100.0 in an isolated `rust:1-bookworm`-derived toolchain after the host Rust installation became unavailable. No production provider credentials or deployment were used.

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | Pass |
| `cargo clippy --locked --all-targets -- -D warnings` | Pass |
| `cargo test --locked` | 152 passed |
| `cargo build --locked --release` | Pass |
| `scripts/smoke.py --binary <release-binary>` | Pass, including exact handoff-reader delivery, old-session rejection, and durable broadcaster-consent revocation |
| `scripts/scale.py --binary <release-binary> --channels 8 --requests 6` | Pass, local lifecycle/load compatibility |
| Production Docker image | Pass: UID 10001, nonempty CA bundle, packaged license, readiness with a synthetic `SENTRY_DSN`, clean shutdown |
| Isolated CLI scenarios | Pass: key generation despite invalid service config; empty environment/CLI database paths rejected; API-only OAuth credentials required; normalized origin/callback/client ID and Secure cookie |

Regression coverage includes the Unicode mod panic, real duplicate-delivery and interrupted-send paths, legacy pending reports across Rust startup, cancellation/backpressure during refresh persistence, stale grant-revocation CAS, disabled-subscription cleanup at full capacity, unspecified pagination sizes, authoritative broadcaster consent, and revocation overflow at 2,500 grants. OAuth mocks validate provider-specific credentials and callback URIs; the grace timer is exercised with controlled Tokio time rather than a one-second wall-clock race.

The smoke and scale harnesses now commit and close every SQLite connection. Leaving them open (`with sqlite3.connect(...)` commits without closing) kept the harness process attached to a pre-restart WAL index, so its later reads intermittently missed rows the service had committed; repeated CI failures at startup snapshot checks traced to this, not to the service. The grant-metadata check now waits for the post-refresh validation stamp instead of sampling the transient `validated_at=0` state.

Schema v5 upgrades preserve existing grants and deliveries. Real Twitch/osu! interoperability, production throughput, and hard process-kill recovery during a provider token exchange remain outside this local proof.

## Single-replica bounded scale (2026-11-05)

Implemented in a fresh checkout of `aticie/ronnia` PR #57, `migration/rust-port`, starting at remote `8f6663995c92ce00f644a90e17595307be148b7f`. The legacy checkout, bridge PR #56, and frontend PR #24 were not changed. All provider traffic below used isolated local HTTP/RFC6455 fixtures, synthetic authorization, and temporary SQLite; no production credentials or Railway deployment were used.

Executed against the root-built release executable:

```sh
cargo fmt --all
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
python3 scripts/smoke.py --binary target/release/ronnia --keep
python3 scripts/scale.py --binary target/release/ronnia --channels 100 --requests 40 --keep
```

Rust verification passed 137 tests, including deterministic worker/admission limits, channel FIFO/fairness, panic/cancellation cleanup, pending/ready/active cleanup, and joining active work at shutdown. Real-socket tests cover independent attachment deadlines, four simultaneous connection attempts, cap mismatch, more-than-64 degradation, foreign reconnect origins, transient failed handoff followed by retry while old/peer notifications continue, and reader abort-and-join. Maintenance regressions cover either membership read failing without consuming the grant edge, exact targeted-action overflow fallback, stale event revisions, owned deletion while creation is topology-gated, the final snapshot after deferred creation passes, and session recovery: a transition missed while a shard was disconnected is repaired by the reconnect-triggered audit plus one guarded full snapshot, while existing owned subscriptions are neither recreated nor deleted. Immediate and migration-grace grant activation each retain one targeted correction without repeated lookups on unrelated settings writes.

The complete existing smoke passed HTTP/OAuth, both grant-activation regressions, delivery/cooldown bookkeeping, ambiguous non-replay, DNS/connect/handoff/keepalive recovery, revocation, and executable maintenance commands. Artifact: `/tmp/ronnia-smoke-oiiwwmpb`.

The scale lifecycle used threshold 2, cap 3, four request workers, 32 total admission permits, 30-second HTTP timeout, and a 600-second reconcile interval. Five active grants grew one conduit to three attached shards and 20 subscriptions; additional grants remained eligible at the cap, removal did not shrink, and restarts reused the persisted topology. A lower configured cap attached all existing remote shards. Isolated reconnects injected two process-local DNS failures while another shard delivered actual PMs and the API stayed ready. The scenario also exercised overlap deduplication, make-before-break, wrong-shard acknowledgements, independent nine-second deadlines, 12-second subscription GET/POST stalls, online/offline and late-result races, targeted-action pressure, held channel-A PM response with channel-B progress, cooldown serialization, failed/ambiguous non-replay, and cancellation with an unwelcomed candidate.

The separate fresh 100-channel workload created all 400 subscriptions in **81.845 seconds**, including two paced creation passes and continuation rather than waiting 600 seconds. Only then was request traffic injected:

| Batch | Injected | Successful | Overflow rejected | Duplicate PMs | Duplicate statistics | Elapsed seconds |
|---|---:|---:|---:|---:|---:|---:|
| Held-response saturation | 40 | 32 | 8 | 0 | 0 | 31.120 |
| After drain | 16 | 16 | 0 | 0 | 0 | 15.991 |

All admitted requests reached terminal outcomes; the workload had 49 successful PM/statistic pairs including a fresh post-restart request. Lifecycle safety cases retained one uncertain and one definite failure without successful statistics or replay. Maximum API probe latency was **0.0109 seconds** across 650 workload probes and **0.0180 seconds** across 369 lifecycle probes; probes alternated readiness and SQLite-backed live listing with five-second timeouts. Linux peak RSS was **16,936 KiB** for the workload and **19,236 KiB** for the lifecycle. Both finished with zero fixture sockets remaining. Scale artifact: `/tmp/ronnia-scale-hhiods8b`.

These are synthetic observations, not promised production channel capacity, equal broadcaster distribution, or throughput. Global provider pacing and SQLite ownership/mailbox/WAL/FULL durability are unchanged. The audit remains bounded to 100 pages and 10,000 owned subscriptions; more than 2,500 active grants reports degraded capacity without partial creation. Private Rust assertions—not external guesses about Tokio task counts—prove worker, permit, queue, and reader-handle invariants. TLS/real-provider interoperability and container bot-mode load were not exercised.

### Configured Railpack image

Downloaded official pinned Railpack **0.43.0** using the [documented GitHub release installation route](https://railpack.com/installation); the Linux archive matched published SHA-256 `248a8f60f36854aabed6124113631078a802de3461660855fc4de9c26065c1fb`. A disposable `moby/buildkit:v0.23.2` supplied BuildKit. The actual root plan selected the configured Rust provider and Rust 1.99.1, retained Cargo release commands, and started `./bin/ronnia`; no root `Dockerfile` was introduced.

```sh
railpack plan . --out /tmp/ronnia-railpack-plan.json
BUILDKIT_HOST=docker-container://ronnia-scale-buildkit \
  railpack build . --name ronnia-scale-api-proof:local --progress plain
```

The final image built from the final sources in **187.30 seconds** with retained builder caches; this is not a controlled build benchmark. Image manifest: `sha256:4955cf3e45babfdc1bc4a8717c7f2772018585c159a495dbf3b0a6ccfc99753e`.

Ran its unmodified default command with `BOT_ENABLED=false`, an ephemeral loopback port, and isolated temporary `/data`. `/ready` returned HTTP 200 before and after restart; both graceful stops exited 0; SQLite integrity was `ok` with zero foreign-key violations. A marker inserted while stopped survived restart. The temporary bind-mounted files needed ownership adjustment for the host-side marker write; no application durability setting changed. Runtime proof: `/tmp/ronnia-scale-final-container-zf28xha3/proof.json` (image `sha256:4955cf3e…`; two earlier checks on production-identical sources are at `/tmp/ronnia-scale-final-container-x681cffo/proof.json` and `/tmp/ronnia-scale-final-container-8gvjxk8f/proof.json`). Runtime containers and the disposable BuildKit container were removed. This is API-only container evidence; bot behavior was proved by the release-binary scenarios above, not by container bot-mode load.


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

The runtime now tracks enabled accounts and active grants separately. An already-enabled legacy account receives a targeted stream-state lookup when its verified grant becomes active, including the delayed migration-grace notification. New enable lookups, subscription reconciliation, revision guards, and targeted retries remain on their existing paths; unrelated settings notifications do not repeatedly poll Helix.

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

The updated smoke ran the newly built release executable, completed startup before granting the already-enabled fixture account, and verified an offline-to-online/osu! transition immediately after OAuth consent with exactly one stream lookup. A separate restart with two-second migration grace verifies no premature eligibility/polling, activation via the real delayed OAuth notification, and no repeated stream lookups from unrelated settings writes. Both paths keep `RECONCILE_INTERVAL_SECS=600` and emit no stream-state event to supply the missing state. The full existing smoke also passed enable/delete wakeups, stale-snapshot ordering, delivery, transport recovery, and maintenance coverage. Artifact: `/tmp/ronnia-smoke-tlwulf02`. The complete sequential formatting/regression/clippy/test/build/smoke command finished in 169.50 seconds. No live providers, credentials, deployments, or PR #56 files were touched.

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

The actual root-built release executable passed all 16 original HTTP routes plus `/health` and `/ready`, maintenance, both signup orders, encrypted device authorization, EventSub delivery/recovery, hybrid state, ambiguous-delivery safety, and isolated same-process DNS recovery. Artifact: `/tmp/ronnia-smoke-g3q1gr6z`. Rust tests also included zero-test binary and doc-test suites; none failed. The complete sequential check command finished in 220.71 seconds. These checks used this checkout's `target/`, not a borrowed executable.

The root Dockerfile also built successfully as `ronnia:split-web-runtime-20261104` (manifest `sha256:96583bea15970d4bc73ba1e1aa91ecf1e843098655e820deeb6ea1941b8ec8b4`). The frontend verification owner exercised two real, separate Compose projects on the disposable `ronnia-split-proof-20261104` network with synthetic configuration, a fresh backend volume, API-only mode, and loopback ports 28857/23857. Chromium rendered the dashboard and provider links; `/api/ready` returned HTTP 200 with `{"status":"ready"}`, `/api/user/me` returned HTTP 200 `null`, and `/api/internal/` returned Nginx HTTP 404. Both projects, their network, and the disposable database volume were removed afterward. No live credentials, deployments, or provider OAuth calls were used. Browser external fonts were intentionally blocked by the localhost-only test policy.

The dashboard changes and frontend-specific checks are in [ronnia-web PR #24](https://github.com/aticie/ronnia-web/pull/24). This split proof is API-only; historical full-bot and provider-simulator coverage is recorded separately below.

Backend `compose.yaml` now contains only the backend, loopback-published on `BIND_PORT` (default 8080). Start backend Compose first to create `${RONNIA_NETWORK:-ronnia}`; the separate frontend Compose joins that external network and defaults its upstream to `http://ronnia:${BIND_PORT:-8080}`. Both projects need matching `BIND_PORT` and `RONNIA_NETWORK`. The frontend repository owns its browser/build checks; the current cross-project proof above supersedes earlier integrated two-service Compose evidence for this layout.


## Static and unit verification

```text
cargo fmt --all -- --check                         PASS
cargo clippy --locked --all-targets -- -D warnings PASS
cargo test --locked                                 PASS — 103 tests
cargo build --release --locked                      PASS
```

The 103 tests include: request parsing and mods; policy and formatting; EventSub frame/timeout/handoff and loopback URL validation; conduit reuse and owned-only subscription reconciliation; hybrid stream-state ordering and stale-snapshot races; targeted category lookup; account grants and consent; encrypted credential rotation/tamper/revocation; OAuth both-order signup and CSRF/session checks; all HTTP route contracts; OpenAPI generation; SQLite v1→v4 upgrade; cache freshness and deterministic set mapping; delivery deduplication and ambiguous outcomes; and shutdown/cancellation behavior.

## End-to-end executable smoke

Command:

```sh
python3 scripts/smoke.py --binary target/release/ronnia --keep
```

Latest result: **PASS** (97.43 s), artifact `/tmp/ronnia-smoke-6gu81j80`. The harness had also passed twice before the final loopback-validation adjustment.

Exercised through the actual binary, real HTTP/WebSocket servers, and SQLite:

- All 16 original routes plus `/health` and `/ready`; authenticated and anonymous shapes; debug `/openapi.json`, `/docs`, `/redoc` gated by `DEBUG_MODE`.
- Both signup orders with verified Twitch `channel:bot` consent; accounts inactive until reauthorization; same/different identity relinking rules; session rotation/revocation and CSRF/origin checks.
- Real device-code bot authorization: pending → browser activation → success; encrypted-only database evidence; invalid scope/denial/different-bot refusal; token refresh after 401.
- EventSub conduit creation/reuse, WebSocket welcome/shard attach, owned subscription reconciliation, `channel.chat.message` → osu! PM → statistics → Twitch acknowledgement.
- Hybrid state: online with targeted category lookup, immediate offline/category deactivation, out-of-order event rejection, slow snapshot versus newer offline race, grant/enable/delete wakeups, short reconciliation repair.
- Same-process DNS recovery: two actual `EAI_NONAME` failures followed by resolution, refusal, reconnect, and delivery. Injected with a process-local `LD_PRELOAD` resolver shim; no `/etc/hosts`, host DNS, or global configuration changes.
- Connection loss and reattachment without duplicating owned subscriptions; server `session_reconnect` make-before-break with old-connection delivery and close ordering; keepalive-timeout recovery where Ping/Pong frames do not extend the deadline.
- Authorization revocation without deleting the account or entering a resubscribe hot loop; `is_sent=false`, 429 `Retry-After`, and missing-success acknowledgement handling.
- Ambiguous osu! POST retained and not replayed, including after restart; conduit/subscription reuse on restart.
- Dry-run behavior; maintenance integrity check, backup/restore, delivery inspection, and backfill through the real CLI.

A temporary ignored integration probe using the same `tokio-tungstenite` dependency connected to the real `wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30`, completed the TLS/WebSocket upgrade, and received a `session_welcome`. It passed in 0.33 s; the throwaway probe was then removed.

Not covered: authenticated live Twitch/osu! provider calls, real EventSub outage retention behavior, provider account restrictions, and production load.

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
- The updated executable smoke harness passed all route, maintenance, authorization, EventSub recovery, and hybrid-state scenarios using separate IP and port variables (96.90 seconds). Compose's missing-port fallback was also checked: listener, target, and published port all default to 8080.

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
- Real wire integration: a fresh Rust service with the internal token enabled and a fixture account was exercised through the patched Python `LegacyAPI`. Pagination, ID/login lookup, claim, duplicate-claim rejection, delivery reporting, idempotent statistic replay, public migration status, and wrong-token rejection all passed.
- The persisted delivery was `sent` with `source=legacy`; exactly one statistics row with `source_id=twitch:<message>` and the original beatmap ID existed. Inserting a grant immediately removed the channel from the legacy list and caused a new claim to be rejected as migrated.
- The patched `LegacyAPI.report_live` endpoint was exercised against a real isolated service: it updated the legacy login, exposed the channel through the public live-users union, and an empty live report cleared it. The legacy by-login lookup then resolved the updated name.
- This verifies the anti-duplication contract at the API boundary; real Twitch IRC and osu! delivery were not used.
- A consistent backup of the live Compose SQLite database was created before applying the coexistence migrations; post-upgrade integrity and foreign-key checks passed.
- The deployed local stack returned public migration status `active:true` with cutoff `2026-12-04T00:00:00Z`; an authenticated internal request returned legacy channels, no-auth returned 404, and a wrong bearer token returned 401. The dashboard proxy blocked `/api/internal/*` with 404.
- Chromium rendered the real dashboard banner with `role=status`, the localized cutoff, and the settings action. A scroll-position check exposed the banner initially above the viewport; it was changed to sticky and reverified at maximum scroll (`y=0`, visible) before the final deployment.

## Sentry and Railway observability

- Sentry SDK 0.49.5 native ingestion was exercised against a local HTTP envelope collector with the real release executable and synthetic Twitch/osu! providers; no production Sentry data was emitted.
- At trace sampling `1`, the collector received request transactions, provider child spans, trace-linked metrics, and an osu! failure issue correlated to its originating request. At sampling `0`, errors and all twelve native metric families still arrived, without transactions.
- Actual WebSocket chat → osu! PM → Twitch feedback scenarios covered metadata 500/401 recovery, Twitch 401/429 retries, definite chat rejection, a missing acknowledgement, and a failed PM. Rejected/uncertain sends were not replayed.
- Saturation reported the originating message and the shared admission budget; queued work drained and database readiness remained healthy.
- Railway JSON and Sentry envelopes were checked for raw chat text, outgoing message bodies, and synthetic access/refresh/client-secret/device credentials. Metric dimensions excluded channel/requester/message/beatmap IDs and raw endpoints/URLs.
- Formatting, Clippy with warnings denied, all 137 Rust tests, the release build, and the existing executable smoke harness passed with the native Sentry changes.
- Configuration scenarios exercised absent/`0`/`0.1`/`1` trace sampling, an empty DSN, nonfinite/malformed/out-of-range sampling, and a malformed DSN. Invalid telemetry configuration failed with the relevant setting name before database initialization, without a panic.
- Two channels were exercised concurrently with one PM deliberately held and Railway logging reduced to `WARN`. The other channel continued, both feedback operations retained distinct request traces, and native metrics/errors still arrived independently of log verbosity.

## Limits

- No real Twitch or osu! account, chat message, OAuth consent, or EventSub production stream was used. The production EventSub endpoint was reached only for an unauthenticated TLS handshake/welcome.
- The patched old Python bot was tested against local fake Twitch/osu/new-API fixtures, not a live IRC connection or real users; its live Twitch/osu announcement delivery remains to be observed during rollout.
- No production-scale channel count, message throughput, rate-limit behavior, or long-duration memory measurement.
- Binary/agent results are only claimed where command output above confirms them.
