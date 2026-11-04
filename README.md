# Ronnia (Rust)

One service for the Twitch → osu! private-message relay and the Ronnia account API. SQLite replaces MongoDB; the bot and API share a schema and lifecycle. This repository ports the current `ronnia` and `ronnia-backend` implementations, not obsolete README descriptions of Azure Service Bus, direct osu! IRC, or unimplemented chat commands.

The Vue dashboard lives separately in [aticie/ronnia-web](https://github.com/aticie/ronnia-web). This repository contains the Rust bot/backend at its root, with no Python application or frontend source. Each repository has its own Compose deployment.

## Design

- Tokio runtime with two worker threads, Axum HTTP API, a persistent Twitch EventSub Conduit WebSocket, and shared Reqwest HTTP pools.
- Current Twitch authorization replaces pasted IRC tokens: streamers consent to `channel:bot`, and the bot account is authorized once through the device-code browser flow. Bot credentials are AES-256-GCM encrypted in SQLite with an external key and renewed automatically.
- A dedicated database thread owns SQLite. A bounded queue prevents database work from blocking Tokio or spawning unbounded blocking jobs.
- SQLite WAL, foreign keys, explicit migrations, and unique Twitch/osu! **IDs**. Cached login names are not identity keys: Twitch names can change ownership.
- Bounded request/feedback queues, sequential request delivery, disk-backed metadata, indexed statistics, and coordinated token refresh.
- One Rust process and one persistent **local** database volume. The separately deployed dashboard uses static Nginx; Node is used only to build it. Do not place SQLite on NFS/shared network storage or run replicas against independent copies.
- No production MongoDB service or Python runtime. BSON support is for the offline importer only.

The approved cutover requires fresh website login. Old JWT cookies are not accepted. Ambiguous osu! sends are retained for inspection, not automatically replayed.

## Build and run

Run these commands from the `aticie/ronnia` repository root. Install current stable Rust, a C toolchain (for bundled SQLite), and Python 3 for the integration harness. The Python files in `scripts/` are verification tools, not the removed legacy application.

```sh
cargo build --release --locked

# Generate the encryption key once and store it as a secret.
target/release/ronnia generate-token-key

# With provider app credentials, encryption key, and database available,
# authorize the bot account by opening the printed Twitch URL:
target/release/ronnia --database ./ronnia.sqlite3 authorize-bot --login YOUR_BOT_LOGIN

# API-only local startup, without bot or provider credentials:
DATABASE_PATH=./ronnia.sqlite3 target/release/ronnia serve --api-only

# Full service (API + EventSub bot):
DATABASE_PATH=./ronnia.sqlite3 target/release/ronnia serve
```

`serve` is the default command. `authorize-bot` requests `user:read:chat user:write:chat user:bot` for the bot account, validates the result, and stores only encrypted credentials. Use `--replace` only to intentionally replace a different bot account. A missing authorization is a recoverable waiting state; the API stays available. Local runs read the process environment, **not** a `.env` file. Docker Compose loads `.env`; copy `.env.example` and fill in provider credentials there. Never commit credentials, exports, or the live database.

### Configuration

| Variable | Default / meaning |
|---|---|
| `DATABASE_PATH` | `ronnia.sqlite3`; override with global `--database PATH` |
| `BIND_IP` | `0.0.0.0`; listener IPv4 or IPv6 address |
| `BIND_PORT` | `8080`; listener port and Compose published port |
| `WEB_PORT` | `3000`; dashboard port in the separate `ronnia-web` Compose project, not a backend setting |
| `PUBLIC_ORIGIN` | `http://localhost:3000`; exact allowed website origin |
| `API_ORIGIN` | `http://localhost:8080`; public API origin, also allowed for same-origin requests |
| `COOKIE_SECURE` | True for HTTPS API origin, otherwise false; cannot disable for HTTPS |
| `TWITCH_CLIENT_ID`, `TWITCH_CLIENT_SECRET` | Twitch app/website credentials |
| `TWITCH_REDIRECT_URI` | `${API_ORIGIN}/oauth2/twitch-redirect` |
| `OSU_CLIENT_ID`, `OSU_CLIENT_SECRET` | osu! app credentials; relay requires delegated chat permission |
| `OSU_REDIRECT_URI` | `${API_ORIGIN}/oauth2/osu-redirect` |
| `TWITCH_TOKEN_KEY` | 64 hex characters from `generate-token-key`; required when the bot is enabled. Losing it makes stored bot credentials unrecoverable |
| `TWITCH_EVENTSUB_URL` | `wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30`; `ws://` is accepted only for isolated local tests |
| `BOT_ENABLED` | `true`; `serve --api-only` overrides to false |
| `DRY_RUN` | `false`; true suppresses all outbound chat and request statistics |
| `ENVIRONMENT` | Legacy `testing` also enables dry-run |
| `DEBUG_MODE` | `false`; true enables `/openapi.json`, Swagger UI `/docs`, and ReDoc `/redoc`; secure CORS remains enforced |
| `RECONCILE_INTERVAL_SECS` | `600`, positive; EventSub drives live changes, with a periodic snapshot/remote audit to repair missed events |
| `REQUEST_QUEUE_CAPACITY` | `128`, positive; overload is logged instead of unbounded accumulation |
| `HTTP_TIMEOUT_SECS` | `20`, positive |
| `SESSION_TTL_SECS` | `2592000` (30 days) |
| `OAUTH_TTL_SECS` | `600` |
| `MIGRATION_ACTIVE` | `false`; shows the dashboard migration banner while set |
| `MIGRATION_CUTOFF_AT` | Optional RFC 3339 timestamp displayed by the banner; does not stop old services automatically |
| `MIGRATION_GRANT_GRACE_SECS` | `10`; handoff delay before the new bot subscribes to a newly consented channel |
| `INTERNAL_API_TOKEN` | Optional strong shared secret for the old bot's legacy bridge. Never expose to browsers |
| `LOG_LEVEL` / `RUST_LOG` | `info`; `RUST_LOG` takes precedence |
| `SENTRY_DSN` | Optional; errors/panics reported, default PII and full-rate tracing disabled |

Provider endpoint overrides exist for isolated integration runs: `TWITCH_API_BASE`, `TWITCH_OAUTH_BASE`, `OSU_API_BASE`, `OSU_OAUTH_BASE`, `TWITCH_EVENTSUB_URL`. Defaults use official providers. Never use unencrypted WebSocket or HTTP endpoints in production.

Register the exact website callback URLs with Twitch and osu!. Streamer Twitch signup requests `channel:bot`; reauthorization is required at cutover because imported records did not grant current chat permission. The bot user authorization is separate and stored encrypted. App, osu! public, osu! delegated, and bot tokens refresh independently. Do not log configuration objects or credentials.

### Container

```sh
cp .env.example .env
# Fill in credentials and TWITCH_TOKEN_KEY, then start the backend:
docker compose up --build -d
# Start the dashboard separately from the ronnia-web checkout (see below).
# First-time bot authorization, stored in the same persistent volume:
docker compose exec ronnia ronnia authorize-bot --login YOUR_BOT_LOGIN
docker compose logs -f ronnia
```

Compose starts only `ronnia` (`serve` with `BOT_ENABLED=true`: account API, Twitch/EventSub bot, and embedded SQLite). No separate MongoDB or bot container is required. The bot authorization command prints a Twitch URL and code; complete it in a browser as the bot account. This is separate from registering your streamer through the dashboard. The API stays available while the bot waits, and the bot picks up its completed grant without a restart. Authorization performed locally against `./ronnia.sqlite3` is not automatically copied into Compose's named volume; run the container command above or explicitly migrate the database.

The image runs as UID/GID 10001 and stores SQLite in the persistent `ronnia-data` volume at `/data/ronnia.sqlite3`. Compose pins this path so an unrelated `DATABASE_PATH` in `.env` cannot bypass the volume. Compose reads `BIND_PORT` from the project `.env` and uses it for both the container listener and published host port (default 8080). It keeps the host binding on `127.0.0.1` and forces the listener IP inside the container to `0.0.0.0` so Docker forwarding works even when local runs use a loopback `BIND_IP`. Put a TLS reverse proxy in front. Supply the real public origins and HTTPS callback URLs independently of the bind address. Keep `/data` persistent; do not use `docker compose down --volumes` on a deployment whose data you need. The build context excludes `.env`, data, and exports.

For example, `.env` can contain `BIND_IP=0.0.0.0` and `BIND_PORT=4343`; Compose will publish `127.0.0.1:4343` to port 4343 in the container. Direct CLI runs still require exporting the `.env` variables (`set -a; source ./.env; set +a` in Bash/Zsh, for a trusted shell-compatible file). The old combined `BIND` setting is replaced by these two variables.

`GET /health` reports process liveness; `GET /ready` checks database readiness. Neither endpoint claims Twitch is connected. A Twitch outage must not take the account API offline. Diagnostics use JSON stderr logs.

### Railway (native Railpack)

The backend root is a Rust-only Railpack build. [`railpack.json`](railpack.json) selects the official Rust provider and Rust 1.99.1, adds runtime CA certificates, and defaults to `DATABASE_PATH=/data/ronnia.sqlite3`, `BIND_IP=0.0.0.0`, and `BIND_PORT=8080`. The provider compiles the committed Cargo manifest/lockfile in release mode and starts `./bin/ronnia` (`serve` is the CLI default). No Cargo command or cache implementation is duplicated here. The existing release profile (`lto="thin"`, one codegen unit, stripped debuginfo), bounded runtime, and bundled SQLite remain unchanged; no runtime SQLite package, OpenSSL development package, Python, or Node is required.

Railway's [builder precedence](https://docs.railway.com/config-as-code/reference#specify-the-builder) says a detected root `Dockerfile` wins even when the builder is Railpack. The retained Docker image is therefore named `Dockerfile.docker`; Compose and CI pass that path explicitly. For a direct Docker build use `docker build -f Dockerfile.docker -t ronnia .`. [`railpack.json`](https://railpack.com/config/file) configures Railpack itself, **not** Railway's service builder.

To switch an existing backend service (or configure a new one), review its Railway settings before redeploying:

1. Use this backend repository and the repository root as the source. Select **Railpack** as the builder. Clear any custom Dockerfile path, `RAILWAY_DOCKERFILE_PATH`, obsolete build command, and old Docker/Python start command. Leave build/start overrides empty so the native provider supplies both; the generated start command is `./bin/ronnia`, not the Docker image's `/usr/local/bin/ronnia`.
2. Attach the existing SQLite volume at **`/data`**, or create a volume there for a fresh installation. Set `DATABASE_PATH=/data/ronnia.sqlite3` explicitly in service variables. Keep one replica and one local SQLite database; do not substitute ephemeral container storage or a shared network filesystem. Back up the current database before switching. If the old service used another path, migrate it deliberately; a new empty volume does not contain existing accounts or encrypted bot grants.
3. Set `BIND_IP=0.0.0.0`, **`BIND_PORT=8080` and `PORT=8080`**, and route the Railway domain to target port 8080. The application intentionally retains its existing `BIND_PORT` contract; it does not consume `PORT`. Railway [uses `PORT` for healthchecks](https://docs.railway.com/deployments/healthchecks#configure-the-healthcheck-port), so keep both variables and the domain target equal if changing ports. Configure healthcheck path **`/ready`**. SQLite migrations run on normal process startup with the volume mounted, not as a pre-deploy command.
4. Keep the provider credentials, `TWITCH_TOKEN_KEY`, public HTTPS origins/callbacks, and any coexistence variables from the configuration table above. Do not regenerate the encryption key when reusing encrypted grants. For an API-only bring-up set `BOT_ENABLED=false`; restore `true` for the full bot. Authorize the bot with `./bin/ronnia authorize-bot --login YOUR_BOT_LOGIN` in the running service, against the same `/data` database. Deploy the separate dashboard independently; no frontend is built here.
5. Redeploy and confirm the logs show Railpack's Rust provider (not “Using detected Dockerfile”), release compilation, and start command `./bin/ronnia`; then check `/ready`. Attached-volume redeploys have a brief interruption, not zero-downtime overlapping SQLite writers.

No deprecated `railway.toml`/`railway.json` is added: [current Railway documentation](https://docs.railway.com/config-as-code) permits those only for existing Config-as-Code services until 2026-12-01. If an existing service already has one configured elsewhere, remove/migrate its obsolete builder/start overrides as part of the switch. Railway's newer project-level IaC is optional and must be reviewed/applied separately; this repository does not provision or mutate Railway infrastructure on push.

**Build cache, not database persistence:** the [official Rust provider](https://railpack.com/languages/rust#buildkit-caching) defines `cargo_registry` (`/root/.cargo/registry`), `cargo_git` (`/root/.cargo/git`), and `cargo_target` (`target`) BuildKit caches. Our configuration extends the native build step to mount the provider's registry/git caches alongside its default target cache; this matters because the provider's separate dummy-source install step can be optimized out. No provider commands are replaced. Cache mounts are not shipped as runtime files; the executable is copied to `bin` before packaging. Reuse depends on the builder retaining compatible cache state; a cold/evicted cache, dependency/toolchain/profile changes, or changed source can still require compilation. This is not a measured speedup or lower-runtime-RSS claim. [Railway supports Dockerfile cache mounts too](https://docs.railway.com/builds/dockerfiles#cache-mounts); the benefit here is managed Cargo caching without service-ID-specific Docker cache instructions. Only the `/data` volume persists SQLite across deployments.

### Register with the local dashboard

Start backend Compose first to create the shared Docker network `${RONNIA_NETWORK:-ronnia}`. Clone [aticie/ronnia-web](https://github.com/aticie/ronnia-web) separately and run `docker compose up --build -d` from that checkout. Its Compose project joins that existing external network, publishes `WEB_PORT` (default 3000), and defaults `API_UPSTREAM` to `http://ronnia:${BIND_PORT:-8080}`. Set the same `BIND_PORT` and, if overridden, `RONNIA_NETWORK` in both projects. `API_UPSTREAM` can be overridden in the frontend project for other deployment topologies. The backend remains published on host loopback only; frontend proxy traffic reaches it directly through the shared private Docker network, without a host-gateway mapping. Stop the frontend project before taking down the backend project's network.

Open **http://localhost:3000** and sign in with either Twitch or osu!. The first provider creates a pending signup; the page then offers only the other provider. Complete both to open account settings. Twitch asks for `channel:bot` consent for the streamer account. Existing accounts missing that consent show **Re-authorize Twitch** in settings.

Use `localhost` consistently in the browser and callback URLs; do not mix it with `127.0.0.1`, because cookies are host-specific. Browser requests use `/api/...` on the frontend origin; Nginx strips `/api` when forwarding to the backend. Provider callbacks go **directly to the backend**, not through the frontend `/api` proxy.

With the default ports, register these exact provider callback URLs:

```text
Twitch: http://localhost:8080/oauth2/twitch-redirect
osu!:   http://localhost:8080/oauth2/osu-redirect
```

Set `PUBLIC_ORIGIN=http://localhost:3000`, `API_ORIGIN=http://localhost:8080`, and `COOKIE_SECURE=false` for local HTTP. Match `TWITCH_REDIRECT_URI` and `OSU_REDIRECT_URI` to those callbacks. If changing ports, update both projects, origins, and provider registrations consistently. Production HTTPS deployments require actual HTTPS origins/callbacks and `COOKIE_SECURE=true`. These are new-checkout instructions; existing configured deployments and registrations are not changed automatically.

See the [frontend README](https://github.com/aticie/ronnia-web#readme) for Node/Vite development and frontend behavior. Its production container receives only the API upstream address, not this repository's `.env` or credentials.


## Coexistence with the old bot

During the migration month, users use **only the new dashboard/API**; the old Python bot remains an IRC request relay for channels that have not yet granted `channel:bot`. SQLite is the sole account/settings authority. Do not keep the old website/backend as a second writable account source.

1. Import the Mongo collections once as described below.
2. Set `MIGRATION_ACTIVE=true`, a `MIGRATION_CUTOFF_AT` date, and a strong `INTERNAL_API_TOKEN` in the new service environment.
3. Start the new stack. New-bot subscriptions activate only for consented channels after the handoff grace.
4. Prepare a **separate legacy checkout** of [PR #56 (`migration/coexistence-bridge`)](https://github.com/aticie/ronnia/pull/56), for example `../ronnia-legacy`. That PR is independently reviewed/deployed and is not included in this Rust branch. Create `legacy-bot.env` from `legacy-bot.env.example`, fill in existing Twitch/osu/Mongo values, and copy `INTERNAL_API_TOKEN` from the new `.env`. Run the patched Python bot from the legacy checkout with that environment, following that checkout's deployment instructions. Do not try to run it from this Rust root.

Deploy the PR #56 bridge from that retained legacy checkout/release **before** the Rust-only cutover removes Python from `master`. Keep that legacy release available throughout coexistence. PR #56 does not need to be merged into this Rust branch; the two branches have separate deployment roles.

For a legacy process on the same host, use `RONNIA_API_URL=http://127.0.0.1:8080` (or the configured backend port). A container must use an explicitly reachable backend address; `ronnia` resolves only if you deliberately attach it to the backend Compose network. Set `RONNIA_LEGACY_MODE=false` only to deliberately revert to its old Mongo user behavior.

The old bot then reads channel policy and eligibility from the new API, claims each IRC request before sending an osu! PM, and reports confirmed statistics back. It also reports its current live legacy set and Twitch logins every poll, so the new dashboard shows legacy streamers and names stay current without giving the old bot account authority. Its Mongo `Users`/`Settings` are not read in bridge mode, and accounts are never deleted by IRC join failures. If the new API is unavailable or a channel has migrated, no PM is sent.

Fresh legacy live reports (within three minutes) are merged into the dashboard's public live list; stale reports disappear if the old bot stops reporting. When a previously live legacy channel grant is recorded, it is immediately removed from the legacy channel and live lists; a claimed delivery blocks the new bot from processing the same Twitch message. The new bot waits `MIGRATION_GRANT_GRACE_SECS` before activating the channel, preventing overlap. Request history from both bots lands in the new SQLite database.

The old bot announces migration once per live session to each unconverted streamer, sending both a Twitch channel message and an osu! PM containing `RONNIA_MIGRATION_URL`. The announcement is re-armed after the channel goes offline and is suppressed in testing/dry-run mode. A failed Twitch/osu! send is logged and is not retried during that session. The tracker is process-local, so restarting the old bot while a stream is live can repeat the announcement once for that session.

At the cutoff, stop the old bot and set `MIGRATION_ACTIVE=false` to remove the banner. No second data merge is required. Stop the old Mongo once the bridge is retired and its beatmap cache is no longer needed.

The `/internal/legacy/*` routes are unavailable without `INTERNAL_API_TOKEN`; the dashboard's `/api/internal/*` proxy path is explicitly blocked. Use a long random token and keep it only in the two services' environments.

## MongoDB → SQLite cutover

1. Stop the Python bot and backend writers for a consistent final export. Back up MongoDB first; the importer never connects to or modifies it.
2. Export the `Ronnia` database's **Users, Settings, Statistics, Beatmaps** collections. Use either uncompressed `mongodump` BSON files or one `mongoexport` JSON file per collection. Empty collections still need empty files/arrays.
3. Place exactly one supported file per collection in a directory, with exact case-sensitive names, e.g. `Users.json`, `Settings.json`, `Statistics.json`, `Beatmaps.json`. JSON arrays, NDJSON, `.jsonl`, `.ndjson`, and `.bson` are supported; decompress `.gz` first. `*.metadata.json` from mongodump is ignored.
4. Validate against the intended destination, then import while the Rust service is stopped:

```sh
target/release/ronnia --database /path/ronnia.sqlite3 import --directory /path/exports --check
target/release/ronnia --database /path/ronnia.sqlite3 import --directory /path/exports
target/release/ronnia --database /path/ronnia.sqlite3 check
```

The report lists per-collection counts, issues, normalization notes, and whether changes committed. Any validation issue aborts the **entire** import. Data is streamed through transactional SQLite staging; the whole database is not loaded into memory. At most 1000 detailed issues are retained, with total issue count/truncation reported. Resolve conflicts explicitly and revalidate; never silently choose a duplicate provider account.

The importer recognizes Extended JSON integers/dates, canonicalizes old snake_case settings aliases, normalizes exclusions/logins, and retains original documents/unknown fields alongside typed data. Missing cooldown defaults to 30 seconds. Invalid/incomplete accounts, conflicting alias values, malformed ranges, duplicate IDs, malformed JSON/BSON, and ambiguous input files are errors. Timestamps use UTC epoch seconds; raw documents preserve source subsecond precision. Upper star bounds of 10 or more retain the old API's unlimited convention. Statistics without source `_id` retain every occurrence and receive synthetic export-SHA256-plus-ordinal identities: replaying the same ordered Statistics export rejects the entire import without adding rows, even if JSON whitespace or object-key order changes. Original raw documents are unchanged. Genuinely different or reordered IDless exports cannot be deduplicated against overlapping historical events without their original source identities.

5. Switch deployment to Rust and retain the Mongo backup for rollback. Users must log in again, and streamers must re-authorize Twitch so the app has `channel:bot` permission; imported accounts are not assumed to have consented. Until then their accounts/settings/history remain intact but their channels are not subscribed. Do not run old and new services simultaneously against divergent databases. The website remains external to this repository; verify its configured API origin and callback URLs at deployment.

### Maintenance

```sh
# Integrity + foreign keys:
target/release/ronnia --database /path/ronnia.sqlite3 check
# Consistent snapshot (destination must not exist):
target/release/ronnia --database /path/ronnia.sqlite3 backup --destination /backups/ronnia.sqlite3
# Fetch missing historical beatmap metadata, in keyset-paginated batches:
target/release/ronnia --database /path/ronnia.sqlite3 backfill
# Inspect failures and uncertain delivery outcomes:
target/release/ronnia --database /path/ronnia.sqlite3 deliveries --limit 50
```

Restore a backup with the service stopped into a fresh database path, then run `check` before switching `DATABASE_PATH`. Do not copy only the live SQLite main file while WAL writers run; use `backup` or stop the service first. Never delete active WAL/SHM files. Backups contain account data; protect file/volume permissions and retention.

Request history has no automatic retention cutoff, preserving current behavior. Metadata freshness expires after 24 hours for request lookups, but stored metadata remains available to historical leaderboards. Expired temporary authentication state and cooldown state are cleaned up. Uncertain deliveries remain available for operator inspection.

Chat subscriptions remain active while a channel is offline. Twitch documents a limit of 300 enabled subscriptions per WebSocket connection; the current single-shard conduit uses four subscriptions per channel, so deployments beyond roughly 75 authorized channels would need additional shards. That scaling is not implemented.

## Request behavior

The bot subscribes to chat and state events for every enabled, authorized channel through EventSub, including channels that are offline. `stream.online`, `stream.offline`, and `channel.update` update request eligibility immediately for channels live in osu! category ID `21465`; `stream.online` triggers a targeted category lookup because Twitch does not include it in that event. Batched snapshots and a remote subscription audit run at startup, after recovery, and every `RECONCILE_INTERVAL_SECS` (default ten minutes) to repair missed events. A slow snapshot cannot overwrite a newer event. Signup, reauthorization, settings changes, and account deletion wake reconciliation. It recognizes the first supported beatmap/set URL in each message, including historical `/b`, `/s`, and `/p/beatmap` forms, and forwards supported mods in their original order without pretending to calculate modded stars/BPM/duration. Set requests consistently use the first map returned by the set API, independent of unrelated map-cache contents. That selected-map mapping persists in SQLite for 24 hours; both the mapping and map must be fresh, so repeated set requests avoid another provider lookup.

| Setting | Default | Behavior |
|---|---|---|
| `enable` | true | Allow requests and membership eligibility |
| `echo` | true | Send Twitch acknowledgement after confirmed osu! delivery |
| `sub-only` | false | Require subscriber/founder, moderator, or VIP |
| `points-only` | false | Require a channel-point reward tag; no specific reward ID restriction |
| `test` | false | Bypass the live/osu-category gate and all per-request filters; does not override the master `enable` switch |
| `cooldown` | 30 | Seconds per channel/requester after successful delivery; persisted across restarts, not updated by user test mode |
| `sr` | `[0,-1]` | Inclusive base-star bounds; `-1` upper bound means unlimited |

Chat subscriptions are not tied to live status, so an offline streamer can reauthorize or change settings without waiting for the next stream. Ordinary requests received while offline or outside osu! are ignored; only users with `test` enabled can exercise the request pipeline in those states. Ordinary chat is not logged at `INFO` (message text is never logged); set `LOG_LEVEL=debug` for message receipt, no-link, and inactive-request diagnostics, or `LOG_LEVEL=trace` only when deeper protocol diagnostics are needed. Disabling a channel removes its chat and state subscriptions. Exclusions are case-insensitive. Outside user test mode the broadcaster cannot request in their own channel. Process dry-run still parses/looks up/filters, but sends no success or rejection chat and records no successful request statistics.

### Connection and delivery guarantees

The EventSub supervisor owns reconnect attempts outside the WebSocket client. DNS failures (including errors a resolver treats as terminal), refused/TLS failures, EOF, keepalive timeouts, and server close codes trigger capped exponential backoff with jitter until shutdown. The conduit and subscriptions survive reconnects; the new session is attached to the shard without resubscribing every channel. On `session_reconnect`, the new connection is established and welcomed before the old one is closed, and already-delivered notifications are deduplicated. Keepalives extend the liveness deadline; ping/pong frames do not. Subscription and chat-send responses are checked for real success, not just HTTP status. Temporary failures never delete registrations. App credentials and the encrypted bot grant refresh independently.

There is no claim to recover chat messages Twitch never delivered during an outage. Invalid or revoked bot authorization remains a degraded waiting state with backoff and explicit diagnostics, not an authentication hot loop or a fatal API failure.

Each accepted request is persisted before its osu! send. Only confirmed delivery produces success history, successful cooldown, and a Twitch success echo. If the response is lost after a possible send, the outcome becomes **uncertain** and is not replayed, including after restart. Known failures require a new chat message to resubmit. This chooses possible omission over duplicate private messages; upstream offers no exactly-once guarantee. Failed/pending requests do not consume successful cooldowns. Twitch acknowledgement failure never changes an already confirmed osu! result.

## HTTP compatibility

The 16 existing routes are retained:

| Group | Routes |
|---|---|
| OAuth | `GET /oauth2/osu-login`, `/oauth2/twitch-login`, `/oauth2/osu-redirect`, `/oauth2/twitch-redirect` |
| Account | `GET /user/me`, `GET /user/logout`, `DELETE /user/me` |
| Settings | `GET`, `POST /user/settings` |
| Exclusions | `GET`, `POST`, `DELETE /user/exclude` (`excluded_user` remains a query parameter) |
| Public | `GET /live/users`; `GET /requests/beatmaps/top/daily`, `/weekly`, `/monthly`; `GET /migration/status` |

The old bot also uses authenticated `/internal/legacy/*` endpoints for channel listings, request claims, and confirmed-delivery reporting. They are hidden when `INTERNAL_API_TOKEN` is unset, require a bearer token otherwise, and are blocked through the dashboard proxy. Keep the service on a private network or behind TLS; the token is a service credential, not a browser credential.

As in the original backend, developer documentation is disabled in production. Set `DEBUG_MODE=true` to expose OpenAPI, Swagger UI, and ReDoc. Browser documentation assets use their standard CDNs; serving the API itself has no CDN dependency.

User JSON remains camelCase and now includes `twitchBotAuthorized`, indicating whether the streamer has granted current chat permission. Settings retain hyphenated keys. Settings POST replaces known settings with supplied/default values rather than becoming PATCH. Pagination is bounded to 1–100 with nonnegative offset. Leaderboards use rolling UTC windows of 1/7/30 days and deterministic ties. BSON-only IDs and the invalid nested aggregation `result` are not exposed.

Authentication uses hashed opaque server-side sessions and browser-bound, expiring, single-use OAuth state. Cookies are HttpOnly, SameSite=Lax, and Secure for HTTPS. Old JWTs are invalid. Logout/deletion revoke sessions. Cookie-authenticated mutation/logout requests must send an allowed `Origin` or `Referer`; arbitrary redirect origins and wildcard credentialed CORS are rejected. Missing/expired/deleted-account authentication returns 401 rather than an internal error. Unauthenticated `GET /user/me` remains JSON null; pending signup remains visible through that endpoint.

See [DIFFERENCES.md](DIFFERENCES.md) for intentional fixes and [VERIFICATION.md](VERIFICATION.md) for exercised checks and measurement limits.

## Verification commands

```sh
cargo test --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
python3 scripts/smoke.py --binary target/release/ronnia
```

Smoke runs the actual executable against isolated local Twitch EventSub WebSocket/Helix/OAuth/osu HTTP simulators and a fresh SQLite database. It exercises the real device-code bot authorization, streamer consent, conduit/subscription lifecycle, reconnect handoff, hybrid live tracking, token refresh, and ambiguous delivery handling. On Linux with `cc`, a process-local resolver shim injects two DNS failures and later successful resolution into the same running service. It does not change host DNS or `/etc/hosts`. This is not a live-provider certification.

For the reproducible original-process memory comparison, prepare a sibling `../ronnia-legacy` checkout containing the original Python bot and its `.venv`, a sibling `../ronnia-backend` checkout with its Python dependency environment, a local Docker daemon, and the `mongo:7` image. These are read-only benchmark inputs, not this Rust root; use explicit flags for other checkout locations:

```sh
python3 scripts/memory.py --binary target/release/ronnia \
  --bot-source ../ronnia-legacy --bot-python ../ronnia-legacy/.venv/bin/python \
  --backend-source ../ronnia-backend --backend-python /tmp/ronnia-backend-baseline/bin/python \
  --output /tmp/ronnia-memory-results.json
```

The benchmark runs original Python bot/backend code with only local-provider destination redirection and an isolated real MongoDB. It compares the same small synthetic workload against Rust+SQLite, reports per-process/service RSS and PSS where available, and excludes the common provider simulators. Summed RSS may double-count shared pages. Results are workload-specific, not production peak-memory or capacity guarantees.

The Rust relay/API code retains the original Apache-2.0 license. Frontend source attribution and its unchanged upstream licensing status are documented in the [separate frontend README](https://github.com/aticie/ronnia-web#readme).
