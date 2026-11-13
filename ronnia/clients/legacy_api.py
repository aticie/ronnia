"""Authenticated client for the new dashboard's internal legacy API.

The new SQLite-backed service is the source of truth for users and settings.
This client is the only way the old bot reads users/settings and writes
delivery statistics while the bridge is active.

The internal token is never logged or included in error messages.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import time
from dataclasses import dataclass
from typing import Any, Mapping
from urllib.parse import quote

import aiohttp

from ronnia.models.database import DBUser

logger = logging.getLogger(__name__)

DEFAULT_TIMEOUT_SECONDS = 10.0
# A deploy replaces the container, so a pooled connection can point at an address
# that no longer answers. A refused connect means the request never left, so one
# retry on a fresh connection is safe even for a claim.
CONNECT_ATTEMPTS = 2
CONNECT_RETRY_DELAY_SECONDS = 0.25
# Only errors raised while establishing the connection: the request never left,
# so a retry cannot double-claim. Anything later may have been delivered.
RETRYABLE_CONNECT_ERRORS = (aiohttp.ClientConnectorError, ConnectionRefusedError)
DEFAULT_CACHE_TTL_SECONDS = 2.0
DEFAULT_PAGE_LIMIT = 100
MAX_PAGE_LIMIT = 100

_TRUTHY = {"1", "true", "yes", "on"}
_FALSY = {"0", "false", "no", "off"}


def env_flag(name: str, default: bool = False) -> bool:
    """Read a conventional boolean environment variable."""
    value = os.getenv(name)
    if value is None:
        return default
    normalized = value.strip().lower()
    if normalized in _TRUTHY:
        return True
    if normalized in _FALSY:
        return False
    return default


class LegacyAPIError(RuntimeError):
    """A visible failure while talking to the new dashboard's internal API."""

    def __init__(
        self,
        message: str,
        *,
        status: int | None = None,
        detail: str | None = None,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.detail = detail


class LegacyUserUnavailable(LegacyAPIError):
    """The API has no eligible legacy user for the requested id/login."""


@dataclass(frozen=True)
class ClaimResult:
    claimed: bool
    reason: str | None = None


class LegacyAPI:
    """Small aiohttp client for ``/internal/legacy/*``."""

    def __init__(
        self,
        base_url: str,
        token: str,
        *,
        cache_ttl: float = DEFAULT_CACHE_TTL_SECONDS,
        timeout: float = DEFAULT_TIMEOUT_SECONDS,
    ) -> None:
        if not base_url or not token:
            raise ValueError("LegacyAPI requires both a base URL and an internal token")

        self.base_url = base_url.rstrip("/")
        self._token = token
        self._cache_ttl = min(float(cache_ttl), DEFAULT_CACHE_TTL_SECONDS)
        self._timeout = timeout
        self._cache: dict[tuple[str, Any], tuple[float, DBUser]] = {}
        self._session: aiohttp.ClientSession | None = None
        self._session_lock = asyncio.Lock()

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None) -> LegacyAPI | None:
        """Build a client from the environment.

        Bridge mode is enabled when ``RONNIA_API_URL`` and
        ``RONNIA_INTERNAL_TOKEN`` are both present. ``RONNIA_LEGACY_MODE=false``
        explicitly keeps the old Mongo behaviour. If bridge mode is explicitly
        enabled without both settings, fail with a clear startup error instead
        of silently falling back to Mongo.
        """
        values = os.environ if env is None else env
        explicit_mode = (values.get("RONNIA_LEGACY_MODE") or "").strip()
        api_url = (values.get("RONNIA_API_URL") or "").strip()
        token = (values.get("RONNIA_INTERNAL_TOKEN") or "").strip()

        if explicit_mode:
            normalized = explicit_mode.lower()
            if normalized in _FALSY:
                logger.info("RONNIA_LEGACY_MODE is disabled; using legacy Mongo behaviour")
                return None
            if normalized not in _TRUTHY:
                raise RuntimeError(
                    "RONNIA_LEGACY_MODE must be one of true/false when set, "
                    f"got {explicit_mode!r}"
                )

            if not api_url or not token:
                missing = [
                    name
                    for name, value in (
                        ("RONNIA_API_URL", api_url),
                        ("RONNIA_INTERNAL_TOKEN", token),
                    )
                    if not value
                ]
                raise RuntimeError(
                    "RONNIA_LEGACY_MODE is enabled but the legacy bridge is "
                    f"missing: {', '.join(missing)}"
                )
            return cls(base_url=api_url, token=token)

        if api_url and token:
            return cls(base_url=api_url, token=token)

        if api_url or token:
            missing = "RONNIA_INTERNAL_TOKEN" if api_url else "RONNIA_API_URL"
            raise RuntimeError(
                "Incomplete legacy bridge configuration: "
                f"{missing} is missing. Set both RONNIA_API_URL and "
                "RONNIA_INTERNAL_TOKEN, or unset both to keep the old behaviour."
            )

        logger.warning(
            "Legacy bridge is not configured (RONNIA_API_URL/RONNIA_INTERNAL_TOKEN "
            "are unset); falling back to the old Mongo implementation."
        )
        return None

    async def _get_session(self) -> aiohttp.ClientSession:
        async with self._session_lock:
            if self._session is None or self._session.closed:
                self._session = aiohttp.ClientSession()
            return self._session

    async def close(self) -> None:
        if self._session is not None and not self._session.closed:
            await self._session.close()
        self._session = None

    async def _request(
        self,
        method: str,
        path: str,
        payload: dict[str, Any] | None = None,
        params: Mapping[str, Any] | None = None,
    ) -> tuple[int, Any]:
        url = f"{self.base_url}{path}"
        headers = {
            "Accept": "application/json",
            "Authorization": f"Bearer {self._token}",
        }
        for attempt in range(1, CONNECT_ATTEMPTS + 1):
            try:
                if attempt == 1:
                    session = await self._get_session()
                    return await self._send(
                        session, method, url, path, payload, params, headers
                    )
                # A refused connect is not proof the service is gone: it may be
                # mid-deploy. Retry on a throwaway session so the name is
                # resolved again instead of reusing the dead pooled socket.
                async with aiohttp.ClientSession() as retry_session:
                    return await self._send(
                        retry_session, method, url, path, payload, params, headers
                    )
            except RETRYABLE_CONNECT_ERRORS as exc:
                # Raised while establishing the connection, before any bytes are
                # sent, so retrying cannot duplicate a claim.
                if attempt == CONNECT_ATTEMPTS:
                    raise LegacyAPIError(
                        f"Legacy API request {method} {path} failed: {exc}"
                    ) from exc
                logger.warning(
                    "Legacy API %s %s could not connect (%s); retrying on a fresh connection",
                    method,
                    path,
                    type(exc).__name__,
                )
                await asyncio.sleep(CONNECT_RETRY_DELAY_SECONDS)
            except (aiohttp.ClientError, asyncio.TimeoutError) as exc:
                raise LegacyAPIError(
                    f"Legacy API request {method} {path} failed: {exc}"
                ) from exc
        raise LegacyAPIError(f"Legacy API request {method} {path} failed")

    async def _send(
        self,
        session: aiohttp.ClientSession,
        method: str,
        url: str,
        path: str,
        payload: dict[str, Any] | None,
        params: Mapping[str, Any] | None,
        headers: Mapping[str, str],
    ) -> tuple[int, Any]:
        async with session.request(
            method,
            url,
            json=payload,
            params=params,
            headers=headers,
            timeout=aiohttp.ClientTimeout(total=self._timeout),
        ) as response:
            raw_body = await response.text()
            try:
                body = json.loads(raw_body) if raw_body else None
            except json.JSONDecodeError:
                body = None
            logger.debug("Legacy API %s %s -> %s", method, path, response.status)
            return response.status, body

    @staticmethod
    def _error(
        method: str,
        path: str,
        status: int,
        body: Any,
    ) -> LegacyAPIError:
        detail = None
        if isinstance(body, dict):
            raw_detail = body.get("detail")
            if isinstance(raw_detail, str):
                detail = raw_detail
        message = f"Legacy API {method} {path} returned HTTP {status}"
        if detail:
            message = f"{message}: {detail}"
        return LegacyAPIError(message, status=status, detail=detail)

    def _cache_get(self, key: tuple[str, Any]) -> DBUser | None:
        entry = self._cache.get(key)
        if entry is None:
            return None
        cached_at, user = entry
        if time.monotonic() - cached_at > self._cache_ttl:
            self._cache.pop(key, None)
            return None
        return user

    def _cache_put(self, key: tuple[str, Any], user: DBUser) -> None:
        self._cache[key] = (time.monotonic(), user)

    async def _fetch_user(self, path: str) -> DBUser:
        status, body = await self._request("GET", path)
        if status == 404:
            raise LegacyUserUnavailable("User is not eligible in the new dashboard")
        if status != 200:
            raise self._error("GET", path, status, body)
        if not isinstance(body, dict):
            raise LegacyAPIError(f"Legacy API GET {path} returned an invalid user payload")
        try:
            return DBUser.model_validate(body)
        except Exception as exc:
            raise LegacyAPIError(
                f"Legacy API GET {path} returned an invalid user payload: {exc}"
            ) from exc

    async def get_user_by_id(self, twitch_id: int) -> DBUser:
        key = ("id", int(twitch_id))
        cached = self._cache_get(key)
        if cached is not None:
            return cached

        user = await self._fetch_user(f"/internal/legacy/channels/{int(twitch_id)}")
        self._cache_put(key, user)
        return user

    async def get_user_by_login(self, login: str) -> DBUser:
        normalized_login = login.strip().lower()
        key = ("login", normalized_login)
        cached = self._cache_get(key)
        if cached is not None:
            return cached

        user = await self._fetch_user(
            f"/internal/legacy/channels/by-login/{quote(normalized_login, safe='')}"
        )
        self._cache_put(key, user)
        return user

    async def fetch_enabled_users(self, limit: int = DEFAULT_PAGE_LIMIT) -> list[DBUser]:
        """Fetch every eligible legacy user, following ``after`` pagination."""
        if not 1 <= limit <= MAX_PAGE_LIMIT:
            raise ValueError(f"limit must be between 1 and {MAX_PAGE_LIMIT}")

        users: list[DBUser] = []
        after = 0
        while True:
            path = "/internal/legacy/channels"
            status, body = await self._request(
                "GET",
                path,
                params={"after": after, "limit": limit},
            )
            if status != 200:
                raise self._error("GET", path, status, body)

            logger.debug(
                "Legacy API GET /internal/legacy/channels after=%s -> %s",
                after,
                status,
            )

            if not isinstance(body, list):
                raise LegacyAPIError(
                    "Legacy API GET /internal/legacy/channels returned an invalid payload"
                )
            if not body:
                break

            page: list[DBUser] = []
            for item in body:
                if not isinstance(item, dict):
                    raise LegacyAPIError(
                        "Legacy API GET /internal/legacy/channels returned an invalid user payload"
                    )
                try:
                    page.append(DBUser.model_validate(item))
                except Exception as exc:
                    raise LegacyAPIError(
                        "Legacy API GET /internal/legacy/channels returned an invalid "
                        f"user payload: {exc}"
                    ) from exc

            users.extend(page)
            last_id = page[-1].twitchId
            if last_id <= after:
                raise LegacyAPIError(
                    "Legacy API pagination did not advance "
                    f"(after={after}, last={last_id})"
                )
            after = last_id

        return users

    async def claim(
        self,
        *,
        twitch_message_id: str,
        twitch_id: int,
        target_osu_id: int,
        message: str,
    ) -> ClaimResult:
        body = {
            "twitchMessageId": str(twitch_message_id),
            "twitchId": int(twitch_id),
            "targetOsuId": int(target_osu_id),
            "message": str(message),
        }
        path = "/internal/legacy/claim"
        status, response_body = await self._request("POST", path, payload=body)

        if status == 409:
            reason = None
            if isinstance(response_body, dict):
                raw_reason = response_body.get("reason")
                if isinstance(raw_reason, str):
                    reason = raw_reason
            logger.debug(
                "Legacy claim rejected for message %s: %s",
                twitch_message_id,
                reason or "duplicate or migrated",
            )
            return ClaimResult(False, reason)

        if status != 200:
            raise self._error("POST", path, status, response_body)

        claimed = isinstance(response_body, dict) and response_body.get("claimed") is True
        reason = None
        if isinstance(response_body, dict) and isinstance(response_body.get("reason"), str):
            reason = response_body["reason"]
        return ClaimResult(claimed, reason)

    async def report_live(self, channels: list[tuple[int, str]]) -> bool:
        """Publish the exact set of currently live legacy channels.

        The dashboard replaces its legacy live-state table on every successful
        call, so an empty ``channels`` list intentionally clears stale rows.
        A failed report is logged and never breaks the bot's polling loop; the
        in-memory streaming membership is unaffected.
        """
        payload = {
            "channels": [
                {"twitchId": int(twitch_id), "login": str(login)}
                for twitch_id, login in channels
            ]
        }
        path = "/internal/legacy/live"
        try:
            status, response_body = await self._request("POST", path, payload=payload)
            if status != 200:
                raise self._error("POST", path, status, response_body)
        except LegacyAPIError:
            logger.warning(
                "Failed to report %d live legacy channel(s) to the dashboard",
                len(payload["channels"]),
                exc_info=True,
            )
            return False
        logger.debug(
            "Reported %d live legacy channel(s) to the dashboard",
            len(payload["channels"]),
        )
        return True

    async def report_delivery(
        self,
        *,
        twitch_message_id: str,
        sent: bool,
        error: str | None = None,
        statistics: dict[str, Any] | None = None,
    ) -> None:
        path = f"/internal/legacy/deliveries/{quote(str(twitch_message_id), safe='')}"
        body = {
            "sent": bool(sent),
            "error": error,
            "statistics": statistics,
        }
        status, response_body = await self._request("POST", path, payload=body)
        if status != 200:
            raise self._error("POST", path, status, response_body)
