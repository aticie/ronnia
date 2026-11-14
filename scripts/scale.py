#!/usr/bin/env python3
"""Isolated single-replica proof using real HTTP/RFC6455 fixtures, not production capacity.

Requires an already-built binary, Python's standard library, and (on Linux) cc for
process-local reconnect DNS injection. Provider pacing is deliberately unchanged.
Private scheduler/reader state is asserted by Rust tests, never inferred here.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import shutil
import sqlite3
import sys
import tempfile
import threading
import time
import urllib.request

from modern import EventSub, Providers, beatmap, check, timestamp
from smoke import (Browser, Service, authorize_bot, connect, eventually,
                   install_signal_handlers, run_captured, settings)


class Proof:
    def __init__(self, binary, root):
        root.mkdir(parents=True, exist_ok=True)
        self.providers = Providers()
        self.ws = EventSub(self.providers)
        self.providers.eventsub = self.ws
        self.service = Service(binary, root, self.providers, self.ws)
        self.service.env.update(EVENTSUB_CHANNELS_PER_SHARD="2", EVENTSUB_MAX_SHARDS="3",
                                REQUEST_WORKERS="4", REQUEST_QUEUE_CAPACITY="32",
                                HTTP_TIMEOUT_SECS="30", RECONCILE_INTERVAL_SECS="600")
        self.browsers = {}
        self.injected = []
        self.latencies = []
        self.peak_rss_kib = None
        self.monitor_stop = threading.Event()
        self.probe_active = threading.Event()
        self.probe_lock = threading.Lock()
        self.probe_process = None
        self.monitor = None
        self.dns_arm = None
        self.started_at = time.monotonic()
        self.results = {}

    def log(self):
        return self.service.logs[-1].read_text(errors="replace")

    def seed(self, count, grant_count=5):
        result = run_captured(self.service.command("check"), env=self.service.env)
        check(result.returncode == 0, f"Fresh SQLite check failed: {result.stdout}")
        with connect(self.service.database) as connection:
            for uid in range(101, 101 + count):
                login = self.login(uid)
                target = 10000 + uid
                connection.execute(
                    "INSERT INTO users(osu_id,osu_username,osu_avatar_url,twitch_id,twitch_username,"
                    "twitch_avatar_url,is_live,settings_json,raw_json) VALUES(?,?,?,?,?,?,0,?,NULL)",
                    (target, f"osu{target}", "", uid, login, "",
                     json.dumps(settings(echo=False))))
                self.providers.pm_targets.add(target)
                self.providers.live[uid] = login
                self.providers.categories[uid] = "21465"
            connection.execute(
                "INSERT INTO beatmaps(id,beatmapset_id,fetched_at,raw_json) VALUES(1001,500,?,?)",
                (int(time.time()), json.dumps(beatmap(1001))))
        authorize_bot(self.service)
        self.ws.start()
        self.service.start(api_only=True)
        for uid in range(101, 101 + grant_count):
            self.grant(uid)
        self.stop()

    @staticmethod
    def login(uid):
        return "streamer" if uid == 101 else f"streamer{uid}"

    def grant(self, uid):
        browser = self.browsers.setdefault(uid, Browser(self.service.origin))
        browser.oauth("twitch", uid)
        return browser

    def start(self):
        self.service.start()
        with self.probe_lock:
            self.probe_process = self.service.process
            self.probe_active.set()
        if self.monitor is None:
            self.monitor = threading.Thread(target=self.monitor_api, daemon=True)
            self.monitor.start()

    def stop(self):
        with self.probe_lock:
            self.probe_active.clear()
            self.probe_process = None
        code = self.service.stop(close_fixture=False)
        check(code in (None, 0), f"Service needed forced shutdown or exited unsuccessfully: {code}")
        eventually(lambda: not self.ws.alive(), "all fixture sockets closed by service shutdown", timeout=5)

    def monitor_api(self):
        # Probe independently of the OAuth browser cookie jar and fixture stalls.
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        while not self.monitor_stop.wait(.2):
            with self.probe_lock:
                process = self.probe_process if self.probe_active.is_set() else None
            if process is None or process.poll() is not None:
                continue
            try:
                status = Path(f"/proc/{process.pid}/status").read_text()
                for line in status.splitlines():
                    if line.startswith("VmHWM:"):
                        value = int(line.split()[1])
                        self.peak_rss_kib = max(self.peak_rss_kib or 0, value)
            except (OSError, ValueError):
                pass
            started = time.monotonic()
            path = "/ready" if len(self.latencies) % 2 == 0 else "/live/users?limit=100"
            try:
                with opener.open(self.service.origin + path, timeout=5) as response:
                    check(response.status == 200, f"{path} failed under synthetic load")
                    response.read()
                with self.probe_lock:
                    if self.probe_active.is_set() and self.probe_process is process:
                        self.latencies.append(time.monotonic() - started)
            except Exception as exc:
                with self.probe_lock:
                    if (self.probe_active.is_set() and self.probe_process is process
                            and self.service.process is process and process.poll() is None):
                        self.results.setdefault("api_errors", []).append(f"{path}: {exc!r}")

    def topology(self, count):
        return (len(self.providers.conduits) == 1 and
                next(iter(self.providers.conduits.values()))["shard_count"] == count and
                all(self.ws.shard_number(shard) is not None for shard in range(count)))

    def converge(self, channels, timeout=180):
        eventually(lambda: len(self.providers.subscriptions) == channels * 4,
                   f"all {channels * 4} paced owned subscriptions (including continuation passes)", timeout)
        eventually(lambda: self.service.scalar("SELECT count(*) FROM users WHERE is_live=1") == channels,
                   "all granted live channels corrected by snapshots", timeout=30)

    def send(self, request_id, uid=101, number=None, user_id=None):
        self.injected.append(request_id)
        self.ws.inject(request_id, "https://osu.ppy.sh/beatmaps/1001", author=request_id,
                       user_id=user_id or 50000 + len(self.injected), uid=uid, number=number)

    def status(self, request_id):
        return self.service.scalar("SELECT status FROM deliveries WHERE twitch_message_id=?", (request_id,))

    def sent(self, request_id):
        return self.status(request_id) == "sent"

    def pm_count(self, request_id):
        return sum(request_id + " ->" in row["message"] for row in self.providers.posts)

    def stats(self, request_id):
        return self.service.scalar("SELECT count(*) FROM statistics WHERE source_id=?", ("twitch:" + request_id,))

    def delivered(self, request_id, uid=101, number=None):
        self.send(request_id, uid, number)
        eventually(lambda: self.sent(request_id), f"successful PM/statistic {request_id}", timeout=30)
        check(self.pm_count(request_id) == self.stats(request_id) == 1, f"Duplicate/missing PM/statistic: {request_id}")

    def reconnect_dns(self):
        if not sys.platform.startswith("linux"):
            self.results["dns"] = "not covered: Linux LD_PRELOAD unavailable"
            return
        # Use the existing resolver fixture, but arm its two failures only after
        # all initial sockets attach. Nothing touches host resolver configuration.
        source = Path(__file__).with_name("fixtures") / "resolver.c"
        text = source.read_text().replace("#include <unistd.h>", "#include <unistd.h>\n#include <stdlib.h>")
        text = text.replace('if (attempt < 2) {',
                            'if (getenv("RONNIA_DNS_ARM") && access(getenv("RONNIA_DNS_ARM"), F_OK) == 0 && attempt < 2) {')
        text = text.replace('unsigned int attempt = atomic_fetch_add_explicit(&attempts, 1, memory_order_relaxed);',
                            'unsigned int attempt = 2;\n        if (getenv("RONNIA_DNS_ARM") && access(getenv("RONNIA_DNS_ARM"), F_OK) == 0)\n'
                            '            attempt = atomic_fetch_add_explicit(&attempts, 1, memory_order_relaxed);')
        generated = self.service.root / "armed-resolver.c"
        generated.write_text(text)
        library = self.service.root / "armed-resolver.so"
        result = run_captured(["cc", "-shared", "-fPIC", "-std=c11", "-O2", str(generated), "-o", str(library), "-ldl"])
        check(result.returncode == 0, f"Cannot build reconnect DNS fixture: {result.stdout}")
        self.dns_arm = self.service.root / "dns-armed"
        self.ws.url = f"ws://recover.localhost:{self.ws.port}/ws"
        self.service.env.update(LD_PRELOAD=str(library), TWITCH_EVENTSUB_URL=self.ws.url,
                                RONNIA_DNS_ARM=str(self.dns_arm))

    def lifecycle(self):
        self.start()
        eventually(lambda: self.topology(3), "five grants grow one conduit to three attached shards", 40)
        self.converge(5)
        conduit = next(iter(self.providers.conduits))
        check([count for _, count, _ in self.providers.topology_history] == [2, 3], "Growth skipped a shard or mutated twice")
        for uid in (106, 107):
            self.grant(uid)
        self.converge(7)
        check(self.topology(3), "Configured cap was exceeded")
        self.browsers[107].ok("/user/settings", "POST", settings(enable=False, echo=False))
        eventually(lambda: len(self.providers.subscriptions) == 24, "disabled grant subscriptions removed", 30)
        check(self.topology(3), "Conduit shrank automatically")
        self.browsers[107].ok("/user/settings", "POST", settings(echo=False))
        self.converge(7)
        before = dict(self.providers.subscription_creates)
        self.stop()
        self.start()
        eventually(lambda: self.topology(3), "persisted three-shard conduit reattached after restart", 30)
        self.converge(7)
        check(next(iter(self.providers.conduits)) == conduit and dict(self.providers.subscription_creates) == before,
              "Restart created another conduit or duplicate subscriptions")
        # Observed topology is authoritative even if an operator lowers the growth cap.
        self.stop()
        self.service.env["EVENTSUB_MAX_SHARDS"] = "1"
        self.start()
        eventually(lambda: self.topology(3), "all remote shards attached above configured cap", 30)
        eventually(lambda: "cap" in self.log().lower(), "configuration-cap mismatch diagnostic", 15)
        self.service.env["EVENTSUB_MAX_SHARDS"] = "3"
        self.results["lifecycle"] = "grow3, cap, no shrink, persisted reuse, lower-cap remote attach"

    def transport(self):
        healthy = self.ws.shard_number(1)
        old = self.ws.shard_number(0)
        if self.dns_arm is not None:
            self.dns_arm.touch()
        self.ws.drop(0)
        self.ws.state("channel.update", category="21465", uid=103)
        self.delivered("healthy_during_reconnect", uid=103)
        check(self.ws.shard_number(1) == healthy, "Healthy peer restarted after another shard failed")
        if self.dns_arm is not None:
            eventually(lambda: self.log().count("RONNIA_SMOKE_DNS_FAILURE") == 2,
                       "two process-local DNS failures on reconnect", 30)
        if self.dns_arm is not None:
            self.dns_arm.unlink()
        eventually(lambda: self.ws.shard_number(0) not in (None, old), "failed shard reattached with bounded backoff", 35)
        # An overlapping transport notification and a distinct transport envelope
        # carrying the same durable chat ID must both result in only one PM.
        self.send("durable_overlap", uid=103, number=self.ws.shard_number(0))
        self.send("durable_overlap", uid=103, number=healthy)
        subscription = self.ws.subscription("channel.chat.message", 103)
        event = {"broadcaster_user_id": "103", "broadcaster_user_login": "streamer103",
                 "broadcaster_user_name": "streamer103", "chatter_user_id": "88001",
                 "chatter_user_login": "durable_overlap", "chatter_user_name": "durable_overlap",
                 "message_id": "durable_overlap", "message": {"text": "https://osu.ppy.sh/beatmaps/1001", "fragments": []},
                 "badges": [], "message_type": "text"}
        self.injected.append("durable_overlap")
        self.ws.envelope(healthy, "notification", {"subscription": subscription, "event": event}, subscription)
        eventually(lambda: self.sent("durable_overlap"), "overlap duplicate completed", 20)
        check(self.pm_count("durable_overlap") == self.stats("durable_overlap") == 1, "Overlap duplicate delivered twice")
        old = self.ws.handoff(0)
        eventually(self.ws.handoff_opened.is_set, "make-before-break candidate opened", 10)
        candidate = self.ws.handoff_candidate
        check(candidate is not None and candidate > old, "Handoff did not open a new candidate socket")
        self.delivered("handoff_old_live", uid=102, number=old)
        self.delivered("handoff_peer_live", uid=103)
        self.ws.delay_welcome.set()
        eventually(lambda: self.ws.clients[candidate]["alive"] and self.ws.clients[candidate]["welcomed"]
                   and self.ws.shard_number(0) == candidate, "exact live handoff candidate attached", 15)
        self.delivered("handoff_new_live", uid=102, number=candidate)
        eventually(lambda: not self.ws.clients[old]["alive"], "retired handoff socket joined/closed", 10)
        check(self.ws.shard_number(1) == healthy, "Handoff restarted an unrelated peer")
        # Foreign-origin reconnect URLs must never receive a connection.
        current = self.ws.shard_number(0)
        self.ws.envelope(current, "session_reconnect", {"session": {"id": self.ws.clients[current]["session"],
                         "status": "reconnecting", "keepalive_timeout_seconds": None,
                         "reconnect_url": self.ws.url.replace("/ws", "/forbidden").replace("recover.localhost", "localhost").replace("127.0.0.1", "localhost") + "?handoff=1",
                         "connected_at": timestamp()}})
        time.sleep(1)
        check(not any("/forbidden" in client["path"] for client in self.ws.clients.values()),
              "Foreign-origin reconnect URL was followed")
        eventually(lambda: self.ws.shard_number(0) is not None, "shard survives invalid reconnect URL", 20)
        # Wrong exact shard row must reject that session and leave peers intact.
        self.ws.wrong_shard_responses = 1
        old = self.ws.shard_number(2)
        history = len(self.ws.attach_history)
        self.ws.drop(2)
        eventually(lambda: len(self.ws.attach_history) > history, "wrong shard response exercised", 20)
        rejected_session = self.ws.attach_history[history][1]
        eventually(lambda: self.ws.shards.get(2) != rejected_session and self.ws.shard_number(2) is not None,
                   "wrong shard response rejected and recovered", 30)
        check(self.ws.shard_number(1) == healthy, "Bad shard attachment restarted healthy peer")
        # Each shard gets its own welcome-relative nine-second attachment budget.
        for shard in (0, 2):
            history = len(self.ws.attach_history)
            self.ws.attach_delay[shard] = 9.5
            self.ws.drop(shard)
            eventually(lambda: len(self.ws.attach_history) > history, f"shard {shard} deadline fixture reached", 25)
            late = self.ws.attach_history[history][1]
            eventually(lambda: self.ws.shards.get(shard) != late and self.ws.shard_number(shard) is not None,
                       f"shard {shard} late attach rejected/retried independently", 30)
            check(self.ws.shard_number(1) == healthy, "Per-shard attach deadline reset healthy peer")
        self.results["transport"] = "isolated reconnect/DNS, durable overlap, handoff, origin, wrong row, per-shard deadline"

    def subscriptions_stall(self, method, uid=None):
        p = self.providers
        p.subscription_stall_method = method
        p.subscription_release.clear()
        p.subscription_entered.clear()
        if method == "GET":
            self.stop()
            self.start()
            eventually(lambda: self.topology(3), "existing shards attach before stalled subscription listing", 30)
        else:
            self.grant(uid)
        eventually(p.subscription_entered.is_set, f"{method} subscription HTTP stall entered", 20)
        sockets = {shard: self.ws.shard_number(shard) for shard in range(3)}
        started = time.monotonic()
        self.delivered("stall_" + method.lower(), uid=103)
        self.ws.state("stream.offline", uid=101)
        eventually(lambda: self.service.scalar("SELECT is_live FROM users WHERE twitch_id=101") == 0,
                   "offline notification dispatched during subscription HTTP stall", 3)
        before = len(p.posts)
        self.send("offline_during_" + method.lower(), uid=101)
        time.sleep(max(0, 12 - (time.monotonic() - started)))
        check(len(p.posts) == before, "Offline channel remained eligible during maintenance stall")
        check({shard: self.ws.shard_number(shard) for shard in range(3)} == sockets,
              "Healthy sockets spuriously reconnected during twelve-second HTTP stall")
        p.subscription_stall_method = None
        p.subscription_release.set()
        self.ws.state("stream.online", uid=101)
        eventually(lambda: self.service.scalar("SELECT is_live FROM users WHERE twitch_id=101") == 1,
                   "online scheduling resumes after stalled operation", 30)

    def stale_snapshots(self):
        p = self.providers
        p.stream_release.clear()
        p.stream_entered.clear()
        # Keep membership/subscriptions stable: a fresh online notification is
        # enough to schedule a revision-guarded targeted lookup. An enable toggle
        # could legitimately remove this subscription before the notification.
        self.ws.state("stream.online", uid=102)
        eventually(p.stream_entered.is_set, "targeted HTTP captured pre-offline state", 20)
        self.ws.state("stream.offline", uid=102)
        eventually(lambda: self.service.scalar("SELECT is_live FROM users WHERE twitch_id=102") == 0,
                   "offline consumed while earlier HTTP is blocked", 3)
        # More than the bounded 1024 targeted-action capacity, while maintenance
        # cannot drain it. Public proof checks final state, not internal length.
        revision = self.service.scalar("SELECT revision FROM channel_stream_state WHERE twitch_id=102")
        for index in range(1100):
            self.ws.state("stream.online" if index % 2 == 0 else "stream.offline",
                          uid=102, at=timestamp((index + 1) * .001))
        eventually(lambda: self.service.scalar("SELECT revision FROM channel_stream_state WHERE twitch_id=102") >= revision + 1100,
                   "all ordered targeted-pressure notifications consumed before HTTP release", 20)
        check(self.service.scalar("SELECT online FROM channel_stream_state WHERE twitch_id=102") == 0,
              "Final offline notification lost under targeted pressure")
        p.live.pop(102, None)
        p.stream_release.set()
        time.sleep(2)
        check(self.service.scalar("SELECT is_live FROM users WHERE twitch_id=102") == 0,
              "Late HTTP/overflowed Cancel resurrected stale stream state")
        self.send("late_snapshot_offline", uid=102)
        time.sleep(1)
        check(self.status("late_snapshot_offline") is None, "Stale snapshot restored request eligibility")
        p.live[102] = self.login(102)
        self.ws.state("stream.online", uid=102)
        eventually(lambda: self.service.scalar("SELECT is_live FROM users WHERE twitch_id=102") == 1,
                   "fresh state correction after overflow", 30)
        self.results["maintenance"] = "12s GET/POST stalls, foreground PM/offline, stale HTTP and targeted pressure"

    def scheduler(self):
        p = self.providers
        a, b = 101, 102
        target_a = 10000 + a
        p.pm_hold_targets = {target_a}
        p.pm_release.clear()
        p.pm_entered.clear()
        self.send("serial_a1", a)
        eventually(p.pm_entered.is_set, "A1 PM accepted with response held", 20)
        self.send("serial_a2", a)
        self.send("parallel_b", b)
        eventually(lambda: self.sent("parallel_b"), "channel B PM completes before held A1 (global pacer unchanged)", 15)
        check(not self.sent("serial_a1") and self.pm_count("serial_a2") == 0,
              "Same-channel A2 started before A1 outcome/cooldown transaction")
        p.pm_hold_targets.clear()
        p.pm_release.set()
        eventually(lambda: self.sent("serial_a1") and self.sent("serial_a2"), "channel A FIFO drains", 20)
        self.browsers[a].ok("/user/settings", "POST", settings(cooldown=60, echo=False))
        self.send("cooldown_first", a, user_id=99001)
        self.send("cooldown_second", a, user_id=99001)
        eventually(lambda: self.sent("cooldown_first"), "first cooldown request committed", 15)
        time.sleep(1.5)
        check(self.pm_count("cooldown_second") == 0 and self.stats("cooldown_second") == 0,
              "Same-channel cooldown evaluation raced")
        self.browsers[a].ok("/user/settings", "POST", settings(echo=False))
        p.ambiguous = True
        self.send("ambiguous_once", b)
        eventually(lambda: self.status("ambiguous_once") == "uncertain", "accepted ambiguous PM retained as uncertain", 20)
        self.send("ambiguous_once", b)
        time.sleep(1)
        check(self.pm_count("ambiguous_once") == 1 and self.stats("ambiguous_once") == 0,
              "Ambiguous PM replayed or counted successful")
        p.pm_failures = 1
        self.send("definite_failure", b)
        eventually(lambda: self.status("definite_failure") == "failed", "definite PM failure retained without success", 20)
        self.send("definite_failure", b)
        time.sleep(1)
        check(len(p.pm_rejected) == 1 and self.pm_count("definite_failure") == self.stats("definite_failure") == 0,
              "Definitely failed PM replayed or counted as successful")
        self.results["scheduler"] = "cross-channel progress, same-channel FIFO/cooldown, failed/ambiguous non-replay"

    def batch(self, label, amount, channels, overflow=False):
        p = self.providers
        initial_overflow = self.log().count("request queue is full; incoming request rejected")
        start = time.monotonic()
        ids = [f"{label}_{index:04d}" for index in range(amount)]
        if overflow:
            p.pm_hold_targets = set(p.pm_targets)
            p.pm_release.clear()
            p.pm_entered.clear()
        for index, request_id in enumerate(ids):
            self.send(request_id, 101 + index % channels)
        if overflow:
            eventually(p.pm_entered.is_set, "saturation requests hold active provider calls", 20)
            eventually(lambda: self.log().count("request queue is full; incoming request rejected") - initial_overflow == amount - 32,
                       "every request beyond 32 permits promptly rejected before held jobs release", 5)
            p.pm_hold_targets.clear()
            p.pm_release.set()
        def complete():
            rejected = self.log().count("request queue is full; incoming request rejected") - initial_overflow
            return sum(self.sent(request_id) for request_id in ids) + rejected == amount
        eventually(complete, f"every admitted {label} request terminates and rejected count balances", max(90, amount * 3))
        rejected = self.log().count("request queue is full; incoming request rejected") - initial_overflow
        completed = sum(self.sent(request_id) for request_id in ids)
        duplicate_pms = sum(max(0, self.pm_count(request_id) - 1) for request_id in ids)
        duplicate_stats = sum(max(0, self.stats(request_id) - 1) for request_id in ids)
        check(duplicate_pms == duplicate_stats == 0, "Batch duplicated PMs/statistics")
        check(sum(self.stats(request_id) for request_id in ids) == completed, "Batch success bookkeeping mismatch")
        check(self.service.scalar("SELECT count(*) FROM deliveries WHERE status IN ('pending','sending')") == 0,
              "Admitted requests did not terminate")
        if overflow:
            check(completed == 32 and rejected == amount - 32, f"Expected 32 total admitted permits: {completed}/{rejected}")
        else:
            check(rejected == 0, "Second drained batch unexpectedly rejected")
        self.results[label] = dict(injected=amount, completed=completed, rejected=rejected,
                                   duplicate_pms=duplicate_pms, duplicate_statistics=duplicate_stats,
                                   elapsed_seconds=round(time.monotonic() - start, 3))

    def load(self, channels, requests):
        active = channels
        shards = min(3, max(1, (active + 1) // 2))
        started = time.monotonic()
        self.start()
        self.converge(active, timeout=180)
        check(self.topology(shards), "Workload topology did not match configured heuristic/cap")
        self.results.update(initial_subscriptions=active * 4,
                            initial_convergence_seconds=round(time.monotonic() - started, 3))
        # Saturate deterministically even for a small --requests invocation.
        self.batch("saturation", max(33, requests), active, overflow=True)
        self.batch("second_batch", min(16, requests), active)
        # Durable IDs are still reserved after process restart, not just deduped in memory.
        before = len(self.providers.posts)
        self.stop()
        self.start()
        eventually(lambda: self.topology(shards), "restart after workload", 30)
        self.send("saturation_0000", uid=101)
        time.sleep(2)
        check(len(self.providers.posts) == before, "Restart replayed durable successful delivery")
        self.delivered("post_restart_new", uid=101)
        self.results["channels"] = active

    def finish(self):
        self.stop()
        self.monitor_stop.set()
        if self.monitor:
            self.monitor.join(6)
        check(not self.results.get("api_errors"), f"API probes failed: {self.results.get('api_errors')}")
        check(self.latencies and max(self.latencies) < 5, "API probe exceeded five seconds")
        check(not self.providers.errors, f"Provider fixture protocol errors: {self.providers.errors}")
        check(all(self.pm_count(request_id) <= 1 and self.stats(request_id) <= 1 for request_id in set(self.injected)),
              "Duplicate PM/statistic across scenario boundaries")
        check(self.service.scalar("SELECT count(*) FROM deliveries WHERE status IN ('pending','sending')") == 0,
              "A durable admitted request did not reach a terminal outcome")
        self.results.update(api_probes=len(self.latencies), api_max_seconds=round(max(self.latencies), 4),
                            elapsed_seconds=round(time.monotonic() - self.started_at, 3),
                            linux_peak_rss_kib=self.peak_rss_kib,
                            injected_notifications=len(self.injected), unique_request_ids=len(set(self.injected)),
                            completed=self.service.scalar("SELECT count(*) FROM deliveries WHERE status='sent'"),
                            uncertain=self.service.scalar("SELECT count(*) FROM deliveries WHERE status='uncertain'"),
                            failed=self.service.scalar("SELECT count(*) FROM deliveries WHERE status='failed'"),
                            rejected_overflow=sum(path.read_text(errors="replace").count(
                                "request queue is full; incoming request rejected") for path in self.service.logs),
                            pm_accepted=len(self.providers.posts), successful_statistics=self.service.scalar("SELECT count(*) FROM statistics"),
                            fixture_sockets_remaining=len(self.ws.alive()))
        return self.results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/ronnia")
    parser.add_argument("--channels", type=int, default=100)
    parser.add_argument("--requests", type=int, default=40)
    parser.add_argument("--keep", action="store_true", help="retain generated SQLite, logs and DNS fixture")
    args = parser.parse_args()
    if not (1 <= args.channels <= 2500 and args.requests > 0):
        parser.error("--channels must be 1..2500 and --requests must be positive")
    install_signal_handlers()
    root = Path(tempfile.mkdtemp(prefix="ronnia-scale-"))
    proofs = []
    proof = None
    try:
        proof = Proof(args.binary, root / "lifecycle")
        proofs.append(proof)
        proof.seed(8)
        proof.reconnect_dns()
        proof.lifecycle()
        proof.transport()
        proof.subscriptions_stall("GET")
        proof.subscriptions_stall("POST", 108)
        proof.converge(8)
        proof.stale_snapshots()
        proof.scheduler()
        # Also prove successful and ambiguous durable IDs survive restart.
        before = len(proof.providers.posts)
        proof.stop()
        proof.start()
        eventually(lambda: proof.topology(3), "restart before durable-ID replay", 30)
        proof.send("durable_overlap", uid=103)
        proof.send("ambiguous_once", uid=102)
        proof.send("definite_failure", uid=102)
        time.sleep(2)
        check(len(proof.providers.posts) == before, "Restart replayed successful/ambiguous durable ID")
        # Cancellation also closes a candidate still waiting for its welcome.
        proof.ws.handoff(0)
        eventually(proof.ws.handoff_opened.is_set, "candidate opened before shutdown cancellation", 10)
        lifecycle = proof.finish()
        # A fresh process/database gives a genuine 400-create initial convergence,
        # not a warm subscription cache. No request traffic precedes convergence.
        proof = Proof(args.binary, root / "workload")
        proofs.append(proof)
        proof.seed(args.channels, grant_count=args.channels)
        proof.load(args.channels, args.requests)
        print(json.dumps({"lifecycle": lifecycle, "workload": proof.finish()}, indent=2, sort_keys=True))
        print("PASS: synthetic local single-replica lifecycle/load; upstream provider pacing unchanged.")
        print("NOT EXTERNALLY ASSERTED: worker/permit/ready/active internals, membership-read failure edges, reader JoinHandles (Rust private tests).")
        print("NOT COVERED: production throughput, TLS/provider interoperability, container bot-mode load.")
        return 0
    except Exception as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        if proof is not None:
            proof.service.print_logs()
            if proof.providers.errors:
                print(f"Fixture errors: {proof.providers.errors}", file=sys.stderr)
        return 1
    finally:
        for item in proofs:
            item.monitor_stop.set()
            with item.probe_lock:
                item.probe_active.clear()
                item.probe_process = None
            item.providers.pm_release.set()
            item.providers.stream_release.set()
            item.providers.subscription_release.set()
            item.service.stop()
            if item.monitor:
                item.monitor.join(6)
            item.ws.close()
            item.providers.close()
        if args.keep:
            print(f"Artifacts: {root}")
        else:
            shutil.rmtree(root)


if __name__ == "__main__":
    sys.exit(main())
