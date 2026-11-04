"""Local, standard-library Twitch EventSub/Helix/OAuth fixtures (no application mocks)."""
from __future__ import annotations

import base64
import collections
import contextlib
import datetime as dt
import hashlib
import http.server
import json
import socket
import struct
import threading
import time
import urllib.parse

BOT_SCOPES = ["user:read:chat", "user:write:chat", "user:bot"]


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def beatmap(map_id):
    return {"id": map_id, "beatmapset_id": 500, "difficulty_rating": 9.0 if map_id == 1004 else 5.25,
            "version": "Synthetic Insane", "bpm": 180.0, "status": "ranked", "hit_length": 125,
            "total_length": 130, "mode": "osu", "synthetic_marker": "generated-not-user-data",
            "beatmapset": {"id": 500, "artist": "Synthetic Artist", "title": "Generated Song",
                           "creator": "Fixture Mapper", "status": "ranked"}}


def websocket_bridge(handler, irc):
    """RFC6455 text transport used only by the unchanged original TwitchIO baseline."""
    key = handler.headers.get("Sec-WebSocket-Key", "")
    check(key, "WebSocket upgrade without key")
    accept = base64.b64encode(hashlib.sha1(
        (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
    handler.send_response(101)
    handler.send_header("Upgrade", "websocket")
    handler.send_header("Connection", "Upgrade")
    handler.send_header("Sec-WebSocket-Accept", accept)
    handler.end_headers()
    handler.wfile.flush()
    handler.close_connection = True
    upstream = socket.create_connection(("127.0.0.1", irc.port), timeout=3)
    upstream.settimeout(0.2)
    stopped = threading.Event()
    send_lock = threading.Lock()

    def send_frame(opcode, payload):
        size = len(payload)
        length = bytes([size]) if size < 126 else b"\x7e" + struct.pack("!H", size) if size < 65536 else b"\x7f" + struct.pack("!Q", size)
        with send_lock:
            handler.connection.sendall(bytes([0x80 | opcode]) + length + payload)

    def receive_frames():
        try:
            while not stopped.is_set():
                header = handler.rfile.read(2)
                if len(header) != 2:
                    break
                opcode, size = header[0] & 15, header[1] & 127
                masked = bool(header[1] & 128)
                if size == 126:
                    size = struct.unpack("!H", handler.rfile.read(2))[0]
                elif size == 127:
                    size = struct.unpack("!Q", handler.rfile.read(8))[0]
                check(size < 1024 * 1024, "Oversized local WebSocket frame")
                mask = handler.rfile.read(4) if masked else b""
                payload = handler.rfile.read(size)
                check(len(payload) == size, "Truncated local WebSocket frame")
                if masked:
                    payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
                if opcode == 8:
                    send_frame(8, payload)
                    break
                if opcode == 9:
                    send_frame(10, payload)
                elif opcode in (0, 1):
                    upstream.sendall(payload + (b"" if payload.endswith(b"\n") else b"\r\n"))
        except (OSError, ValueError, struct.error):
            pass
        finally:
            stopped.set()
            with contextlib.suppress(OSError):
                upstream.shutdown(socket.SHUT_RDWR)

    reader = threading.Thread(target=receive_frames, daemon=True)
    reader.start()
    pending = b""
    try:
        while not stopped.is_set():
            try:
                data = upstream.recv(65536)
            except socket.timeout:
                continue
            if not data:
                break
            pending += data
            while b"\n" in pending:
                line, pending = pending.split(b"\n", 1)
                send_frame(1, line.rstrip(b"\r") + b"\r\n")
    except OSError:
        pass
    finally:
        stopped.set()
        upstream.close()
        with contextlib.suppress(OSError):
            handler.connection.shutdown(socket.SHUT_RDWR)
        reader.join(1)


def timestamp(offset=0):
    return (dt.datetime.now(dt.timezone.utc) + dt.timedelta(seconds=offset)).isoformat().replace("+00:00", "Z")


class EventSub:
    """Real RFC6455 server, deliberately bound but initially refusing connections."""
    def __init__(self, providers):
        self.providers = providers
        self.lock = threading.RLock()
        self.clients = {}
        self.connections = 0
        self.attaches = []
        self.stopped = threading.Event()
        self.started = False
        self.keepalives = True
        self.text_frames = 0
        self.delay_welcome = threading.Event()
        self.delay_welcome.set()
        self.handoff_opened = threading.Event()
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                key = self.headers.get("Sec-WebSocket-Key", "")
                if not key or self.headers.get("Upgrade", "").lower() != "websocket":
                    self.send_error(400)
                    return
                self.send_response(101)
                self.send_header("Upgrade", "websocket")
                self.send_header("Connection", "Upgrade")
                self.send_header("Sec-WebSocket-Accept", base64.b64encode(hashlib.sha1(
                    (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode())
                self.end_headers()
                self.wfile.flush()
                self.close_connection = True
                with owner.lock:
                    owner.connections += 1
                    number = owner.connections
                    owner.clients[number] = {"socket": self.connection, "session": f"session-{number}",
                                             "alive": True, "lock": threading.Lock(), "welcomed": False}
                if "handoff=1" in self.path:
                    owner.handoff_opened.set()
                    while not owner.delay_welcome.wait(.1):
                        if owner.stopped.is_set():
                            return
                owner.clients[number]["welcomed"] = True
                owner.envelope(number, "session_welcome", {"session": {
                    "id": f"session-{number}", "status": "connected", "keepalive_timeout_seconds": 10,
                    "reconnect_url": None, "connected_at": timestamp(), "recovery_url": None}})
                try:
                    while not owner.stopped.is_set():
                        header = self.rfile.read(2)
                        if len(header) != 2:
                            break
                        opcode, size = header[0] & 15, header[1] & 127
                        if size == 126:
                            size = struct.unpack("!H", self.rfile.read(2))[0]
                        elif size == 127:
                            size = struct.unpack("!Q", self.rfile.read(8))[0]
                        if size > 1024 * 1024:
                            raise ValueError("oversized fixture frame")
                        mask = self.rfile.read(4) if header[1] & 128 else b""
                        payload = self.rfile.read(size)
                        if mask:
                            payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
                        if opcode == 8:
                            break
                        if opcode == 9:
                            owner.frame(number, 10, payload)
                        elif opcode in (1, 2):
                            owner.text_frames += 1
                except (OSError, ValueError, struct.error):
                    pass
                finally:
                    owner.clients[number]["alive"] = False

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler, bind_and_activate=False)
        self.server.daemon_threads = True
        self.server.block_on_close = False
        self.server.server_bind()
        self.port = self.server.server_address[1]
        self.url = f"ws://127.0.0.1:{self.port}/ws"

    def start(self):
        if not self.started:
            self.started = True
            self.server.server_activate()
            threading.Thread(target=self.server.serve_forever, daemon=True).start()
            threading.Thread(target=self.ticker, daemon=True).start()

    def ticker(self):
        while not self.stopped.wait(5.0):
            if self.keepalives:
                for number, client in list(self.clients.items()):
                    if client["alive"] and client["welcomed"]:
                        with contextlib.suppress(OSError):
                            self.envelope(number, "session_keepalive", {})

    def frame(self, number, opcode, payload):
        size = len(payload)
        length = bytes([size]) if size < 126 else b"\x7e" + struct.pack("!H", size) if size < 65536 else b"\x7f" + struct.pack("!Q", size)
        client = self.clients[number]
        with client["lock"]:
            client["socket"].sendall(bytes([0x80 | opcode]) + length + payload)

    def envelope(self, number, kind, payload, subscription=None, at=None, message_id=None):
        metadata = {"message_id": message_id or f"envelope-{time.time_ns()}", "message_type": kind,
                    "message_timestamp": at or timestamp()}
        if subscription:
            metadata.update(subscription_type=subscription["type"], subscription_version=subscription["version"])
        self.frame(number, 1, json.dumps({"metadata": metadata, "payload": payload}).encode())

    def attached(self):
        with self.lock:
            return [number for number, client in self.clients.items()
                    if client["alive"] and client["session"] in self.attaches]

    def joined(self, channel="streamer"):
        # Compatibility of the workload adapter, not an IRC production transport.
        with self.providers.lock:
            subscribed = any(row["type"] == "channel.chat.message" and
                             row["condition"]["broadcaster_user_id"] == "101"
                             for row in self.providers.subscriptions.values())
        return self.attached() if subscribed else []

    def subscription(self, kind, uid=101):
        return self.providers.subscription(kind, uid)

    def notify(self, kind, event, uid=101, at=None, number=None, message_id=None):
        subscription = self.subscription(kind, uid)
        if not subscription:
            raise AssertionError(f"No real subscription for {kind}/{uid}")
        numbers = self.attached()
        if number is None:
            if not numbers:
                raise AssertionError("EventSub shard is not attached")
            number = max(numbers)
        self.envelope(number, "notification", {"subscription": subscription, "event": event},
                      subscription, at, message_id)

    def inject(self, message_id, text, author="viewer", subscriber=False, moderator=False,
               vip=False, points=False, user_id=301, number=None):
        badges = [{"set_id": name, "id": "1", "info": ""} for name, enabled in
                  [("subscriber", subscriber), ("moderator", moderator), ("vip", vip),
                   ("broadcaster", user_id == 101)] if enabled]
        self.notify("channel.chat.message", {"broadcaster_user_id": "101", "broadcaster_user_login": "streamer",
                    "broadcaster_user_name": "Streamer", "chatter_user_id": str(user_id),
                    "chatter_user_login": author, "chatter_user_name": author, "message_id": message_id,
                    "message": {"text": text, "fragments": [{"type": "text", "text": text}]},
                    "badges": badges, "message_type": "text", "color": "#1E90FF",
                    "channel_points_custom_reward_id": "reward" if points else None},
                    number=number, message_id="transport-" + message_id)

    def state(self, kind, category=None, at=None):
        event = {"broadcaster_user_id": "101", "broadcaster_user_login": "streamer",
                 "broadcaster_user_name": "Streamer"}
        if kind == "channel.update":
            event.update(title="Synthetic stream", language="en", category_id=category,
                         category_name="osu!" if category == "21465" else "Other", content_classification_labels=[])
        if kind == "stream.online":
            event.update(id="synthetic-stream", type="live", started_at=timestamp())
        self.notify(kind, event, at=at)

    def revoke(self):
        subscription = self.subscription("channel.chat.message")
        subscription["status"] = "authorization_revoked"
        self.providers.chat_revoked = True
        self.envelope(max(self.attached()), "revocation", {"subscription": subscription}, subscription)
        with self.providers.lock:
            self.providers.subscriptions.pop(subscription["id"], None)

    def handoff(self):
        self.delay_welcome.clear()
        self.handoff_opened.clear()
        old = max(self.attached())
        self.envelope(old, "session_reconnect", {"session": {"id": self.clients[old]["session"],
                      "status": "reconnecting", "keepalive_timeout_seconds": None,
                      "reconnect_url": self.url + "?handoff=1", "connected_at": timestamp()}})
        return old

    def echoes(self):
        with self.providers.lock:
            return [row["message"] for row in self.providers.chat_posts if row.get("fixture_sent", True)]

    def drop(self):
        for client in list(self.clients.values()):
            client["alive"] = False
            with contextlib.suppress(OSError):
                client["socket"].shutdown(socket.SHUT_RDWR)
            with contextlib.suppress(OSError):
                client["socket"].close()

    def close(self):
        self.stopped.set()
        self.delay_welcome.set()
        self.drop()
        if self.started:
            self.server.shutdown()
        self.server.server_close()


class Providers:
    def __init__(self, baseline=False):
        self.lock = threading.RLock()
        self.calls = []
        self.tokens = collections.Counter()
        self.posts = []
        self.chat_posts = []
        self.live = {101: "streamer"}
        self.categories = {101: "21465"}
        self.stream_401 = False
        self.map_401 = False
        self.bot_401 = False
        self.bot_401_hits = 0
        self.chat_drop = False
        self.ambiguous = False
        self.invalid_scopes = False
        self.missing_user_scope = False
        self.device_authorized = False
        self.device_denied = False
        self.device_polls = 0
        self.device_pending_polls = 0
        self.bot_token = "bot-access-0"
        self.bot_refresh = "bot-refresh-0"
        self.bot_tokens = {self.bot_token}
        self.bot_login = "smokebot"
        self.bot_user_id = "999"
        self.bot_expires_in = 30
        self.refreshes = 0
        self.chat_revoked = False
        self.stream_entered = threading.Event()
        self.stream_release = threading.Event()
        self.stream_release.set()
        self.conduits = {}
        self.subscriptions = {}
        self.subscription_creates = collections.Counter()
        self.chat_post_attempts = 0
        self.chat_send_attempts = 0
        self.chat_attempt_times = []
        self.chat_429 = 0
        self.chat_retry_after = "1"
        self.chat_missing_ack = False
        self.chat_auth = ""
        self.serial = 0
        self.errors = []
        self.irc = None
        self.eventsub = None
        self.baseline = baseline
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self): self.respond("GET")
            def do_POST(self): self.respond("POST")
            def do_PATCH(self): self.respond("PATCH")
            def do_DELETE(self): self.respond("DELETE")

            def send_json(self, status, value, headers=None):
                raw = json.dumps(value).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                for key, header_value in (headers or {}).items():
                    self.send_header(key, header_value)
                self.end_headers()
                self.wfile.write(raw)

            def respond(self, method):
                try:
                    self.dispatch(method)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception as exc:
                    with owner.lock:
                        owner.errors.append(repr(exc))
                    self.send_json(500, {"error": str(exc)})

            def dispatch(self, method):
                parsed = urllib.parse.urlsplit(self.path)
                path, query = parsed.path, urllib.parse.parse_qs(parsed.query)
                if path == "/irc-websocket" and owner.irc is not None:
                    return websocket_bridge(self, owner.irc)
                raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                if raw and "json" in self.headers.get("Content-Type", ""):
                    body = json.loads(raw)
                else:
                    form = urllib.parse.parse_qs(raw.decode())
                    body = {key: (" ".join(values) if key == "scopes" else values[-1])
                            for key, values in form.items()}
                body = {**{k: v[-1] for k, v in query.items()}, **body}
                auth = self.headers.get("Authorization", "").removeprefix("Bearer ").removeprefix("OAuth ")
                with owner.lock:
                    owner.calls.append((method, path, body, dict(self.headers)))
                if path == "/device/activate":
                    check(body.get("user_code") == "SMOKE-CODE", "Wrong browser device code")
                    owner.device_authorized = True
                    return self.send_json(200, {"authorized": True})
                if path == "/twitch/oauth/device":
                    check(set(body.get("scopes", "").split()) == set(BOT_SCOPES), "Device request scopes wrong")
                    owner.device_authorized = False
                    return self.send_json(200, {"device_code": "private-device-code", "user_code": "SMOKE-CODE",
                        "verification_uri": owner.origin + "/device/activate", "expires_in": 120, "interval": 1})
                if path in ("/twitch/oauth/token", "/osu/oauth/token") and method == "POST":
                    provider = "twitch" if path.startswith("/twitch") else "osu"
                    grant = body.get("grant_type")
                    if grant == "urn:ietf:params:oauth:grant-type:device_code":
                        check(body.get("device_code") == "private-device-code", "Wrong device poll code")
                        owner.device_polls += 1
                        if owner.device_denied:
                            return self.send_json(400, {"error": "access_denied", "message": "access_denied"})
                        if not owner.device_authorized:
                            owner.device_pending_polls += 1
                            return self.send_json(400, {"error": "authorization_pending", "message": "authorization_pending"})
                        return self.send_json(200, {"access_token": owner.bot_token, "refresh_token": owner.bot_refresh,
                                                   "expires_in": owner.bot_expires_in, "scope": BOT_SCOPES, "token_type": "bearer"})
                    if grant == "refresh_token":
                        check(body.get("refresh_token") == owner.bot_refresh, "Wrong rotating refresh credential")
                        owner.refreshes += 1
                        owner.bot_token = f"bot-access-{owner.refreshes}"
                        owner.bot_refresh = f"bot-refresh-{owner.refreshes}"
                        owner.bot_tokens.add(owner.bot_token)
                        return self.send_json(200, {"access_token": owner.bot_token, "refresh_token": owner.bot_refresh,
                                                   "expires_in": 3600, "scope": BOT_SCOPES, "token_type": "bearer"})
                    scope = str(body.get("scope") or "public")
                    key = provider + ":" + scope
                    owner.tokens[key] += 1
                    code = body.get("code")
                    token = "user-" + str(code) if code else f"app-{key}-{owner.tokens[key]}"
                    return self.send_json(200, {"access_token": token, "expires_in": 3600,
                                               "refresh_token": "synthetic-refresh", "token_type": "bearer"})
                if path == "/twitch/oauth/validate":
                    if owner.baseline and auth == "synthetic":
                        return self.send_json(200, {"client_id": "synthetic-twitch", "login": "smokebot",
                            "user_id": "999", "expires_in": 3600, "scopes": ["chat:read", "chat:edit"]})
                    if auth.startswith("bot-access"):
                        if owner.bot_401:
                            owner.bot_401 = False
                            owner.bot_401_hits += 1
                            return self.send_json(401, {"status": 401, "message": "invalid access token"})
                        check(auth == owner.bot_token, "Old bot credential used after refresh")
                        return self.send_json(200, {"client_id": "synthetic-twitch", "login": owner.bot_login,
                            "user_id": owner.bot_user_id, "expires_in": 3600,
                            "scopes": ["user:read:chat"] if owner.invalid_scopes else BOT_SCOPES})
                    identity = int(auth.removeprefix("user-"))
                    return self.send_json(200, {"client_id": "synthetic-twitch", "login": "streamer" if identity == 101 else f"streamer{identity}",
                        "user_id": str(identity), "expires_in": 3600,
                        "scopes": [] if owner.invalid_scopes or owner.missing_user_scope else ["channel:bot"]})
                if path == "/twitch/helix/users":
                    identity = int(auth.removeprefix("user-"))
                    login = "streamer" if identity == 101 else f"streamer{identity}"
                    return self.send_json(200, {"data": [{"id": str(identity), "login": login,
                        "display_name": login, "profile_image_url": "https://example.invalid/avatar.png"}]})
                if path == "/osu/api/v2/me":
                    identity = int(auth.removeprefix("user-"))
                    return self.send_json(200, {"id": identity, "username": f"osu{identity}",
                                               "avatar_url": "https://example.invalid/avatar.png"})
                if path == "/twitch/helix/streams":
                    if owner.stream_401:
                        owner.stream_401 = False
                        return self.send_json(401, {"error": "Unauthorized", "status": 401})
                    check(owner.baseline or "game_id" not in query, "Hybrid snapshot incorrectly filters category")
                    ids = {part for raw in query.get("user_id", []) for part in raw.split(",")}
                    check(owner.baseline or len(ids) <= 100, "Stream snapshot exceeded 100 IDs per request")
                    rows = [{"id": f"stream-{uid}", "user_id": str(uid), "user_login": login,
                             "game_id": owner.categories.get(uid, "21465"), "type": "live", "started_at": timestamp()}
                            for uid, login in list(owner.live.items()) if str(uid) in ids]
                    owner.stream_entered.set()
                    check(owner.stream_release.wait(8), "Stalled stream snapshot was not released")
                    return self.send_json(200, {"data": rows, "pagination": {}})
                if path.startswith("/twitch/helix/eventsub/"):
                    check(auth.startswith("app-twitch:"), "Conduit/subscription request lacks app bearer")
                    resource = path.removeprefix("/twitch/helix/eventsub/")
                    if resource == "conduits":
                        if method == "GET":
                            return self.send_json(200, {"data": list(owner.conduits.values()), "pagination": {}})
                        if method == "POST":
                            owner.serial += 1
                            row = {"id": f"conduit-{owner.serial}", "shard_count": body["shard_count"]}
                            owner.conduits[row["id"]] = row
                            return self.send_json(200, {"data": [row]})
                    if resource == "conduits/shards":
                        check(body.get("conduit_id") in owner.conduits, "Unknown conduit shard")
                        if method == "PATCH":
                            shard = body["shards"][0]
                            session = shard["transport"]["session_id"]
                            check(any(c["session"] == session and c["welcomed"] for c in owner.eventsub.clients.values()),
                                  "Shard attached before actual welcome")
                            owner.eventsub.attaches.append(session)
                            return self.send_json(202, {"data": [{"id": "0", "status": "enabled", "transport": shard["transport"]}], "errors": []})
                        return self.send_json(200, {"data": [{"id": "0", "status": "enabled"}], "pagination": {}})
                    if resource == "subscriptions":
                        if method == "GET":
                            return self.send_json(200, {"data": list(owner.subscriptions.values()), "pagination": {},
                                                       "total": len(owner.subscriptions), "total_cost": 0, "max_total_cost": 10000})
                        if method == "DELETE":
                            owner.subscriptions.pop(body["id"], None)
                            self.send_response(204)
                            self.send_header("Content-Length", "0")
                            self.end_headers()
                            return
                        if method == "POST":
                            check(body["transport"]["method"] == "conduit", "Not a conduit subscription")
                            check(body["transport"]["conduit_id"] in owner.conduits, "Unknown subscription conduit")
                            condition = {str(key): str(value) for key, value in body["condition"].items()}
                            body = {**body, "condition": condition}
                            check(set(condition) <= {"broadcaster_user_id", "user_id"}, f"Unexpected condition {condition}")
                            if body["type"] == "channel.chat.message":
                                check(condition.get("user_id") == owner.bot_user_id, "Chat subscription missing bot user_id")
                                owner.chat_post_attempts += 1
                                if owner.chat_revoked:
                                    return self.send_json(403, {"error": "Forbidden", "status": 403,
                                                                "message": "authorization revoked"})
                            else:
                                check(body["type"] in ("stream.online", "stream.offline", "channel.update"),
                                      f"Unexpected subscription type {body['type']}")
                                check(str(body.get("version")) == ("2" if body["type"] == "channel.update" else "1"),
                                      "Wrong subscription version")
                            key = (body["type"], condition["broadcaster_user_id"])
                            if any((r["type"], r["condition"]["broadcaster_user_id"]) == key for r in owner.subscriptions.values()):
                                return self.send_json(409, {"error": "Conflict", "status": 409})
                            owner.serial += 1
                            row = {**body, "id": f"subscription-{owner.serial}", "status": "enabled", "cost": 0,
                                   "created_at": timestamp()}
                            owner.subscriptions[row["id"]] = row
                            owner.subscription_creates[key] += 1
                            return self.send_json(202, {"data": [row], "total": len(owner.subscriptions),
                                                       "total_cost": 0, "max_total_cost": 10000})
                if path == "/twitch/helix/chat/messages" and method == "POST":
                    owner.chat_auth = auth
                    check(auth in owner.bot_tokens,
                          f"Chat send must use an issued bot user token, got {auth[:24]}")
                    check(str(body.get("sender_id")) == owner.bot_user_id, "Wrong chat sender identity")
                    check(str(body.get("broadcaster_id", "")).isdigit(), "Wrong chat broadcaster")
                    owner.chat_send_attempts += 1
                    owner.chat_attempt_times.append(time.monotonic())
                    if owner.chat_429 > 0:
                        owner.chat_429 -= 1
                        return self.send_json(429, {"error": "Too Many Requests", "status": 429},
                                              {"Retry-After": owner.chat_retry_after})
                    if owner.chat_missing_ack:
                        owner.chat_missing_ack = False
                        return self.send_json(200, {"data": [{}]})
                    sent = not owner.chat_drop
                    owner.chat_drop = False
                    owner.chat_posts.append({**body, "fixture_sent": sent})
                    return self.send_json(200, {"data": [{"message_id": f"chat-{len(owner.chat_posts)}", "is_sent": sent,
                        "drop_reason": None if sent else {"code": "automod_held", "message": "Held"}}]})
                if path.startswith("/osu/api/v2/beatmaps/"):
                    if owner.map_401:
                        owner.map_401 = False
                        return self.send_json(401, {"error": "expired token"})
                    return self.send_json(200, beatmap(int(path.rsplit("/", 1)[1])))
                if path == "/osu/api/v2/beatmapsets/500":
                    return self.send_json(200, {**beatmap(1003)["beatmapset"], "beatmaps": [beatmap(1003), beatmap(1002)]})
                if path == "/osu/api/v2/chat/new" and method == "POST":
                    check(body.get("target_id") == 201 and body.get("is_action") is False, f"Wrong PM: {body}")
                    owner.posts.append(body)
                    if owner.ambiguous:
                        owner.ambiguous = False
                        self.close_connection = True
                        with contextlib.suppress(OSError):
                            self.connection.shutdown(socket.SHUT_RDWR)
                        self.connection.close()
                        return
                    return self.send_json(200, {"channel_id": 123, "message_id": len(owner.posts)})
                owner.errors.append(f"Unexpected request: {method} {path}")
                return self.send_json(404, {"error": "unexpected fixture endpoint"})

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.server.block_on_close = False
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.origin = f"http://127.0.0.1:{self.server.server_port}"

    def close(self):
        self.stream_release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(2)

    def count_path(self, path):
        with self.lock:
            return sum(call[1] == path for call in self.calls)

    def subscription(self, kind, uid=101):
        """Read-only snapshot of a real remote subscription for assertions."""
        with self.lock:
            return next((dict(row) for row in self.subscriptions.values()
                         if row["type"] == kind and row["condition"]["broadcaster_user_id"] == str(uid)), None)
