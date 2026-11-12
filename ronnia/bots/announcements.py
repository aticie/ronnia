"""Tracks live transitions so each legacy user is announced once per session."""

from __future__ import annotations

from collections.abc import Iterable, Mapping

from ronnia.models.database import DBUser


def select_announcements(
        pending: Iterable[DBUser],
        announced_at: Mapping[int, float],
        now: float,
        dedupe_seconds: float,
        batch: int,
) -> list[DBUser]:
    """Pick the notices allowed to be sent right now.

    ``announced_at`` is the durable record of previously delivered notices; a
    user inside the de-duplication window is skipped even though the in-memory
    session state forgot them, which is what stops a restart (or a crash loop)
    from re-announcing every live channel. At most ``batch`` users are returned
    so a single cycle cannot flood Twitch or the osu! PM API.
    """
    if batch <= 0:
        return []
    selected: list[DBUser] = []
    for user in pending:
        if len(selected) >= batch:
            break
        last = announced_at.get(user.twitchId)
        if last is not None and now - last < dedupe_seconds:
            continue
        selected.append(user)
    return selected


class LiveAnnouncementTracker:
    """Track currently live legacy users and queue one announcement per session.

    A user is re-armed only after they go offline and start a new live session.
    Pending announcements that have not been flushed are dropped if the user
    goes offline before delivery.
    """

    def __init__(self, enabled: bool = True) -> None:
        self.enabled = enabled
        self._live: set[int] = set()
        self._announced: set[int] = set()
        self._pending: list[DBUser] = []

    @property
    def live_twitch_ids(self) -> set[int]:
        return set(self._live)

    @property
    def announced_twitch_ids(self) -> set[int]:
        return set(self._announced)

    @property
    def pending(self) -> list[DBUser]:
        return list(self._pending)

    def observe(
        self,
        users: Iterable[DBUser],
        live_twitch_ids: Iterable[int],
    ) -> list[DBUser]:
        """Update live state and queue announcements for newly live users."""
        users_by_id = {user.twitchId: user for user in users}
        requested_live_ids = {int(twitch_id) for twitch_id in live_twitch_ids}
        current_live_ids = {
            twitch_id for twitch_id in requested_live_ids if twitch_id in users_by_id
        }

        self._live = current_live_ids
        self._announced.intersection_update(self._live)
        self._pending = [
            user for user in self._pending if user.twitchId in self._live
        ]

        newly_live = [
            user
            for twitch_id, user in users_by_id.items()
            if twitch_id in current_live_ids and twitch_id not in self._announced
        ]

        if self.enabled and newly_live:
            pending_ids = {user.twitchId for user in self._pending}
            for user in newly_live:
                if user.twitchId not in pending_ids:
                    self._pending.append(user)
                    pending_ids.add(user.twitchId)

        # Mark the session as announced even before delivery so a failing send
        # cannot cause a message storm on every polling cycle. The user is
        # re-armed after going offline.
        self._announced.update(user.twitchId for user in newly_live)

        return newly_live if self.enabled else []

    def take_pending(self) -> list[DBUser]:
        pending = self._pending
        self._pending = []
        return pending

    def requeue(self, users: Iterable[DBUser]) -> None:
        """Put un-delivered notices back so a later cycle retries them.

        ``observe`` marks a session announced *before* delivery to avoid a
        message storm, so a failed send re-arms the user here: the marker is
        cleared and the notice is queued again while the channel is still live.
        """
        queued = {user.twitchId for user in self._pending}
        for user in users:
            if user.twitchId not in self._live:
                continue
            self._announced.discard(user.twitchId)
            if user.twitchId not in queued:
                self._pending.append(user)
                queued.add(user.twitchId)
