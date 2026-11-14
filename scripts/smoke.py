#!/usr/bin/env python3
"""Exercise an already-built ronnia against local providers; standard library only.

Usage: scripts/smoke.py --binary target/debug/ronnia

The real executable is driven through its own CLI, HTTP API, SQLite database,
outbound Helix/OAuth requests and a real local EventSub WebSocket server. No
application source is imported, patched or text-inspected. Fixture accounts and
history are written directly into a freshly initialized SQLite database.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import http.cookiejar
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

from modern import BOT_SCOPES, EventSub, Providers, beatmap, check, timestamp


TOKEN_KEY = "11" * 32


def eventually(predicate, description, timeout=20.0):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            result = predicate()
            if result:
                return result
        except (OSError, sqlite3.Error, urllib.error.URLError) as exc:
            last = exc
        time.sleep(0.05)
    raise AssertionError(f"Timed out: {description}" + (f" ({last})" if last else ""))


def install_signal_handlers():
    def terminate(_number, _frame):
        raise KeyboardInterrupt("termination requested")
    signal.signal(signal.SIGTERM, terminate)


def settle_chat_feedback(providers, eventsub, timeout=12.0):
    """Wait for the paced feedback queue to drain before exact counts."""
    deadline = time.monotonic() + timeout
    previous = None
    stable_at = time.monotonic()
    while time.monotonic() < deadline:
        current = (providers.chat_send_attempts, len(eventsub.echoes()))
        if current != previous:
            previous, stable_at = current, time.monotonic()
        elif time.monotonic() - stable_at >= 2.2:
            return
        time.sleep(0.1)
    raise AssertionError("Chat feedback queue did not settle")


def settle_stream_lookups(providers, timeout=15.0):
    """Wait for startup stream lookups to stop before taking an exact baseline.

    The bot coalesces targeted category lookups behind a short debounce, so a
    snapshot triggered during startup can land after the baseline is captured and
    make the grant-driven count look like two. Require the counter to be stable
    for longer than that debounce first.
    """
    deadline = time.monotonic() + timeout
    previous = None
    stable_at = time.monotonic()
    while time.monotonic() < deadline:
        current = providers.count_path("/twitch/helix/streams")
        if current != previous:
            previous, stable_at = current, time.monotonic()
        elif time.monotonic() - stable_at >= 1.5:
            return
        time.sleep(0.1)
    raise AssertionError("Stream lookups did not settle before the baseline")


def run_captured(command, env=None, timeout=15):
    process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, text=True, start_new_session=True)
    try:
        stdout, _ = process.communicate(timeout=timeout)
    except BaseException:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        process.communicate(timeout=3)
        raise
    return subprocess.CompletedProcess(command, process.returncode, stdout)


def cli_json(output):
    """Extract a terminal CLI JSON value after optional structured stderr logs."""
    decoder = json.JSONDecoder()
    for index, character in enumerate(output):
        if character not in "[{":
            continue
        try:
            value, consumed = decoder.raw_decode(output[index:])
        except ValueError:
            continue
        if not output[index + consumed:].strip():
            return value
    raise AssertionError(f"No terminal CLI JSON value:\n{output}")


@contextlib.contextmanager
def connect(path, *, readonly=False):
    """A SQLite connection that commits and closes deterministically.

    `with sqlite3.connect(...)` commits but never closes, and a leaked connection
    keeps this process attached to an old WAL index after the service restarts, so
    later reads can miss rows the service has already committed.
    """
    target = f"file:{path}?mode=ro" if readonly else str(path)
    connection = sqlite3.connect(target, uri=readonly, timeout=5)
    try:
        with connection:
            yield connection
    finally:
        connection.close()


def database_rows(path):
    with connect(path, readonly=True) as conn:
        return tuple(conn.iterdump())


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def settings(**overrides):
    result = {"echo": True, "enable": True, "sub-only": False,
              "points-only": False, "test": False, "cooldown": 0, "sr": [0, -1]}
    result.update(overrides)
    return result


def insert_history(connection, map_id, age_days=0, mods=None):
    """Insert one historical request row for the fixture streamer."""
    connection.execute(
        "INSERT INTO statistics(requester_channel_name,requested_beatmap_id,requested_channel_name,"
        "mods,timestamp,raw_json) VALUES('fixtureviewer',?,'streamer',?,?,?)",
        (map_id, mods, int(time.time()) - age_days * 86400,
         json.dumps({"requested_beatmap_id": map_id})))


def seed_catalog(database):
    """Insert the historical request/beatmap fixture used by leaderboard and backfill checks."""
    with connect(database) as connection:
        for map_id, age, mods in ((9001, 0, "+HD"), (9002, 8, None), (9003, 40, None)):
            insert_history(connection, map_id, age, mods)
        for map_id in (9001, 9002, 9003):
            connection.execute(
                "INSERT INTO beatmaps(id,beatmapset_id,fetched_at,raw_json) VALUES(?,500,?,?)",
                (map_id, int(time.time()), json.dumps(beatmap(map_id))))


def seed_fixture(service):
    """Populate a fresh SQLite database with the account, settings, and history fixture."""
    result = run_captured(service.command("check"), env=service.env)
    check(result.returncode == 0, f"Fresh SQLite check failed:\n{result.stdout}")
    with connect(service.database) as connection:
        connection.execute(
            "INSERT INTO users(osu_id,osu_username,osu_avatar_url,twitch_id,twitch_username,"
            "twitch_avatar_url,is_live,settings_json,raw_json) VALUES(?,?,?,?,?,?,?,?,?)",
            (201, "osu201", "https://example.invalid/osu201.png", 101, "streamer",
             "https://example.invalid/twitch101.png", 0, json.dumps(settings()),
             json.dumps({"fixture_extra": "preserve-profile"})))
        connection.execute("INSERT INTO exclusions(twitch_id,login) VALUES(101,'importedblock')")
        for name, value in settings().items():
            document = {"name": name, "value": value,
                        "description": "synthetic fixture metadata " + name,
                        "type": "range" if name == "sr" else "value" if name == "cooldown" else "toggle"}
            connection.execute(
                "UPDATE settings_metadata SET value_json=?,description=?,type=?,raw_json=? WHERE name=?",
                (json.dumps(value), document["description"], document["type"],
                 json.dumps(document), name))
    seed_catalog(service.database)
    check(service.scalar("SELECT count(*) FROM users") == 1, "Fixture user missing")
    check(service.scalar("SELECT count(*) FROM statistics") == 3, "Fixture history missing")
    check(service.scalar("SELECT count(*) FROM beatmaps") == 3, "Fixture beatmaps missing")
    check(service.scalar("SELECT count(*) FROM twitch_channel_grants") == 0,
          "Fixture fabricated a Twitch channel grant")
    check(service.scalar("SELECT is_live FROM users WHERE twitch_id=101") == 0,
          "Fixture live flag must not imply bot eligibility")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


class Browser:
    def __init__(self, origin):
        self.origin = origin
        self.cookies = http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}),
                                                  urllib.request.HTTPCookieProcessor(self.cookies), NoRedirect())
        self.routes = set()

    def request(self, path, method="GET", data=None, trusted=True):
        headers = {"Origin": self.origin, "Referer": self.origin + "/dashboard"} if trusted else {}
        raw = None
        if data is not None:
            raw = json.dumps(data).encode()
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(self.origin + path, data=raw, method=method, headers=headers)
        self.routes.add((method, urllib.parse.urlsplit(path).path))
        try:
            response = self.opener.open(req, timeout=2)
        except urllib.error.HTTPError as exc:
            response = exc
        with response:
            body = response.read()
            try:
                value = json.loads(body) if body else None
            except ValueError:
                value = body.decode(errors="replace")
            return response.status, response.headers, value

    def ok(self, path, method="GET", data=None):
        status, headers, value = self.request(path, method, data)
        check(status == 200, f"{method} {path}: expected 200, got {status}: {value}")
        return value

    def oauth_status(self, provider, identity):
        status, headers, _ = self.request(f"/oauth2/{provider}-login")
        check(status in (302, 303, 307), f"OAuth start failed: {status}")
        location = urllib.parse.urlsplit(headers["Location"])
        check(location.hostname == "127.0.0.1", "OAuth start escaped fake-provider origin")
        state = urllib.parse.parse_qs(location.query)["state"][0]
        check(state and not state.startswith("http"), "OAuth state is not an opaque nonce")
        scopes = dict(urllib.parse.parse_qsl(location.query)).get("scope", "")
        if provider == "twitch":
            check("channel:bot" in scopes.split(), f"Twitch signup lost channel:bot scope: {scopes}")
            check(dict(urllib.parse.parse_qsl(location.query)).get("force_verify") == "true",
                  "Twitch reconsent must force verification")
        path = f"/oauth2/{provider}-redirect?" + urllib.parse.urlencode({"code": identity, "state": state})
        status, headers, value = self.request(path)
        return status, headers, value, path

    def oauth(self, provider, identity):
        status, headers, value, path = self.oauth_status(provider, identity)
        check(status in (302, 303, 307), f"OAuth callback failed: {status} {value}")
        check(headers["Location"].startswith(self.origin), "OAuth callback redirect escaped trusted origin")
        for cookie in headers.get_all("Set-Cookie", []):
            if "Max-Age=0" not in cookie and "1970" not in cookie:
                check("httponly" in cookie.lower() and "samesite=lax" in cookie.lower(),
                      f"Insecure session cookie: {cookie}")
        return path


class Service:
    def __init__(self, binary, root, providers, eventsub):
        self.binary = str(Path(binary).absolute())
        check(Path(self.binary).is_file(), f"Build the service first: missing {self.binary}")
        self.root = Path(root)
        self.database = self.root / "isolated.sqlite3"
        port = free_port()
        self.origin = f"http://127.0.0.1:{port}"
        self.providers = providers
        self.eventsub = eventsub
        self.process = None
        self.logs = []
        self.env = {name: os.environ[name] for name in ("PATH", "HOME", "SYSTEMROOT", "TMPDIR") if name in os.environ}
        self.env.update({"BIND_IP": "127.0.0.1", "BIND_PORT": str(port), "API_ORIGIN": self.origin,
                         "PUBLIC_ORIGIN": self.origin, "COOKIE_SECURE": "false", "RUST_LOG": "info",
                         "TWITCH_CLIENT_ID": "synthetic-twitch", "TWITCH_CLIENT_SECRET": "synthetic-secret",
                         "OSU_CLIENT_ID": "123", "OSU_CLIENT_SECRET": "synthetic-secret",
                         "TWITCH_TOKEN_KEY": TOKEN_KEY,
                         "TWITCH_EVENTSUB_URL": eventsub.url,
                         "TWITCH_API_BASE": providers.origin + "/twitch/helix",
                         "TWITCH_OAUTH_BASE": providers.origin + "/twitch/oauth",
                         "OSU_API_BASE": providers.origin + "/osu/api/v2",
                         "OSU_OAUTH_BASE": providers.origin + "/osu/oauth",
                         "RECONCILE_INTERVAL_SECS": "600", "HTTP_TIMEOUT_SECS": "2", "DRY_RUN": "false",
                         "EVENTSUB_CHANNELS_PER_SHARD": "100", "EVENTSUB_MAX_SHARDS": "1",
                         "MIGRATION_GRANT_GRACE_SECS": "0", "MIGRATION_ACTIVE": "false"})

    def command(self, *args):
        return [self.binary, "--database", str(self.database), *args]

    def configure_dns_failure(self):
        if not sys.platform.startswith("linux"):
            return False
        source = Path(__file__).with_name("fixtures") / "resolver.c"
        library = self.root / "resolver.so"
        result = run_captured(["cc", "-shared", "-fPIC", "-std=c11", "-O2",
                               str(source), "-o", str(library), "-ldl"])
        check(result.returncode == 0, f"Cannot compile process-local DNS fault fixture:\n{result.stdout}")
        url = f"ws://recover.localhost:{self.eventsub.port}/ws"
        self.eventsub.url = url
        self.env.update({"LD_PRELOAD": str(library), "TWITCH_EVENTSUB_URL": url})
        return True

    def scalar(self, sql, params=()):
        with connect(self.database, readonly=True) as conn:
            row = conn.execute(sql, params).fetchone()
            return row[0] if row else None

    def start(self, dry_run=False, api_only=False, debug=False):
        check(self.process is None, "Service already running")
        log_path = self.root / f"service-{len(self.logs)}.log"
        self.logs.append(log_path)
        self.log_file = log_path.open("wb")
        env = dict(self.env, DRY_RUN=str(dry_run).lower(), DEBUG_MODE=str(debug).lower())
        self.process = subprocess.Popen(self.command("serve", *(["--api-only"] if api_only else [])),
                                        env=env, stdout=self.log_file, stderr=subprocess.STDOUT,
                                        start_new_session=True)
        probe = Browser(self.origin)
        def ready():
            check(self.process.poll() is None, f"Service exited ({self.process.returncode}); see log")
            return probe.request("/health")[0] == 200 and probe.request("/ready")[0] == 200
        eventually(ready, "service HTTP readiness")

    def stop(self, close_fixture=True):
        if self.process is None:
            return
        process, self.process = self.process, None
        if process.poll() is None:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=3)
        self.log_file.close()
        if close_fixture:
            self.eventsub.drop()
        return process.returncode

    def print_logs(self):
        for path in self.logs:
            print(f"\n--- {path.name} ---", file=sys.stderr)
            print(path.read_text(errors="replace")[-24000:], file=sys.stderr)


EXPECTED_ROUTES = {("GET", "/oauth2/osu-login"), ("GET", "/oauth2/osu-redirect"),
                   ("GET", "/oauth2/twitch-login"), ("GET", "/oauth2/twitch-redirect"),
                   ("GET", "/live/users"), ("GET", "/requests/beatmaps/top/daily"),
                   ("GET", "/requests/beatmaps/top/weekly"), ("GET", "/requests/beatmaps/top/monthly"),
                   ("GET", "/user/me"), ("GET", "/user/logout"), ("DELETE", "/user/me"),
                   ("GET", "/user/settings"), ("POST", "/user/settings"),
                   ("GET", "/user/exclude"), ("POST", "/user/exclude"), ("DELETE", "/user/exclude"),
                   ("GET", "/health"), ("GET", "/ready")}


def activate_device(providers):
    request = urllib.request.Request(providers.origin + "/device/activate",
                                     data=json.dumps({"user_code": "SMOKE-CODE"}).encode(),
                                     method="POST", headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=5) as response:
        check(response.status == 200, "Device browser activation failed")


def authorize_bot(service, login="smokebot", replace=False, activate=True, expected=0,
                  expected_user_id="999", timeout=25):
    """Run the real authorize-bot CLI against the local OAuth device provider."""
    command = service.command("authorize-bot", "--login", login, *(["--replace"] if replace else []))
    env = {key: value for key, value in service.env.items() if key != "LD_PRELOAD"}
    process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, start_new_session=True)
    lines = []

    def reader():
        for line in process.stdout:
            lines.append(line)
    thread = threading.Thread(target=reader, daemon=True)
    thread.start()
    try:
        if activate:
            deadline = time.monotonic() + 10
            prompted = False
            while time.monotonic() < deadline:
                prompted = any("SMOKE-CODE" in line for line in lines)
                if prompted or process.poll() is not None:
                    break
                time.sleep(0.05)
            if prompted:
                check(any(service.providers.origin in line for line in lines),
                      f"Device prompt lacks verification URI: {''.join(lines)}")
                # Let the real CLI observe at least one authorization_pending poll.
                time.sleep(1.4)
                activate_device(service.providers)
            elif expected == 0:
                raise AssertionError(f"authorize-bot never printed a device prompt:\n{''.join(lines)}")
        stdout, stderr = process.communicate(timeout=timeout)
    except BaseException:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        process.communicate(timeout=3)
        raise
    finally:
        thread.join(1)
    output = "".join(lines) + (stdout or "")
    check(process.returncode == expected,
          f"authorize-bot exit {process.returncode}, expected {expected}:\n{output}\n{stderr}")
    if expected == 0:
        identity = cli_json(output)
        check(set(identity) == {"user_id", "login"}, f"BotIdentity shape changed: {identity}")
        check(identity["user_id"] == expected_user_id and identity["login"] == login.lower(),
              f"BotIdentity did not match validated account: {identity}")
        check("private-device-code" not in output and "bot-access" not in output
              and "bot-refresh" not in output, "CLI printed a device code or bot credential")
    return output


def auth_row(service):
    with connect(service.database, readonly=True) as conn:
        row = conn.execute("SELECT user_id,login,ciphertext,expires_at,scopes_json,validated_at,revision "
                           "FROM twitch_bot_auth WHERE singleton=1").fetchone()
        return row


def assert_encrypted_grant(service):
    def validated(row):
        return row is not None and row[5] > 0
    # A refresh clears the stamp until the follow-up validation commits.
    eventually(lambda: validated(auth_row(service)), "stored bot grant validation stamp")
    row = auth_row(service)
    check(row is not None, "authorize-bot did not persist a bot grant")
    user_id, login, ciphertext, expires_at, scopes_json, validated_at, revision = row
    check(user_id == "999" and login == "smokebot", f"Stored bot identity wrong: {row[:2]}")
    check(expires_at > int(time.time()) and revision >= 1 and validated_at > 0, f"Grant metadata invalid: {row}")
    check(isinstance(ciphertext, bytes) and len(ciphertext) > 12, "Grant ciphertext missing nonce prefix")
    check(b"bot-access" not in ciphertext and b"bot-refresh" not in ciphertext,
          "Access/refresh token stored in plaintext")
    check(set(json.loads(scopes_json)) == set(BOT_SCOPES), f"Stored scopes wrong: {scopes_json}")
    raw = Path(service.database).read_bytes()
    check(b"bot-access" not in raw and b"bot-refresh" not in raw,
          "Plaintext bot credential present in SQLite file")


def exercise_api(service):
    browser = Browser(service.origin)
    check(browser.ok("/health") == {"status": "ok"}, "Liveness route wrong")
    check(browser.ok("/ready") == {"status": "ready"}, "Readiness route wrong")
    check(browser.request("/openapi.json")[0] == 404, "Debug docs exposed without DEBUG_MODE")
    check(browser.ok("/user/me") is None, "Anonymous /user/me must be JSON null")
    check(browser.request("/user/settings")[0] == 401, "Missing session must return 401")
    settle_stream_lookups(service.providers)
    before_streams = service.providers.count_path("/twitch/helix/streams")
    browser.oauth("twitch", 101)
    user = browser.ok("/user/me")
    check(user["osuId"] == 201 and user["twitchId"] == 101, "Existing fixture account login failed")
    check(set(user) == {"osuId", "osuUsername", "osuAvatarUrl", "twitchId", "twitchUsername",
                        "twitchAvatarUrl", "excludedUsers", "isLive", "twitchBotAuthorized"},
          f"Public UserResponse fields changed: {sorted(user)}")
    check(user["twitchBotAuthorized"] is True, "Verified channel:bot consent not exposed")
    check(service.scalar("SELECT status FROM twitch_channel_grants WHERE twitch_id=101") == "granted",
          "Twitch login did not persist verified channel:bot grant")
    metadata = browser.ok("/user/settings")
    check({entry["name"] for entry in metadata} == set(settings()), "Settings metadata missing")
    check(next(row for row in metadata if row["name"] == "echo")["description"] ==
          "synthetic fixture metadata echo", "API lost fixture metadata")
    check(browser.ok("/user/exclude") == ["importedblock"], "API lost fixture exclusion")
    browser.ok("/user/exclude?excluded_user=MiXeDBlock", "POST")
    check("mixedblock" in browser.ok("/user/exclude"), "Exclusion add not normalized")
    browser.ok("/user/exclude?excluded_user=MIXEDBLOCK", "DELETE")
    check("mixedblock" not in browser.ok("/user/exclude"), "Case-insensitive exclusion removal failed")
    check(browser.request("/user/settings", "POST", settings(), trusted=False)[0] == 403,
          "Cookie mutation without trusted Origin/Referer accepted")
    check(browser.ok("/user/settings", "POST", settings()) is None, "Settings success must be JSON null")
    check(browser.request("/user/settings", "POST", settings(cooldown=-1))[0] in (400, 422),
          "Invalid negative cooldown accepted")
    for route in ("daily", "weekly", "monthly"):
        rows = browser.ok(f"/requests/beatmaps/top/{route}?limit=100&offset=0")
        counts = {row["_id"]: row["count"] for row in rows}
        expected = {9001: 1, 9002: 1} if route == "monthly" else {9001: 1}
        check(counts == expected, f"{route} UTC window wrong (weekly must be 7d): {counts}")
        check(rows[0]["synthetic_marker"] == "generated-not-user-data", "Leaderboard lost map metadata")
        check("result" not in rows[0], "Legacy lookup internals leaked into API")
    check(browser.request("/live/users?limit=0")[0] in (400, 422), "Invalid pagination accepted")
    eventually(lambda: browser.ok("/live/users?limit=100&offset=0") == ["streamer"],
               "Existing fixture account becomes eligible immediately on grant activation", timeout=15)
    check(browser.ok("/user/me")["isLive"] is True, "Verified grant+osu live state not reflected")
    # The grant-driven lookup is coalesced behind a debounce, so it lands after the
    # eligibility check above returns; wait for it before demanding an exact count.
    eventually(lambda: service.providers.count_path("/twitch/helix/streams") > before_streams,
               "Grant activation triggers a stream lookup", timeout=10)
    observed_streams = service.providers.count_path("/twitch/helix/streams")
    check(observed_streams == before_streams + 1,
          f"Existing enabled fixture account got {observed_streams - before_streams} grant-driven stream "
          f"lookups, want exactly 1 (baseline {before_streams}, observed {observed_streams})")

    # Reauthorizing the same identity is allowed and rotates the browser session.
    old_token = next(cookie.value for cookie in browser.cookies if cookie.name == "token")
    old_hash = hashlib.sha256(old_token.encode()).hexdigest()
    check(service.scalar("SELECT count(*) FROM sessions WHERE token_hash=?", (old_hash,)) == 1,
          "Reauthorization precondition: old session hash missing")
    browser.oauth("twitch", 101)
    new_token = next(cookie.value for cookie in browser.cookies if cookie.name == "token")
    check(old_token != new_token, "Reauthorization did not rotate the browser session")
    check(service.scalar("SELECT count(*) FROM sessions WHERE token_hash=?", (old_hash,)) == 0,
          "Rotated session token remained valid")
    stale = Browser(service.origin)
    stale.cookies.set_cookie(http.cookiejar.Cookie(0, "token", old_token, None, False, "127.0.0.1", False,
                                                   False, "/", True, False, None, True, None, None, {}))
    check(stale.request("/user/settings")[0] == 401, "Rotated browser cookie still authorized")
    check(browser.ok("/user/me")["twitchId"] == 101, "Replacement browser session is not authorized")

    # A Twitch token without verified channel:bot consent must never create a grant.
    service.providers.missing_user_scope = True
    outsider = Browser(service.origin)
    try:
        status, _headers, _value, _path = outsider.oauth_status("twitch", 106)
    finally:
        service.providers.missing_user_scope = False
    check(status >= 400, f"Unconsented Twitch login accepted: {status}")
    check(service.scalar("SELECT count(*) FROM users WHERE twitch_id=106") == 0,
          "Unconsented Twitch login created an account")
    check(service.scalar("SELECT count(*) FROM twitch_channel_grants WHERE twitch_id=106") == 0,
          "Unconsented Twitch login created a channel grant")
    browser.routes.update(outsider.routes)

    for first, twitch_id, osu_id in [("osu", 102, 202), ("twitch", 103, 203)]:
        signup = Browser(service.origin)
        replay = signup.oauth(first, osu_id if first == "osu" else twitch_id)
        check(signup.ok("/user/me") == {"signup": first}, f"{first}-first signup marker missing")
        check(service.scalar("SELECT count(*) FROM twitch_channel_grants") == 1,
              f"{first}-only partial signup fabricated a channel grant")
        check(signup.request(replay)[0] == 400, "Single-use OAuth state replay accepted")
        signup.oauth("twitch" if first == "osu" else "osu", twitch_id if first == "osu" else osu_id)
        linked = signup.ok("/user/me")
        check((linked["twitchId"], linked["osuId"]) == (twitch_id, osu_id), "Signup linked wrong identities")
        check(linked["twitchBotAuthorized"] is True, f"{first}-first signup did not persist grant consent")
        check(service.scalar("SELECT status FROM twitch_channel_grants WHERE twitch_id=?",
                             (twitch_id,)) == "granted", "Signup grant not committed atomically")
        token = next(cookie.value for cookie in signup.cookies if cookie.name == "token")
        check(service.scalar("SELECT count(*) FROM sessions WHERE token_hash=?", (token,)) == 0,
              "Session token stored in plaintext")
        if first == "osu":
            check(signup.request("/user/me", "DELETE")[0] in (302, 303, 307), "Account deletion failed")
            check(service.scalar("SELECT count(*) FROM users WHERE twitch_id=102") == 0, "Account not deleted")
            check(service.scalar("SELECT count(*) FROM twitch_channel_grants WHERE twitch_id=102") == 0,
                  "Account deletion left a grant row")
            signup.cookies.set_cookie(http.cookiejar.Cookie(0, "token", token, None, False, "127.0.0.1", False,
                                                            False, "/", True, False, None, True, None, None, {}))
            check(signup.request("/user/settings")[0] == 401, "Deleted account session still authorized")
        else:
            check(signup.request("/user/logout")[0] in (302, 303, 307), "Logout failed")
            check(signup.ok("/user/me") is None, "Logout did not clear session")
        browser.routes.update(signup.routes)

    # A signed-in identity cannot be replaced by authorizing a different Twitch account.
    status, _headers, _value, _path = browser.oauth_status("twitch", 104)
    check(status in (400, 409), f"Different Twitch identity takeover accepted: {status}")
    check(service.scalar("SELECT twitch_id FROM users WHERE osu_id=201") == 101, "Ownership was replaced")
    browser.oauth("twitch", 101)
    check(browser.ok("/user/me")["twitchId"] == 101, "Original account session was not recoverable")
    check(EXPECTED_ROUTES <= browser.routes, f"Unexercised routes: {EXPECTED_ROUTES - browser.routes}")
    return browser


def wait_owned_subscriptions(service, description, uid=101, timeout=25):
    """Wait for the complete asynchronous owned subscription set and a socket."""
    def complete():
        conduit = service.scalar("SELECT conduit_id FROM twitch_conduit WHERE singleton=1")
        rows = [service.providers.subscription(kind, uid) for kind in
                ("channel.chat.message", "channel.update", "stream.offline", "stream.online")]
        return (conduit is not None and service.eventsub.attached()
                and all(row is not None and row["status"] == "enabled"
                        and row["transport"].get("method") == "conduit"
                        and row["transport"].get("conduit_id") == conduit for row in rows))
    eventually(complete, description, timeout=timeout)


def wait_offline_startup(service, before_streams, before_revision, description):
    """Require provider traffic and its durable offline correction, not a log."""
    eventually(
        lambda: service.providers.count_path("/twitch/helix/streams") > before_streams
        and service.scalar(
            "SELECT count(*) FROM channel_stream_state "
            "WHERE twitch_id=101 AND revision>? AND online=0 AND category_id=''",
            (before_revision,),
        ) == 1
        and service.eventsub.attached(),
        description,
        timeout=25,
    )


def exercise_grant_grace(service, browser):
    """An already-enabled account refreshes state at activation, not the 600s audit."""
    providers = service.providers
    service.stop()
    service.env["MIGRATION_GRANT_GRACE_SECS"] = "2"
    # Reuse the fixture account with no consent at startup, as during cutover.
    with connect(service.database) as conn:
        conn.execute("DELETE FROM twitch_channel_grants WHERE twitch_id=101")
    providers.live = {}
    startup_streams = providers.count_path("/twitch/helix/streams")
    startup_revision = service.scalar("SELECT revision FROM channel_stream_state WHERE twitch_id=101") or 0
    service.start()
    wait_offline_startup(service, startup_streams, startup_revision,
                         "grace regression provider startup snapshot applied")
    check(browser.ok("/live/users?limit=100") == [], "Offline startup snapshot was not applied")
    before_streams = providers.count_path("/twitch/helix/streams")
    providers.live = {101: "streamer"}
    browser.oauth("twitch", 101)
    check(service.scalar("SELECT active_at>unixepoch() FROM twitch_channel_grants WHERE twitch_id=101"),
          "Grace regression did not create a future activation")
    check(browser.ok("/user/me")["twitchBotAuthorized"] is True,
          "Grace must not hide verified public consent")
    check(browser.ok("/live/users?limit=100") == [], "Grant became eligible before grace expired")
    browser.ok("/user/settings", "POST", settings(echo=False))
    check(providers.count_path("/twitch/helix/streams") == before_streams,
          "Unrelated settings notification during grace triggered a stream lookup")
    eventually(lambda: browser.ok("/live/users?limit=100") == ["streamer"],
               "Grace expiry refreshes existing enabled account without periodic reconciliation", timeout=15)
    check(providers.count_path("/twitch/helix/streams") == before_streams + 1,
          "Grace expiry did not trigger exactly one stream lookup")
    browser.ok("/user/settings", "POST", settings())
    browser.ok("/user/settings", "POST", settings())
    time.sleep(0.5)
    check(providers.count_path("/twitch/helix/streams") == before_streams + 1,
          "Unrelated settings notifications repeatedly polled stream state")
    service.stop()
    service.env["MIGRATION_GRANT_GRACE_SECS"] = "0"
    service.start()
    wait_owned_subscriptions(service, "complete owned subscription set after grace regression", timeout=25)


def exercise_bot(service, browser):
    eventsub, providers = service.eventsub, service.providers
    wait_owned_subscriptions(service, "complete owned subscriptions for granted live channel", timeout=45)
    owned = set(providers.conduits) - {"foreign-conduit"}
    check(len(owned) == 1 and "foreign-conduit" in providers.conduits,
          f"Bot did not create exactly one owned conduit beside the unrelated one: {providers.conduits}")
    check("foreign-sub" in providers.subscriptions, "Bot deleted an unrelated remote subscription")
    check(eventsub.text_frames == 0, f"Client sent unexpected WebSocket text frames: {eventsub.text_frames}")
    check(providers.tokens["twitch:public"] >= 2, "Helix 401 did not refresh app token")
    eventually(lambda: browser.ok("/live/users?limit=100") == ["streamer"], "Hybrid live snapshot exposed via API")
    check(providers.refreshes >= 1, "Short-lived bot token was not refreshed before use")
    log = service.logs[-1].read_text(errors="replace")
    check("bot-access" not in log and "bot-refresh" not in log,
          "Service log leaked a bot access or refresh credential")

    sequence = 0
    def deliver(text, author="viewer", **tags):
        nonlocal sequence
        sequence += 1
        message_id = f"smoke-{sequence}"
        before_posts = len(providers.posts)
        before_stats = service.scalar("SELECT count(*) FROM statistics")
        before_echoes = len(eventsub.echoes())
        eventsub.inject(message_id, text, author=author, **tags)
        eventually(lambda: len(providers.posts) == before_posts + 1, f"osu PM for {message_id}")
        eventually(lambda: service.scalar("SELECT count(*) FROM statistics") == before_stats + 1,
                   f"success history for {message_id}")
        eventually(lambda: any("Request sent!" in echo for echo in eventsub.echoes()[before_echoes:]),
                   f"success echo for {message_id}")
        echoes = [echo for echo in eventsub.echoes()[before_echoes:] if "Request sent!" in echo]
        check("Synthetic Artist - Generated Song [Synthetic Insane]" in echoes[-1],
              f"Echo map formatting wrong: {echoes[-1]}")
        post = providers.posts[-1]
        check(author in post["message"] and "180 BPM" in post["message"] and "5.25*" in post["message"]
              and "2:05" in post["message"], f"osu PM formatting wrong: {post}")
        return message_id, post

    def settle_feedback():
        settle_chat_feedback(providers, eventsub)

    def reject(text, author="blocked", **tags):
        nonlocal sequence
        sequence += 1
        before = len(providers.posts)
        stats = service.scalar("SELECT count(*) FROM statistics")
        eventsub.inject(f"smoke-reject-{sequence}", text, author=author, **tags)
        time.sleep(0.45)
        check(len(providers.posts) == before, f"Rejected request sent a PM: {author}")
        check(service.scalar("SELECT count(*) FROM statistics") == stats, "Rejected request wrote success history")

    _, post = deliver("https://osu.ppy.sh/beatmapsets/500#osu/1001 +HD DT", subscriber=True, points=True)
    check(providers.chat_auth.startswith("bot-access"),
          f"Chat feedback used the wrong token: {providers.chat_auth[:16]}")
    check("[SUB]" in post["message"] and "+HDDT" in post["message"] and "USED POINTS" in post["message"],
          "Mods/subscriber/reward details lost")
    check(providers.tokens["osu:public"] >= 2, "osu metadata 401 did not refresh public token")
    duplicate_id, post = deliver("https://old.ppy.sh/p/beatmap?b=1002", author="legacyviewer")
    check("/b/1002 " in post["message"], "Legacy map URL resolved wrong ID")
    before = len(providers.posts)
    eventsub.inject(duplicate_id, "https://old.ppy.sh/p/beatmap?b=1002", author="legacyviewer")
    time.sleep(0.4)
    check(len(providers.posts) == before, "Duplicate Twitch message ID delivered twice")
    _, post = deliver("https://old.ppy.sh/s/500", author="setviewer", moderator=True)
    check("/b/1003 " in post["message"] and "[MOD]" in post["message"], "Mapset must select API first map")

    browser.ok("/user/exclude?excluded_user=BlOcKeD", "POST")
    reject("https://osu.ppy.sh/beatmaps/1001", author="BLOCKED")
    deliver("https://osu.ppy.sh/beatmaps/1001", author="allowed_after_exclusion")
    browser.ok("/user/exclude?excluded_user=blocked", "DELETE")
    reject("https://osu.ppy.sh/beatmaps/1001", author="streamer", user_id=101)
    browser.ok("/user/settings", "POST", settings(**{"sub-only": True}))
    reject("https://osu.ppy.sh/beatmaps/1001", author="nonsubscriber", subscriber=False)
    deliver("https://osu.ppy.sh/beatmaps/1001", author="subscriber", subscriber=True)
    deliver("https://osu.ppy.sh/beatmaps/1001", author="vipviewer", vip=True)
    browser.ok("/user/settings", "POST", settings(**{"points-only": True}))
    reject("https://osu.ppy.sh/beatmaps/1001", author="noreward")
    deliver("https://osu.ppy.sh/beatmaps/1001", author="rewardviewer", points=True)
    browser.ok("/user/settings", "POST", settings(sr=[5.25, 5.25]))
    reject("https://osu.ppy.sh/beatmaps/1004", author="toohard")
    deliver("https://osu.ppy.sh/beatmaps/1001", author="inclusive_boundary")
    browser.ok("/user/settings", "POST", settings(cooldown=60))
    deliver("https://osu.ppy.sh/beatmaps/1001", author="cooldownviewer", user_id=401)
    reject("https://osu.ppy.sh/beatmaps/1001", author="cooldownviewer", user_id=401)
    deliver("https://osu.ppy.sh/beatmaps/1001", author="other_cooldownviewer", user_id=402)
    browser.ok("/user/settings", "POST", settings(test=True, sr=[9, 9], **{"sub-only": True, "points-only": True}))
    deliver("https://osu.ppy.sh/beatmaps/1001", author="streamer", user_id=101)
    browser.ok("/user/settings", "POST", settings())

    # The bot sees its own EventSub message; it must not answer itself.
    own_before = len(providers.posts)
    eventsub.inject("smoke-own-message", "https://osu.ppy.sh/beatmaps/1001", author="smokebot", user_id=999)
    time.sleep(0.4)
    check(len(providers.posts) == own_before, "Bot answered its own EventSub message")

    # HTTP 200 with is_sent=false is not a delivered Twitch chat acknowledgement.
    settle_feedback()
    chat_before = len(providers.chat_posts)
    echoes_before = len(eventsub.echoes())
    posts_before = len(providers.posts)
    stats_before = service.scalar("SELECT count(*) FROM statistics")
    providers.chat_drop = True
    eventsub.inject("smoke-held", "https://osu.ppy.sh/beatmaps/1001", author="heldviewer")
    eventually(lambda: len(providers.posts) == posts_before + 1, "request with held feedback still PMs")
    eventually(lambda: service.scalar("SELECT count(*) FROM statistics") == stats_before + 1,
               "request with held feedback still records history")
    eventually(lambda: len(providers.chat_posts) > chat_before, "feedback chat POST observed")
    check(providers.chat_posts[-1]["fixture_sent"] is False, "Fixture did not model an unsent feedback")
    time.sleep(1.0)
    check(len(eventsub.echoes()) == echoes_before, "is_sent=false feedback was treated as a Twitch acknowledgement")

    # 429 with Retry-After is retried a bounded number of times, then succeeds.
    settle_feedback()
    attempts = providers.chat_send_attempts
    attempt_index = attempts
    echoes_before = len(eventsub.echoes())
    posts_before = len(providers.posts)
    providers.chat_429 = 1
    eventsub.inject("smoke-429", "https://osu.ppy.sh/beatmaps/1001", author="ratelimitedviewer")
    eventually(lambda: len(providers.posts) == posts_before + 1, "rate-limited request still PMs")
    eventually(lambda: len(eventsub.echoes()) == echoes_before + 1, "feedback succeeded after 429 retry", timeout=12)
    check(providers.chat_429 == 0, "Fixture 429 was not consumed")
    check(providers.chat_send_attempts >= attempts + 2, "Chat 429 feedback was not retried")
    retry_times = providers.chat_attempt_times[attempt_index:]
    check(len(retry_times) >= 2 and retry_times[1] - retry_times[0] >= 0.9,
          f"Retry-After was not respected: {retry_times}")

    # HTTP 200 without is_sent is uncertain: report/no replay, never acknowledge.
    settle_feedback()
    attempts = providers.chat_send_attempts
    echoes_before = len(eventsub.echoes())
    posts_before = len(providers.posts)
    providers.chat_missing_ack = True
    eventsub.inject("smoke-missing-ack", "https://osu.ppy.sh/beatmaps/1001", author="uncertainchat")
    eventually(lambda: len(providers.posts) == posts_before + 1, "missing-ack request still PMs")
    eventually(lambda: providers.chat_send_attempts > attempts, "chat feedback POST observed")
    time.sleep(1.2)
    check(providers.chat_send_attempts == attempts + 1, "Missing is_sent acknowledgement was replayed")
    check(len(eventsub.echoes()) == echoes_before, "Missing is_sent was treated as delivered")


def exercise_hybrid(service, browser):
    eventsub, providers = service.eventsub, service.providers
    clock = {"value": 0}
    def at():
        clock["value"] += 10
        return timestamp(clock["value"])
    def active():
        return (providers.subscription("channel.chat.message") is not None and
                browser.ok("/live/users?limit=100") == ["streamer"])
    def inactive():
        return (providers.subscription("channel.chat.message") is not None and
                browser.ok("/live/users?limit=100") == [])

    def reject_inactive(message_id):
        settle_chat_feedback(providers, eventsub)
        posts = len(providers.posts)
        stats = service.scalar("SELECT count(*) FROM statistics")
        attempts = providers.chat_send_attempts
        eventsub.inject(message_id, "https://osu.ppy.sh/beatmaps/1001", author="offlineviewer")
        time.sleep(1.2)
        check(len(providers.posts) == posts, f"Inactive request sent an osu PM: {message_id}")
        check(service.scalar("SELECT count(*) FROM statistics") == stats,
              f"Inactive request recorded success: {message_id}")
        check(providers.chat_send_attempts == attempts,
              f"Inactive request emitted Twitch feedback: {message_id}")

    eventsub.state("channel.update", category="509659", at=at())
    eventually(inactive, "category away from osu makes channel immediately inactive")
    reject_inactive("category-away-normal")
    eventsub.state("channel.update", category="21465", at=at())
    eventually(active, "category back to osu reactivates while online")

    eventsub.state("stream.offline", at=at())
    eventually(inactive, "stream.offline makes channel immediately inactive")
    check(providers.subscription("stream.online") and providers.subscription("stream.offline")
          and providers.subscription("channel.update"), "Offline channel lost its state subscriptions")
    reject_inactive("offline-normal")
    browser.ok("/user/settings", "POST", settings(test=True))
    posts = len(providers.posts)
    stats = service.scalar("SELECT count(*) FROM statistics")
    echoes = len(eventsub.echoes())
    eventsub.inject("offline-test-mode", "https://osu.ppy.sh/beatmaps/1001", author="offlinetester")
    eventually(lambda: len(providers.posts) == posts + 1, "test mode delivers while offline")
    eventually(lambda: service.scalar("SELECT count(*) FROM statistics") == stats + 1,
               "offline test-mode request records history")
    eventually(lambda: any("Request sent!" in echo for echo in eventsub.echoes()[echoes:]),
               "offline test-mode success feedback")
    check(inactive(), "Offline test mode fabricated live eligibility")
    browser.ok("/user/settings", "POST", settings())
    reject_inactive("offline-normal-after-test")
    late = clock["value"] - 500
    eventsub.state("channel.update", category="21465", at=timestamp(late))
    time.sleep(1.0)
    check(inactive(), "Late category event reactivated an offline channel")

    # stream.online carries no category; a targeted Helix lookup supplies it.
    before_streams = providers.count_path("/twitch/helix/streams")
    eventsub.state("stream.online", at=at())
    eventually(active, "stream.online with targeted current-stream lookup becomes eligible")
    check(providers.count_path("/twitch/helix/streams") > before_streams,
          "Online event did not trigger a targeted category lookup")
    late_offline = clock["value"] - 500
    eventsub.state("stream.offline", at=timestamp(late_offline))
    time.sleep(1.0)
    check(active(), "Out-of-order offline undid a newer online event")

    # New channel grant/enable notifications wake reconciliation immediately.
    browser.ok("/user/settings", "POST", settings(enable=False, test=True))
    eventually(lambda: all(providers.subscription(kind) is None for kind in
                          ("channel.chat.message", "stream.online", "stream.offline", "channel.update")),
               "enable remains master switch even in test mode: all subscriptions removed")
    check(browser.ok("/live/users?limit=100") == [], "Disabled account remained live")
    browser.ok("/user/settings", "POST", settings())
    eventually(active, "enable immediately restores eligibility")
    wait_owned_subscriptions(service, "complete owned subscriptions restored after re-enable", timeout=15)

    # A slow HTTP snapshot captured before a newer offline event must not win.
    providers.stream_release.clear()
    providers.stream_entered.clear()
    providers.live = {101: "streamer"}
    race = Browser(service.origin)
    race.oauth("twitch", 105)
    race.oauth("osu", 205)
    eventually(lambda: providers.stream_entered.is_set(), "grant-driven reconciliation snapshot started")
    eventsub.state("stream.offline", at=at())
    providers.stream_release.set()
    time.sleep(1.2)
    check(inactive(), "Stale snapshot resurrected live state or removed the chat subscription")
    reject_inactive("stale-snapshot-normal")
    check(race.request("/user/me", "DELETE")[0] in (302, 303, 307), "Deleting temporary channel failed")
    eventsub.state("stream.online", at=at())
    eventually(active, "primary channel resumes after stale-snapshot race")

    # A fresh offline grant gets chat immediately, without fabricated live state.
    providers.live = {101: "streamer"}
    signup = Browser(service.origin)
    signup.oauth("twitch", 107)
    signup.oauth("osu", 207)
    eventually(lambda: providers.subscription("channel.chat.message", 107) is not None,
               "new offline grant is subscribed without waiting for the audit", timeout=15)
    check("streamer107" not in browser.ok("/live/users?limit=100"),
          "Offline grant fabricated live eligibility")
    linked = signup.ok("/user/me")
    check(linked["twitchBotAuthorized"] is True and linked["isLive"] is False,
          "Offline granted account status incorrect")
    check(signup.request("/user/me", "DELETE")[0] in (302, 303, 307), "Deleting granted account failed")
    eventually(lambda: providers.subscription("channel.chat.message", 107) is None,
               "account deletion removed its chat subscription", timeout=15)
    check(service.scalar("SELECT count(*) FROM users WHERE twitch_id=107") == 0, "Deletion left the account")

    # Restore the primary channel for transport and persistence tests.
    providers.live = {101: "streamer"}
    browser.ok("/user/settings", "POST", settings())
    eventsub.state("stream.online", at=at())
    eventually(active, "primary channel active again")


def exercise_transport(service, browser):
    eventsub, providers = service.eventsub, service.providers
    check(eventsub.joined(), "chat subscription missing before transport tests")
    chats_before = providers.subscription_creates[("channel.chat.message", "101")]

    # Session reconnect: keep processing the old connection until the new welcome.
    old = eventsub.handoff()
    eventually(lambda: eventsub.handoff_opened.is_set(), "client opened the reconnect URL")
    candidate = eventsub.handoff_candidate
    check(candidate is not None and candidate > old, "Reconnect did not open a new candidate socket")
    before = len(providers.posts)
    eventsub.inject("smoke-handoff-old", "https://osu.ppy.sh/beatmaps/1001",
                    author="handoffviewer", number=old)
    eventually(lambda: len(providers.posts) == before + 1, "old connection still delivered during handoff")
    eventsub.delay_welcome.set()
    eventually(lambda: eventsub.clients[candidate]["alive"] and eventsub.clients[candidate]["welcomed"]
               and eventsub.shard_number(0) == candidate,
               "exact new live session welcomed and attached", timeout=15)
    eventsub.inject("smoke-handoff-new", "https://osu.ppy.sh/beatmaps/1001",
                    author="posthandoff", number=candidate)
    eventually(lambda: len(providers.posts) == before + 2
               and service.scalar("SELECT status FROM deliveries WHERE twitch_message_id=?",
                                  ("smoke-handoff-new",)) == "sent",
               "exact candidate reader delivered after handoff")
    eventually(lambda: not eventsub.clients[old]["alive"], "retired old socket was not closed", timeout=20)
    check(providers.subscription_creates[("channel.chat.message", "101")] == chats_before,
          "Handoff recreated a duplicate owned subscription")

    # Connection drop: capped reconnect and reattach without duplicate subscriptions.
    old_connections = eventsub.connections
    before = len(providers.posts)
    eventsub.drop()
    eventually(lambda: eventsub.connections > old_connections and eventsub.joined(),
               "reconnect and shard reattach after transport drop", timeout=30)
    eventsub.inject("smoke-reconnected", "https://osu.ppy.sh/beatmaps/1001", author="reconnectedviewer")
    eventually(lambda: len(providers.posts) == before + 1, "delivery after reconnect")
    check(providers.subscription_creates[("channel.chat.message", "101")] == chats_before,
          "Reconnect recreated existing owned subscriptions")

    # Missing keepalives (not Ping) must trip the application timer and reconnect.
    old_connections = eventsub.connections
    eventsub.keepalives = False
    probe_connection = max(eventsub.attached())
    stop_pings = threading.Event()

    def ping_probe():
        while not stop_pings.wait(1.0):
            with contextlib.suppress(OSError, KeyError):
                eventsub.frame(probe_connection, 9, b"smoke-ping")
    pinger = threading.Thread(target=ping_probe, daemon=True)
    pinger.start()
    try:
        eventually(lambda: eventsub.connections > old_connections and eventsub.joined(),
                   "keepalive timeout reconnect", timeout=30)
    finally:
        stop_pings.set()
        pinger.join(2)
    eventsub.keepalives = True
    before = len(providers.posts)
    eventsub.inject("smoke-keepalive", "https://osu.ppy.sh/beatmaps/1001", author="keepaliveviewer")
    eventually(lambda: len(providers.posts) == before + 1, "delivery after keepalive reconnect")


def exercise_dry_run(service, browser):
    service.stop()
    service.start(dry_run=True)
    wait_owned_subscriptions(service, "complete dry-run owned subscriptions", timeout=25)
    before_posts = len(service.providers.posts)
    before_echoes = len(service.eventsub.echoes())
    before_stats = service.scalar("SELECT count(*) FROM statistics")
    browser.ok("/user/settings", "POST", settings())
    service.eventsub.inject("dryrun-success", "https://osu.ppy.sh/beatmaps/1010", author="dryrunviewer")
    eventually(lambda: service.providers.count_path("/osu/api/v2/beatmaps/1010") > 0,
               "dry-run request actually parsed and looked up")
    browser.ok("/user/settings", "POST", settings(sr=[0, 1]))
    service.eventsub.inject("dryrun-reject", "https://osu.ppy.sh/beatmaps/1011", author="dryrunviewer2")
    eventually(lambda: service.providers.count_path("/osu/api/v2/beatmaps/1011") > 0,
               "dry-run rejected request actually processed")
    time.sleep(0.5)
    check(len(service.providers.posts) == before_posts, "Dry-run sent osu PM")
    check(len(service.eventsub.echoes()) == before_echoes, "Dry-run emitted success/rejection chat")
    check(service.scalar("SELECT count(*) FROM statistics") == before_stats, "Dry-run wrote request statistics")
    check(service.process.poll() is None, "Dry-run request processing killed service")


def exercise_uncertain(service, browser):
    """Accepted PM with a lost response: never replayed, including after restart."""
    providers, eventsub = service.providers, service.eventsub
    settle_chat_feedback(providers, eventsub)
    before_posts = len(providers.posts)
    before_echoes = len(eventsub.echoes())
    before_stats = service.scalar("SELECT count(*) FROM statistics")
    time.sleep(0.4)
    before_stats = service.scalar("SELECT count(*) FROM statistics")
    with providers.lock:
        providers.ambiguous = True
    eventsub.inject("smoke-uncertain", "https://osu.ppy.sh/beatmaps/1001", author="uncertain_viewer")
    eventually(lambda: len(providers.posts) >= before_posts + 1, "ambiguous POST accepted by fake provider")
    eventually(lambda: service.scalar("SELECT status FROM deliveries WHERE twitch_message_id='smoke-uncertain'") ==
               "uncertain", "uncertain delivery persisted after response loss")
    time.sleep(2.2)
    check(len(providers.posts) == before_posts + 1, "Ambiguous POST was replayed")
    after_stats = service.scalar("SELECT count(*) FROM statistics")
    check(after_stats == before_stats, f"Uncertain PM recorded as success ({before_stats}->{after_stats})")
    check(len(eventsub.echoes()) == before_echoes, "Uncertain PM emitted success chat")

    created = providers.subscription_creates[("channel.chat.message", "101")]
    conduits = len(providers.conduits)
    service.stop()
    service.start()
    wait_owned_subscriptions(service, "complete owned subscriptions after restart", timeout=30)
    check(len(providers.conduits) == conduits, "Restart created a second owned conduit")
    check("foreign-conduit" in providers.conduits, "Restart deleted the unrelated conduit")
    check(providers.subscription_creates[("channel.chat.message", "101")] == created,
          "Restart recreated already-owned remote subscriptions")
    eventsub.inject("smoke-uncertain", "https://osu.ppy.sh/beatmaps/1001", author="uncertain_viewer")
    time.sleep(2.2)
    check(len(providers.posts) == before_posts + 1, "Restart/duplicate replayed uncertain PM")
    check(service.scalar("SELECT count(*) FROM users WHERE twitch_id=101") == 1,
          "Transport failures deleted a registered account")
    check(service.scalar("SELECT count(*) FROM statistics WHERE requested_channel_name='streamer'") > 3,
          "Bot history not persisted across restart")
    rows = browser.ok("/requests/beatmaps/top/daily?limit=100")
    check(any(row["_id"] == 1001 and row["count"] > 1 for row in rows), "Bot requests absent from leaderboard")


def exercise_short_reconcile(service, browser):
    """Missed state notification repaired by a short configured reconciliation."""
    providers, eventsub = service.providers, service.eventsub
    service.stop()
    service.env["RECONCILE_INTERVAL_SECS"] = "3"
    try:
        service.start()
        wait_owned_subscriptions(service, "complete short-reconcile owned subscriptions", timeout=25)
        providers.live = {}
        eventually(lambda: eventsub.joined() and browser.ok("/live/users?limit=100") == [],
                   "periodic reconciliation marks missed offline state while preserving chat", timeout=15)
        providers.live = {101: "streamer"}
        eventually(lambda: eventsub.joined() and browser.ok("/live/users?limit=100") == ["streamer"],
                   "periodic reconciliation restores missed online state", timeout=15)
    finally:
        service.env["RECONCILE_INTERVAL_SECS"] = "600"


def exercise_revocation(service, browser):
    eventsub, providers = service.eventsub, service.providers
    check(service.scalar("SELECT count(*) FROM users WHERE twitch_id=101") == 1, "Precondition account missing")
    check(service.scalar("SELECT status FROM twitch_channel_grants WHERE twitch_id=101") == "granted"
          and browser.ok("/user/me")["isLive"] is True,
          "Revocation precondition: primary channel is not granted and live")
    authorizations_before = providers.count_path("/twitch/helix/authorization/users")
    eventsub.revoke()
    eventually(lambda: providers.count_path("/twitch/helix/authorization/users") > authorizations_before
               and service.scalar("SELECT status FROM twitch_channel_grants WHERE twitch_id=101") == "revoked"
               and providers.subscription("channel.chat.message") is None
               and service.scalar("SELECT is_live FROM users WHERE twitch_id=101") == 0
               and "streamer" not in browser.ok("/live/users?limit=100"),
               "revocation durably revoked consent, removed its subscription and live eligibility", timeout=12)
    user = browser.ok("/user/me")
    check(user["twitchBotAuthorized"] is False and user["isLive"] is False,
          "Revoked consent remained authorized or live in the account response")
    attempts = providers.chat_post_attempts
    time.sleep(1.5)
    check(providers.chat_post_attempts <= attempts + 3,
          "Revoked authorization caused an unbounded resubscribe hotloop")
    check(service.scalar("SELECT count(*) FROM users WHERE twitch_id=101") == 1,
          "Subscription revocation deleted the registered account")
    check(service.process.poll() is None, "Revocation crashed the service")
    check(browser.ok("/health")["status"] == "ok", "Revocation broke API liveness")


def exercise_cli_errors(service):
    """Device/validation failures preserve the encrypted evidence and account."""
    providers = service.providers
    service.stop()
    before = auth_row(service)
    providers.device_denied = True
    authorize_bot(service, expected=1, activate=False, timeout=20)
    providers.device_denied = False
    check(auth_row(service) == before, "Denied device flow changed the stored grant")

    providers.invalid_scopes = True
    output = authorize_bot(service, expected=1, timeout=25)
    providers.invalid_scopes = False
    check(auth_row(service) == before, "Invalid scopes replaced a valid stored grant")
    check("bot-access" not in output and "bot-refresh" not in output, "Failure exposed a bot credential")

    providers.bot_login = "otherbot"
    providers.bot_user_id = "888"
    authorize_bot(service, login="otherbot", expected=1, timeout=20)
    check(auth_row(service) == before, "Different bot identity was replaced without --replace")
    authorize_bot(service, login="otherbot", replace=True, expected=0,
                  expected_user_id="888", timeout=30)
    row = auth_row(service)
    check(row[0] == "888" and row[1] == "otherbot" and row != before,
          "--replace did not rotate the stored bot identity")
    eventsub = service.eventsub
    eventsub.start()
    service.start()
    eventually(eventsub.attached, "bot reconnects after explicit identity replacement", timeout=30)


def exercise_debug_and_maintenance(service, browser):
    service.stop()
    debug = Service(service.binary, service.root / "debug", service.providers, service.eventsub)
    debug.root.mkdir(exist_ok=True)
    result = run_captured(debug.command("check"), env=debug.env)
    check(result.returncode == 0, f"Debug database check failed:\n{result.stdout}")
    debug.start(api_only=True, debug=True)
    try:
        probe = Browser(debug.origin)
        check(probe.request("/openapi.json")[0] == 200, "DEBUG_MODE did not expose OpenAPI")
        check(probe.request("/docs")[0] == 200, "DEBUG_MODE did not expose docs")
        check(probe.request("/redoc")[0] == 200, "DEBUG_MODE did not expose redoc")
        document = probe.ok("/openapi.json")
        paths = document["paths"]
        check(all(path in paths for path in ("/user/me", "/health", "/ready")), "OpenAPI lost routes")
        check("twitchBotAuthorized" in json.dumps(document), "OpenAPI lost twitchBotAuthorized")
    finally:
        debug.stop()

    integrity = run_captured(service.command("check"), env=service.env)
    check(integrity.returncode == 0, f"CLI database check failed:\n{integrity.stdout}")
    check(cli_json(integrity.stdout) == {"integrity": ["ok"], "foreignKeyViolations": 0},
          "CLI integrity report incorrect")
    inspection = run_captured(service.command("deliveries", "--limit", "10"), env=service.env)
    check(inspection.returncode == 0, f"CLI deliveries failed:\n{inspection.stdout}")
    check(any(row["twitchMessageId"] == "smoke-uncertain" and row["status"] == "uncertain"
              for row in cli_json(inspection.stdout)), "CLI deliveries omitted uncertain accepted PM")

    backup = service.root / "backup.sqlite3"
    source = service.database
    original = database_rows(source)
    result = run_captured(service.command("backup", "--destination", str(backup)), env=service.env)
    check(result.returncode == 0 and backup.is_file(), f"CLI backup failed:\n{result.stdout}")
    check(database_rows(backup) == original, "Consistent SQLite backup lost data")
    existing = run_captured(service.command("backup", "--destination", str(backup)), env=service.env)
    check(existing.returncode != 0, "Backup unexpectedly overwrote existing destination")
    check(database_rows(backup) == original, "Rejected overwrite changed backup")
    restored_check = run_captured([service.binary, "--database", str(backup), "check"], env=service.env)
    check(restored_check.returncode == 0, f"Restored backup failed CLI check:\n{restored_check.stdout}")
    connections = service.eventsub.connections
    service.database = backup
    try:
        service.start(api_only=True)
        check(browser.ok("/user/me")["twitchId"] == 101, "Restored sessions/account unusable through API")
        check(any(row["_id"] == 1001 and row["count"] > 1
                  for row in browser.ok("/requests/beatmaps/top/daily?limit=100")),
              "Restored request history unusable through API")
        check(service.eventsub.connections == connections, "--api-only unexpectedly connected EventSub")
    finally:
        service.stop()
        service.database = source

    database = service.root / "backfill.sqlite3"
    command = [service.binary, "--database", str(database)]
    result = run_captured([*command, "check"], env=service.env)
    check(result.returncode == 0, f"Backfill database check failed:\n{result.stdout}")
    seed_catalog(database)
    with connect(database) as conn:
        insert_history(conn, 1300)
    with connect(database, readonly=True) as conn:
        check(conn.execute("SELECT count(*) FROM statistics WHERE requested_beatmap_id=1300").fetchone()[0] == 1,
              "Missing metadata fixture did not contain real history")
        check(conn.execute("SELECT count(*) FROM beatmaps WHERE id=1300").fetchone()[0] == 0,
              "Backfill fixture unexpectedly already has metadata")
    before = service.providers.count_path("/osu/api/v2/beatmaps/1300")
    backfilled = run_captured([*command, "backfill"], env=service.env)
    check(backfilled.returncode == 0, f"CLI backfill failed:\n{backfilled.stdout}")
    check(service.providers.count_path("/osu/api/v2/beatmaps/1300") == before + 1,
          "Backfill did not query missing map through configured provider")
    with connect(database, readonly=True) as conn:
        row = conn.execute("SELECT raw_json FROM beatmaps WHERE id=1300").fetchone()
        check(row and json.loads(row[0])["synthetic_marker"] == "generated-not-user-data",
              "Backfill did not persist real provider metadata")
    repeat = run_captured([*command, "backfill"], env=service.env)
    check(repeat.returncode == 0 and service.providers.count_path("/osu/api/v2/beatmaps/1300") == before + 1,
          "Backfill fetched already populated history again")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/ronnia", help="already-built service executable")
    parser.add_argument("--keep", action="store_true", help="retain generated fixture, SQLite and logs")
    args = parser.parse_args()
    install_signal_handlers()
    root = Path(tempfile.mkdtemp(prefix="ronnia-smoke-"))
    providers = Providers()
    eventsub = EventSub(providers)
    providers.eventsub = eventsub
    service = None
    try:
        service = Service(args.binary, root, providers, eventsub)
        seed_fixture(service)
        dns_fault = service.configure_dns_failure()
        # An unrelated conduit already on the account must not be adopted or deleted.
        providers.conduits["foreign-conduit"] = {"id": "foreign-conduit", "shard_count": 1}
        providers.subscriptions["foreign-sub"] = {
            "id": "foreign-sub", "type": "channel.follow", "version": "2",
            "condition": {"broadcaster_user_id": "101", "moderator_user_id": "777"},
            "transport": {"method": "webhook", "callback": "https://example.invalid/hook"},
            "status": "enabled", "cost": 0, "created_at": timestamp()}
        providers.stream_401 = True
        providers.map_401 = True
        # No stored bot grant: API must stay up and report readiness.
        service.start()
        initial = Browser(service.origin)
        check(initial.ok("/health") == {"status": "ok"} and initial.ok("/ready") == {"status": "ready"},
              "No-grant degraded startup did not serve API")
        check(initial.ok("/live/users?limit=100") == [], "Ungranted fixture channel was active")
        check(providers.count_path("/twitch/helix/eventsub/conduits") == 0,
              "Bot contacted EventSub before a grant existed")
        check(service.scalar("SELECT count(*) FROM twitch_bot_auth") == 0,
              "A bot authorization was fabricated before the device flow")

        authorize_bot(service, login="SmokeBot")
        assert_encrypted_grant(service)
        check(providers.device_polls >= 1, "Device authorization never polled pending")
        check(providers.device_pending_polls >= 1, "CLI never observed authorization_pending")
        service.stop()
        providers.bot_401 = True
        providers.live = {}
        startup_streams = providers.count_path("/twitch/helix/streams")
        startup_revision = service.scalar("SELECT revision FROM channel_stream_state WHERE twitch_id=101") or 0
        service.start()
        eventually(lambda: providers.refreshes >= 1, "Validate 401 did not trigger a token refresh")
        eventually(lambda: providers.bot_401_hits >= 1, "Bot never validated its stored credential")
        assert_encrypted_grant(service)
        if dns_fault:
            def dns_log():
                text = service.logs[-1].read_text(errors="replace")
                return (text.count("RONNIA_SMOKE_DNS_FAILURE") >= 2
                        and text.count("RONNIA_SMOKE_DNS_ATTEMPT") >= 3)
            eventually(dns_log, "two actual EAI_NONAME failures then a resolved attempt", timeout=30)
        # The real WebSocket server is still bound but not listening: connection refused
        # is survived, then the same endpoint becomes available.
        time.sleep(0.5)
        check(service.process.poll() is None, "Transport refusal killed the service")
        eventsub.start()
        # Finish startup with the enabled fixture account offline and ungranted.
        # Going live now must be discovered by consent, not startup or a state event.
        wait_offline_startup(service, startup_streams, startup_revision,
                             "ungranted fixture account provider startup snapshot applied")
        check(initial.ok("/live/users?limit=100") == [], "Ungranted startup account became eligible")
        providers.live = {101: "streamer"}

        browser = exercise_api(service)
        exercise_grant_grace(service, browser)
        exercise_bot(service, browser)
        exercise_hybrid(service, browser)
        exercise_transport(service, browser)
        exercise_uncertain(service, browser)
        exercise_dry_run(service, browser)
        exercise_short_reconcile(service, browser)
        exercise_revocation(service, browser)
        exercise_cli_errors(service)
        exercise_debug_and_maintenance(service, browser)
        check(not providers.errors, f"Fake provider protocol errors: {providers.errors}")
        print("PASS: all 16 HTTP routes, /health+/ready, both signup orders with channel:bot,")
        print("      encrypted device authorization, EventSub conduit/shard/subscriptions, real WS chat→osu PMs,")
        print("      hybrid online/offline/category ordering, existing-account grant/grace lookup, watch wakeups,")
        print("      refused connect + DNS EAI_NONAME recovery, reconnect/handoff/keepalive, revocation safety,")
        print("      token refresh/401, is_sent=false, ambiguous accepted POST not replayed, dry-run.")
        print("PASS: CLI check, backup/restore, deliveries, backfill,")
        print("      device denial/invalid scopes/identity replacement, debug OpenAPI gated by DEBUG_MODE.")
        print("PASS: same-process DNS EAI_NONAME recovery via isolated LD_PRELOAD." if dns_fault else
              "NOT COVERED: process-local DNS recovery (requires Linux LD_PRELOAD).")
        print("NOT COVERED: TLS/real-provider interoperability, Twitch outage retention and production load.")
        return 0
    except Exception as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        if providers.errors:
            print(f"Provider errors: {providers.errors}", file=sys.stderr)
        if service:
            service.stop()
            service.print_logs()
        return 1
    finally:
        if service:
            service.stop()
        eventsub.close()
        providers.close()
        if args.keep:
            print(f"Artifacts: {root}")
        else:
            import shutil
            shutil.rmtree(root)


if __name__ == "__main__":
    sys.exit(main())
