#!/usr/bin/env python3
"""Measure real Rust+SQLite and original Python bot+API+Mongo process RSS.

An already-built Rust service and both original Python dependency environments are
required. The Docker mongo image must already exist; this script never pulls it.
All data/providers are synthetic and local. Original source trees are read-only.

The original stack keeps its TwitchIO IRC transport and 60-second stream poll; the
Rust stack uses the approved EventSub conduit/WebSocket plus hybrid reconciliation.
Both carry the identical scheduled request count, message text, settings and public
HTTP workload, so the memory comparison reflects the intentional transport design
difference rather than a different amount of work.

Example (from repository root):
  scripts/memory.py --binary target/release/ronnia \
    --bot-source ../ronnia-legacy --bot-python ../ronnia-legacy/.venv/bin/python \
    --backend-source ../ronnia-backend \
    --backend-python /tmp/ronnia-backend-baseline/bin/python \
    --output scripts/memory-results.json

RSS sums include application descendants and the actual dedicated MongoDB server,
not an import-only baseline. Sum RSS can double-count shared pages; PSS is also
recorded when /proc permissions allow it. Local fake providers and the measuring
Python process are excluded equally. SQLite runs inside the Rust process. This
30-request/2.25-second, 75-second sample is rate-safe for the original TwitchIO
acknowledgement window and the osu 1-request/second pacing, but it is not a
production capacity or peak-memory benchmark.
"""
from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import json
import os
from pathlib import Path
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time

from modern import EventSub, Providers
from smoke import (Browser, IRC, Service, authorize_bot, check, eventually,
                   free_port, install_signal_handlers, make_exports)


class Child:
    def __init__(self, command, env, log_path):
        self.command = [str(part) for part in command]
        self.path = log_path
        self.log = log_path.open("wb")
        self.process = subprocess.Popen(self.command, env=env, stdout=self.log,
                                        stderr=subprocess.STDOUT, start_new_session=True)

    def stop(self):
        if self.process.poll() is None:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=3)
        self.log.close()


def command_output(command, env=None, timeout=20):
    """Bound both the process and descendants, including timeout/error cleanup."""
    process = subprocess.Popen([str(item) for item in command], env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except BaseException:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        process.communicate(timeout=3)
        raise
    check(process.returncode == 0, f"Command failed ({process.returncode}): {command[0]}\n{stdout}\n{stderr}")
    return stdout.strip()


def proc_snapshot(groups):
    processes = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            stat = (entry / "stat").read_text()
            tail = stat[stat.rindex(")") + 2:].split()
            status = {}
            for line in (entry / "status").read_text().splitlines():
                key, _, value = line.partition(":")
                status[key] = value.strip()
            if "VmRSS" not in status:
                continue
            pss = None
            try:
                for line in (entry / "smaps_rollup").read_text().splitlines():
                    if line.startswith("Pss:"):
                        pss = int(line.split()[1]) * 1024
                        break
            except (OSError, ValueError):
                pass
            processes[int(entry.name)] = {"pid": int(entry.name), "ppid": int(tail[1]),
                                           "name": status.get("Name", "?"),
                                           "rss_bytes": int(status["VmRSS"].split()[0]) * 1024,
                                           "pss_bytes": pss}
        except (OSError, ValueError, IndexError):
            continue
    result = {}
    seen = set()
    for name, roots in groups.items():
        selected = set(roots)
        for root in roots:
            check(root in processes, f"Cannot measure live process {root} ({name}) through /proc")
        while True:
            children = {pid for pid, info in processes.items() if info["ppid"] in selected}
            if children <= selected:
                break
            selected.update(children)
        selected -= seen
        seen.update(selected)
        members = [processes[pid] for pid in sorted(selected) if pid in processes]
        result[name] = {"rss_bytes": sum(item["rss_bytes"] for item in members),
                        "pss_bytes": sum(item["pss_bytes"] for item in members)
                        if all(item["pss_bytes"] is not None for item in members) else None,
                        "processes": members}
    result["service_total_rss_bytes"] = sum(group["rss_bytes"] for group in result.values())
    return result


def summarize(samples):
    result = {}
    group_names = [name for name in samples[0]["memory"] if name != "service_total_rss_bytes"]
    for name in [*group_names, "service_total"]:
        values = [sample["memory"]["service_total_rss_bytes"] if name == "service_total"
                  else sample["memory"][name]["rss_bytes"] for sample in samples]
        result[name] = {"min_rss_bytes": min(values), "median_rss_bytes": int(statistics.median(values)),
                        "max_rss_bytes": max(values), "last_rss_bytes": values[-1]}
    return result


class IrcProtocol:
    """Transport adapter for the unchanged original TwitchIO IRC stack."""
    name = "IRC PRIVMSG with mandatory tags"

    def __init__(self, irc):
        self.irc = irc

    def inject(self, index, text, author, user_id):
        self.irc.inject(f"memory-request-{index}", text, author=author, user_id=user_id)

    def delivered(self, count):
        return len(self.irc.echoes()) >= count

    def joined(self):
        return self.irc.joined()

    def echoes(self):
        return len(self.irc.echoes())


class EventSubProtocol:
    """Transport adapter for the modern EventSub conduit chat notifications."""
    name = "EventSub channel.chat.message over conduit WebSocket"

    def __init__(self, eventsub):
        self.eventsub = eventsub

    def inject(self, index, text, author, user_id):
        self.eventsub.inject(f"memory-request-{index}", text, author=author, user_id=user_id)

    def delivered(self, count):
        return len(self.eventsub.echoes()) >= count

    def joined(self):
        return self.eventsub.joined()

    def echoes(self):
        return len(self.eventsub.echoes())


def workload(origin, protocol, providers, groups, processes, requests, duration, interval):
    browser = Browser(origin)
    eventually(lambda: browser.ok("/live/users?limit=100") == ["streamer"], "memory workload live HTTP route",
               timeout=45)
    eventually(protocol.joined, "memory workload real chat subscription", timeout=35)
    initial_posts = len(providers.posts)
    initial_echoes = protocol.echoes()
    # Warm both complete stacks with one actual delivered request before samples.
    protocol.inject("warmup", "https://osu.ppy.sh/beatmaps/1100", "memorywarmup", 9001)
    eventually(lambda: len(providers.posts) == initial_posts + 1, "memory warmup osu PM", timeout=25)
    eventually(lambda: protocol.echoes() == initial_echoes + 1, "memory warmup success echo", timeout=25)
    before_posts, before_echoes = len(providers.posts), protocol.echoes()
    started = time.monotonic()
    sent = 0
    http_requests = 0
    samples = []
    # Identical scheduled requests and public HTTP routes in both variants.
    while True:
        elapsed = time.monotonic() - started
        for process in processes:
            check(process.poll() is None, f"Application process exited during measurement ({process.returncode})")
        if sent < requests and elapsed >= sent * interval:
            protocol.inject(sent, f"https://osu.ppy.sh/beatmaps/{1200 + sent} +HD",
                            f"memoryviewer{sent}", 5000 + sent)
            sent += 1
        check(browser.ok("/user/me") is None, "Unexpected anonymous account in memory workload")
        check(browser.ok("/live/users?limit=100") == ["streamer"], "Memory workload lost live channel")
        http_requests += 2
        samples.append({"elapsed_secs": round(time.monotonic() - started, 6),
                        "memory": proc_snapshot(groups)})
        if elapsed >= duration:
            break
        time.sleep(0.2)
    check(sent == requests, "Measurement duration did not accommodate requested chat workload")
    eventually(lambda: len(providers.posts) == before_posts + requests, "all memory workload PMs", timeout=25)
    eventually(lambda: protocol.echoes() == before_echoes + requests, "all memory workload success echoes", timeout=25)
    check(not providers.errors, f"Local provider errors during workload: {providers.errors}")
    return {"summary": summarize(samples), "samples": samples,
            "workload": {"transport": protocol.name, "scheduled_chat_requests": requests,
                         "successful_osu_pms": len(providers.posts) - before_posts,
                         "success_echoes": protocol.echoes() - before_echoes,
                         "public_http_requests": http_requests, "warmup_requests": 1,
                         "sample_duration_secs": samples[-1]["elapsed_secs"],
                         "chat_interval_secs": interval}}


def write_fixture(root):
    directory = root / "exports"
    make_exports(directory)
    # Identical functional workload: original test=True bypasses its known policy
    # defects, not the parser, provider calls, delivery, echo or real statistics.
    path = directory / "Users.json"
    users = json.loads(path.read_text())
    users[0]["settings"]["test"] = True
    path.write_text(json.dumps(users))
    return directory


def measure_rust(args, root):
    root.mkdir()
    providers = Providers()
    providers.bot_expires_in = 3600
    eventsub = EventSub(providers)
    providers.eventsub = eventsub
    service = None
    try:
        eventsub.start()
        service = Service(args.binary, root, providers, eventsub)
        directory = write_fixture(root)
        command_output(service.command("import", "--directory", directory), env=service.env)
        authorize_bot(service)
        service.start()
        # Real channel:bot consent through the API, never a fabricated grant row.
        Browser(service.origin).oauth("twitch", 101)
        groups = {"application": [service.process.pid], "external_database": [],
                  "additional_external": args.extra_pid}
        protocol = EventSubProtocol(eventsub)
        result = workload(service.origin, protocol, providers, groups, [service.process],
                          args.requests, args.seconds, args.interval)
        check(service.scalar("SELECT count(*) FROM statistics") == 3 + 1 + args.requests,
              "Rust workload did not persist all real request events")
        result["database"] = {"engine": "SQLite", "location": "in-process",
                              "request_events": 3 + 1 + args.requests}
        result["reconciliation"] = {"interval_secs": int(service.env["RECONCILE_INTERVAL_SECS"]),
                                    "transport": "EventSub conduit WebSocket"}
        return result
    except BaseException:
        if service:
            service.stop()
            service.print_logs()
        raise
    finally:
        if service:
            service.stop()
        eventsub.close()
        providers.close()


def measure_python(args, root):
    root.mkdir()
    providers = Providers(baseline=True)
    irc = IRC()
    providers.irc = irc
    irc.start()
    children = []
    container = None
    try:
        directory = write_fixture(root)
        # An exclusively owned Mongo container: original hardcoded Ronnia DB name
        # cannot touch a user's Mongo service or any preexisting database.
        container = f"ronnia-memory-{os.getpid()}-{time.monotonic_ns()}"
        command_output(["docker", "run", "--name", container, "--rm", "--detach", "--pull", "never",
                        "--publish", "127.0.0.1::27017", args.mongo_image,
                        "--bind_ip_all", "--wiredTigerCacheSizeGB", "0.25"], timeout=30)
        mongo_pid = int(command_output(["docker", "inspect", "--format", "{{.State.Pid}}", container]))
        address = command_output(["docker", "port", container, "27017/tcp"]).splitlines()[0]
        api_port = free_port()
        origin = f"http://127.0.0.1:{api_port}"
        env = {name: os.environ[name] for name in ("PATH", "HOME", "SYSTEMROOT", "TMPDIR") if name in os.environ}
        env.update({"MONGODB_URL": f"mongodb://{address}/?directConnection=true",
                    "TWITCH_CLIENT_ID": "synthetic-twitch", "TWITCH_CLIENT_SECRET": "synthetic-secret",
                    "OSU_CLIENT_ID": "123", "OSU_CLIENT_SECRET": "synthetic-secret",
                    "TMI_TOKEN": "oauth:synthetic", "BOT_NICK": "smokebot", "SENTRY_DSN": "",
                    "JWT_SECRET_KEY": "synthetic-local-memory-only-secret", "LOG_LEVEL": "INFO",
                    "PYTHONDONTWRITEBYTECODE": "1", "PYTHONUNBUFFERED": "1",
                    "OSU_REDIRECT_URI": origin + "/oauth2/osu-redirect",
                    "TWITCH_REDIRECT_URI": origin + "/oauth2/twitch-redirect",
                    "BASELINE_API_PORT": str(api_port), "SMOKE_PROVIDER_ORIGIN": providers.origin})
        launcher = Path(__file__).with_name("baseline.py").resolve()
        # Seed waits for Mongo readiness inside bounded pymongo server selection.
        seed_command = [args.bot_python, launcher, "seed", "--exports", directory]
        last_error = None
        deadline = time.monotonic() + 30
        while True:
            try:
                command_output(seed_command, env=env, timeout=6)
                break
            except AssertionError as exc:
                last_error = exc
                if time.monotonic() >= deadline:
                    raise AssertionError(f"Mongo readiness/seed failed: {last_error}")
                time.sleep(0.3)
        bot = Child([args.bot_python, launcher, "bot", "--source", args.bot_source], env, root / "python-bot.log")
        children.append(bot)
        backend = Child([args.backend_python, launcher, "backend", "--source", args.backend_source],
                        env, root / "python-api.log")
        children.append(backend)
        browser = Browser(origin)
        eventually(lambda: browser.request("/user/me")[0] == 200, "original backend readiness", timeout=35)
        groups = {"application": [bot.process.pid, backend.process.pid], "external_database": [mongo_pid],
                  "additional_external": args.extra_pid}
        protocol = IrcProtocol(irc)
        result = workload(origin, protocol, providers, groups, [child.process for child in children],
                          args.requests, args.seconds, args.interval)
        counts = json.loads(command_output([args.bot_python, launcher, "counts"], env=env))
        check(counts["Statistics"] == 3 + 1 + args.requests,
              f"Original workload did not persist every request event: {counts}")
        result["database"] = {"engine": "MongoDB", "image": args.mongo_image,
                              "wired_tiger_cache_gb": 0.25, "host_pid": mongo_pid,
                              "request_events": counts["Statistics"]}
        result["interpreters"] = {"bot": command_output([args.bot_python, "--version"]),
                                  "backend": command_output([args.backend_python, "--version"])}
        return result
    except BaseException:
        for child in children:
            child.stop()
            print(f"\n--- {child.path.name} ---\n{child.path.read_text(errors='replace')[-24000:]}", file=sys.stderr)
        print(f"Baseline provider errors: {providers.errors}", file=sys.stderr)
        print(f"Baseline IRC connections: {irc.connections}", file=sys.stderr)
        raise
    finally:
        for child in children:
            child.stop()
        irc.close()
        providers.close()
        if container:
            command_output(["docker", "rm", "--force", container], timeout=15)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", default="target/release/ronnia")
    parser.add_argument("--bot-source", type=Path, default=Path("../ronnia-legacy"))
    parser.add_argument("--bot-python", type=Path, default=Path("../ronnia-legacy/.venv/bin/python"))
    parser.add_argument("--backend-source", type=Path, default=Path("../ronnia-backend"))
    parser.add_argument("--backend-python", type=Path, default=Path("/tmp/ronnia-backend-baseline/bin/python"))
    parser.add_argument("--mongo-image", default="mongo:7")
    parser.add_argument("--seconds", type=float, default=75.0)
    parser.add_argument("--requests", type=int, default=30)
    parser.add_argument("--interval", type=float, default=2.25)
    parser.add_argument("--extra-pid", type=int, action="append", default=[],
                        help="also measure an explicitly supplied external service + descendants in both variants")
    parser.add_argument("--output", type=Path, default=Path("scripts/memory-results.json"))
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    install_signal_handlers()
    check(sys.platform.startswith("linux"), "This RSS harness requires Linux /proc")
    check(args.requests > 0 and args.interval >= 1.1 and args.seconds >= args.requests * args.interval + 2,
          "Use requests>0, interval>=1.1 and seconds>=requests*interval+2 for both real implementations")
    for field in ("binary", "bot_python", "backend_python", "bot_source", "backend_source"):
        # Resolving the executable symlink escapes pyvenv.cfg and silently runs
        # the base interpreter without the original application's dependencies.
        value = Path(os.path.abspath(getattr(args, field)))
        check(value.exists(), f"Missing prerequisite --{field.replace('_', '-')}: {value}")
        setattr(args, field, value)
    command_output(["docker", "image", "inspect", args.mongo_image])
    root = Path(tempfile.mkdtemp(prefix="ronnia-memory-"))
    report = {"schema_version": 1, "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
              "status": "running", "metric": "Linux VmRSS, sampled after full-stack warmup",
              "rss_caveat": "Summed RSS can double-count shared pages; inspect per-process RSS/PSS samples.",
              "scope": "Actual original bot + original FastAPI backend + dedicated MongoDB versus actual Rust + in-process SQLite",
              "excluded_equally": ["measurement process", "local fake HTTP providers", "local IRC/WebSocket test servers", "Docker daemon"],
              "source_directories": {"bot": str(args.bot_source), "backend": str(args.backend_source)},
              "runtime_redirections": ["aiohttp HTTP destination URLs only", "TwitchIO WebSocket destination only"],
              "fixture": "Same four generated Mongo exports; one linked user; test=true in both; three historical rows/maps",
              "workload": {"scheduled_chat_requests": args.requests, "interval_secs": args.interval,
                           "duration_secs": args.seconds, "warmup_requests": 1,
                           "protocol_difference": "original TwitchIO IRC + 60s poll versus EventSub conduit WebSocket + hybrid reconciliation"},
              "measurement_limits": ["small synthetic workload, not production capacity", "no TLS", "one run per implementation",
                                     "Mongo WiredTiger cache configured to 0.25 GiB", "sampled maximum is not exact process peak"]}
    code = 0
    try:
        report["python"] = measure_python(args, root / "python")
        report["rust"] = measure_rust(args, root / "rust")
        old = report["python"]["summary"]["service_total"]["median_rss_bytes"]
        new = report["rust"]["summary"]["service_total"]["median_rss_bytes"]
        report["comparison"] = {"python_service_median_rss_bytes": old, "rust_service_median_rss_bytes": new,
                                "rust_over_python_ratio": new / old}
        report["status"] = "complete"
        print(f"Measured service median RSS: Python+Mongo {old / 1048576:.2f} MiB; Rust+SQLite {new / 1048576:.2f} MiB")
        print("Small local synthetic workload only; see JSON for full process samples and caveats.")
    except BaseException as exc:
        report["status"] = "failed"
        report["error"] = str(exc)
        print(f"Memory comparison failed: {exc}", file=sys.stderr)
        code = 1
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
        print(f"Measurement report: {args.output.resolve()} ({report['status']})")
        if args.keep:
            print(f"Artifacts: {root}")
        else:
            shutil.rmtree(root)
    return code


if __name__ == "__main__":
    sys.exit(main())
