# Intentional differences from the Python implementations

This is a clean Rust cutover, not a Python wrapper. The reference behavior is the legacy Python `aticie/ronnia` and `aticie/ronnia-backend` sources, not historical architecture descriptions. This branch contains the Rust bot/API at repository root; Python application/config/tests/assets are removed, but Python verification drivers remain in `scripts/`. Existing website routes, fields, settings, link families, relay formatting, request-history concepts, and both signup orders remain. Original source trees are not runtime dependencies.

## Reliability and delivery

| Python behavior / defect | Rust behavior |
|---|---|
| TwitchIO 2.x IRC with an operator-pasted user token; library-owned recovery and no outer DNS-recovery contract | Current EventSub Conduit over WebSocket with managed bot authorization; application-owned capped, jittered recovery for DNS/connect/TLS/EOF/keepalive/close failures until shutdown |
| Sixth channel-join failure deletes a registered user (`twitch_bot.py:278–284`) | Subscription failures never delete accounts; channel permission revocation marks consent revoked and waits for reauthorization |
| Manager sends IRC channel snapshots over fragile localhost TCP; EOF loops and empty snapshots | Designed-in EventSub subscriptions with no internal TCP transport; chat and state subscriptions cover every enabled, authorized channel regardless of stream state |
| User test mode cannot exercise the pipeline while offline or outside osu! because no chat was subscribed | Chat remains subscribed while offline; `test` mode bypasses the live/osu-category gate and request filters, while `enable=false` remains the master switch |
| Static `TMI_TOKEN` expires and cannot renew | Bot account uses the device-code browser flow; AES-256-GCM encrypted access/refresh credentials with external key, `/validate` checks, single-flight rotation, and revocation handling |
| Streamer Twitch signup requested an unused identity scope | Signup requests `channel:bot` with explicit verification; unsupported/mismatched consent is rejected, and imported accounts require reauthorization |
| All enabled users are polled every minute for live/category, and chat eligibility can lag | `stream.online`/`stream.offline`/`channel.update` update eligibility immediately, with a 10-minute reconciliation snapshot/audit and revision guards against stale responses |
| Helix 401 calls authentication that keeps the existing expired token, then recursively retries while holding permits (`clients/twitch.py:42–60`) | Single-flight token refresh; invalidate expired token, release resource permits, retry safely without recursive semaphore acquisition |
| osu! API failures parsed as ordinary success JSON; HTTP pacing races across concurrent calls | Status-aware errors, request deadlines, coordinated pacing, separate app/public/delegated credentials, shared pools |
| osu! PM, Twitch success echo, and statistics run concurrently; failures can falsely claim success | Persist delivery intent first; confirmed PM precedes success history/cooldown and acknowledgement |
| No durable delivery identity/outcome | Twitch message-ID deduplication and explicit pending/sending/sent/failed/uncertain outcomes; interrupted sends become uncertain |
| No explicit ambiguous-send policy | Do not replay ambiguous POSTs. Inspection via `deliveries`. Known failures require a new chat request. Possible omissions are preferred over duplicate PMs, as chosen in design |
| IRC acknowledges nothing explicit; chat success can be assumed from a send call | Helix Send Chat Message response must report `is_sent`; drop reasons are failures and ambiguous outcomes are never acknowledged or replayed |
| Twitch eligibility is eventually reconciled only on the next minute poll | Consent, enablement, live/category events, and hard failures trigger immediate reconciliation; batch snapshots are a repair mechanism, not the routine path |
| INFO logging includes every chat message; Sentry traces/profiles sample at 100% | Structured diagnostics without unconditional chat-content logging; optional Sentry error/panic reporting, no default PII/full-rate tracing |

There is no static Twitch IRC token and no TwitchIO dependency. Temporary DNS/network problems recover automatically; invalid or revoked bot authorization requires the operator to run `authorize-bot` again. Outage messages never delivered by Twitch cannot be reconstructed, and reconnection can cause duplicate deliveries, which are deduplicated locally.

## Parsing and policies

| Python behavior / defect | Rust behavior |
|---|---|
| Full parser crashes on legacy `?s=...` links because single-map parsing indexes a missing `b` result | Parse legacy map/set queries safely; malformed inputs do not crash processing |
| Regex host dots are unescaped and URL/numeric boundaries are loose | Parse actual supported hosts and IDs while preserving valid URL families and surrounding chat punctuation |
| Mod regex matches inside ordinary words | Recognized mod runs must have word boundaries; ordering/deduplication and display-only semantics remain |
| Cooldown model default 0, backend/default metadata 30 | One 30-second default; explicit stored values preserved |
| A channel's cooldown duration prunes entries belonging to other channels; inconsistent lookup/update keys | Stable channel/requester key, individually expired cooldowns, monotonic elapsed time |
| Cooldown can be consumed by a later policy rejection or failed PM | Only confirmed successful requests establish cooldown; test-mode requests do not alter ordinary cooldown state |
| Cooldown state disappears on restart | Successful cooldown receipts persist in SQLite; elapsed time within each process uses a monotonic clock |
| Subscriber predicate compares a boolean with string `"0"`, letting non-subscribers through | Correct boolean eligibility, including founder/mod/VIP handling |
| Star filter uses exclusive endpoints; either `-1` disables the whole check | Inclusive endpoints; upper `-1` disables only upper bound. Base-star metadata retained; no invented modded calculations |
| Backend accepts nullable star ranges the bot cannot deserialize | Invalid/null ranges rejected with validation errors or import issues |
| Exclusion case behavior differs between writers/readers | Normalize lowercase, case-insensitive uniqueness and comparison |
| Disabling a user may leave request acceptance until the next membership poll | Current enabled/settings state checked before delivery, in addition to membership eligibility |
| Set lookup uses arbitrary first cached map, differing from API order | First map from the set API determines selection; a persisted 24-hour selected-map mapping avoids repeated set requests |
| Process `testing` can still send star-range rejection messages | Process dry-run suppresses all outgoing chat and success statistics, including rejection messages |

User `test` mode and process dry-run remain distinct. User test mode bypasses request filters but still delivers; process dry-run never delivers. Existing mods remain forwarded display text; conflicting combinations are not silently changed, and BPM/stars/duration remain base values. The API's historical upper-range value of 10 or more still means unlimited.

## Website/API and security

| Python behavior / defect | Rust behavior |
|---|---|
| Weekly leaderboard subtracts 30 days (`routers/requests.py:24`) | Rolling seven days; daily/monthly remain rolling 1/30 days, now explicitly UTC |
| Leaderboard lookup can return nested BSON ObjectIds and duplicated raw `result` | JSON-safe top-level metadata/count/beatmap ID; no invalid nested BSON result; deterministic tie ordering |
| Unbounded/negative pagination | Limit 1–100, offset nonnegative, deliberate validation response |
| Linking builds a fresh user and overwrites settings/exclusions/live state | Transactional identity update preserving unrelated user state; reject conflicting provider ownership |
| Registration serializes `sub_only`/`points_only` inconsistently with aliases | One canonical hyphenated wire representation; one-time importer accepts historical aliases and rejects conflicting aliases |
| Missing/deleted user or malformed JWT can cause internal exceptions | Explicit 401 authentication failures; unauthenticated `/user/me` remains null |
| Full account JWTs expire after a year and cannot be revoked on logout | Hash-stored opaque sessions, 30-day configurable lifetime, revocation and rotation; old JWT cookies invalid at cutover |
| OAuth `state` is a return URL rather than a CSRF nonce | Random, expiring, single-use, browser-bound state; provider/signup/session context checked |
| Arbitrary `state`/Referer redirect; interpolated authorization query | Allowlisted return origins and URL-encoded authorization parameters |
| Cookies lack explicit Secure/HttpOnly policy | HttpOnly, SameSite=Lax, configurable Secure mandatory with HTTPS |
| Cookie-authenticated mutations lack an explicit CSRF policy | Trusted exact Origin/Referer validation, exact-origin credentialed CORS, including logout/deletion |
| Repeated same-provider partial signup can fail unpredictably | Explicit provider/context checks and conflicts; both legitimate signup orders retained |

Provider IDs are unique; cached login names deliberately are not. A recycled Twitch login must not block a different verified account or make the bot deliver to the stale owner. EventSub broadcaster/chatter IDs identify identity and destination; a verified current grant is required for channel subscriptions.

## Storage, migration, and resource use

- Listener configuration now uses separate `BIND_IP` (an IPv4/IPv6 literal) and `BIND_PORT` values rather than a combined `BIND` address. Compose uses `BIND_PORT` on both sides of its loopback-only published mapping and binds the container listener to `0.0.0.0`.

- MongoDB is replaced by embedded SQLite. Schema/default initialization no longer depends on starting the bot first. Bot/API use the same typed settings and database actor.
- Twitch chat permission is explicit, per-channel consent, not inferred from registration or old live state. Imported accounts keep all data but remain inactive until reauthorization. The public `twitchBotAuthorized` field exposes this.
- The bot's access/refresh credentials are stored only as AES-256-GCM ciphertext with an external key; AAD binds the format and bot user ID. Rotation uses compare-and-swap revisions, and temporary validation failures do not destroy usable credentials or accounts.
- Live and category state use EventSub with per-family event timestamps and a revision guard against stale snapshot overwrites. Version-1 databases upgrade transactionally, reset unverified live flags, and normalize legacy exclusion logins.
- Unique provider IDs, foreign keys, transactions, time-window/history indexes, and cache primary keys replace loose Mongo documents/nonunique indexes. Raw source records and unknown fields remain stored for migration provenance.
- The importer streams all four export collections through SQLite staging, validates before commit, and never silently discards conflicting/invalid accounts. Malformed exports abort the entire cutover. BSON, JSON arrays, NDJSON, Extended JSON dates/numbers, and historical setting aliases are supported.
- The historical SQLite→Mongo script is replaced, not copied. Its destructive missing-avatar deletion, positional-column assumptions, malformed exclusion upserts, unbounded bulk lists, and per-avatar token creation are not retained.
- Historical beatmap backfill is keyset-paginated instead of capped at 100,000; uses shared credentials/pools and stores freshness timestamps. It reports failures with a nonzero outcome.
- Expired beatmap request-cache entries stay on disk for historical display but are not returned as fresh request metadata. This replaces Mongo TTL deletion while preserving useful leaderboard metadata. Request history remains durable with no silent retention cutoff.
- One shared admission budget covers queued and running requests. A bounded worker scheduler runs independent channels concurrently while keeping each channel FIFO and cooldown evaluation/commit serial. HTTP connections and provider pacing remain shared.
- Common parser operations avoid per-mod/path/query vector allocation. Tokio uses available parallelism (or `TOKIO_WORKER_THREADS`); request concurrency remains separately bounded by `REQUEST_WORKERS`. One dedicated SQLite worker retains sole connection ownership and the existing WAL/FULL durability settings.
- Independent EventSub shard supervisors recover without resetting healthy peers. One maintenance task owns subscription HTTP and revision-guarded snapshots, leaving foreground notifications responsive during provider stalls. Automatic shard growth never shrinks, respects the configured cap, and retains the 10,000-owned-subscription audit safety bound.
- Consistent backup/integrity commands, a nonroot image, persistent volume, correct port/env-file configuration, and secret-excluding build context replace outdated deployment instructions.

These are concrete structural resource improvements, not a claim that every workload is faster. See `VERIFICATION.md` for actual memory measurements, workload scope, checks, and provider/production limits.

## Coexistence migration period

- All users use the new dashboard/API. The old Python bot remains only as an IRC relay for enabled channels that have never granted Twitch `channel:bot`.
- The new SQLite database is the sole user/settings source. A one-time Mongo import precedes rollout; there is no bidirectional database sync.
- The old bot patch lives independently in [PR #56 (`migration/coexistence-bridge`)](https://github.com/aticie/ronnia/pull/56); run it from a separate checkout, never this Rust root. It fetches legacy channels and current settings from authenticated internal endpoints and claims/reports each request. Its Mongo Users/Settings are bypassed in bridge mode; Mongo remains only for its beatmap cache. The Rust `/internal/legacy/*` API is retained unchanged.
- A claim creates a delivery tombstone understood by the new bot. The old bot cannot send after a channel migrates, and the new bot skips any legacy-claimed Twitch message.
- New grants become active for the new bot after a configurable grace period (`MIGRATION_GRANT_GRACE_SECS`, default ten seconds), while removal from the old bot is immediate. This prevents overlapping delivery during handoff.
- Grant activation wakes a targeted current-stream lookup even for an imported/legacy account that was already enabled at startup. This also happens when migration grace expires, so fresh live/osu! eligibility does not wait for the ten-minute repair snapshot. Unrelated settings/state notifications do not repeatedly poll already-known channels; targeted lookup failures and stale-snapshot races retain the existing retry/revision guards.
- Legacy request statistics are written by the old bot through the bridge with the same Twitch-message source identity as the new bot; repeated reports are idempotent.
- The old bot announces migration once per live session to both Twitch chat and the streamer's osu! PM, re-arming after offline. The new dashboard shows a banner driven by `/migration/status`.
- Internal bridge routes require a shared bearer token, are hidden when unconfigured, and are blocked through the dashboard proxy.
- At cutoff, stopping the old bot requires no second data merge.

## Separately deployed Vue dashboard

Frontend integration lives in [aticie/ronnia-web](https://github.com/aticie/ronnia-web), based on its upstream commit `ec0ff76ef8e3b06c873f5213c71a0901ef6514f9`; there is no `web/` source in this repository. Attribution and upstream licensing status are documented there.

- Backend Compose runs only the Rust service, published at `127.0.0.1:${BIND_PORT:-8080}`. Start it first to create `${RONNIA_NETWORK:-ronnia}`. The separate frontend Compose joins that external Docker network, runs static Nginx on `WEB_PORT` (default 3000), and defaults `API_UPSTREAM` to `http://ronnia:${BIND_PORT:-8080}`. Set matching `BIND_PORT` and `RONNIA_NETWORK` in both projects; frontend `API_UPSTREAM` can be overridden. No host-gateway mapping or public backend publication is required.
- Browser calls use `/api`; the frontend proxy forwards to the Rust service. `PUBLIC_ORIGIN=http://localhost:3000` and `API_ORIGIN=http://localhost:8080` are the local defaults; provider callbacks go directly to the backend origin. Credentials and the backend `.env` never enter the frontend image or bundle. Existing deployment configuration is not rewritten by this split.
- Removed compiled-in production API/auth URLs and the page's HTTP-upgrading CSP. Local origins and HTTP cookie settings are explicit, while existing direct backend OAuth callback registrations remain usable.
- Fixed anonymous `null` profile handling and unconditional navigation away from partial signup. Either provider can be first, and only the missing provider remains visible.
- Settings reload waits for account hydration. Session expiration produces a visible login message instead of exceptions or an automatic refetch loop.
- Added the current Twitch permission field and re-consent action. Logout/deletion clear frontend state only after the backend succeeds.
- Frontend live-list requests now obey the API's 100-item pagination limit. Exclusion failures preserve the existing list and successful mutations use the server's normalized login.
- Full settings writes are serialized and coalesce the latest pending snapshot, preventing a slower older request from overwriting newer changes. Errors preserve the form and allow an explicit retry.
- Updated compatible Vue/Vite/TypeScript tooling, removed unused cookie/integration dependencies, and patched vulnerable transitive build packages. The frontend production container runs Nginx only, not Node.
