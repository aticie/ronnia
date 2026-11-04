#!/usr/bin/env python3
"""Launch the unchanged original applications against isolated local providers.

Used by memory.py with each original application's actual dependency interpreter.
Only HTTP destination URLs and TwitchIO's WebSocket destination are redirected;
request parsing, policy, provider serialization, database clients, bot manager,
FastAPI routes, and framework behavior remain the original implementations.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import runpy
import sys
import urllib.parse


def redirect_network(origin):
    import aiohttp

    original_request = aiohttp.ClientSession._request
    allowed = urllib.parse.urlsplit(origin).netloc
    replacements = (("https://id.twitch.tv/oauth2", origin + "/twitch/oauth"),
                    ("https://api.twitch.tv/helix", origin + "/twitch/helix"),
                    ("https://osu.ppy.sh/oauth", origin + "/osu/oauth"),
                    ("https://osu.ppy.sh/api/v2", origin + "/osu/api/v2"))

    async def local_request(self, method, url, **kwargs):
        target = str(url)
        for source, replacement in replacements:
            if target == source or target.startswith(source + "/") or target.startswith(source + "?"):
                target = replacement + target[len(source):]
                break
        parsed = urllib.parse.urlsplit(target)
        if parsed.netloc != allowed or parsed.scheme not in ("http", "ws"):
            raise RuntimeError(f"Baseline blocked nonlocal provider destination: {parsed.scheme}://{parsed.netloc}")
        return await original_request(self, method, target, **kwargs)

    aiohttp.ClientSession._request = local_request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("seed", "counts", "bot", "backend"))
    parser.add_argument("--source", type=Path)
    parser.add_argument("--exports", type=Path)
    args = parser.parse_args()
    if args.mode in ("seed", "counts"):
        from pymongo import MongoClient
        from bson import json_util

        with MongoClient(os.environ["MONGODB_URL"], serverSelectionTimeoutMS=3000) as client:
            database = client["Ronnia"]
            if args.mode == "seed":
                for name in ("Users", "Settings", "Statistics", "Beatmaps"):
                    if database[name].count_documents({}):
                        raise RuntimeError("Refusing to seed a nonempty Mongo baseline database")
                    rows = json_util.loads((args.exports / f"{name}.json").read_text())
                    if rows:
                        database[name].insert_many(rows)
            print(json.dumps({name: database[name].count_documents({})
                              for name in ("Users", "Settings", "Statistics", "Beatmaps")}))
        return
    source = args.source.resolve()
    if not source.is_dir():
        raise RuntimeError(f"Original source directory missing: {source}")
    sys.path.insert(0, str(source))
    redirect_network(os.environ["SMOKE_PROVIDER_ORIGIN"])
    if args.mode == "bot":
        sys.path.insert(0, str(source / "ronnia"))
        import twitchio.websocket

        twitchio.websocket.HOST = os.environ["SMOKE_PROVIDER_ORIGIN"].replace("http://", "ws://", 1) + "/irc-websocket"
        runpy.run_module("ronnia.main", run_name="__main__")
    else:
        import uvicorn

        uvicorn.run("app.main:app", host="127.0.0.1", port=int(os.environ["BASELINE_API_PORT"]),
                    log_level="warning", access_log=False)


if __name__ == "__main__":
    main()
