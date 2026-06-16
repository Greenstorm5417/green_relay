"""Resource namespaces grouping the API endpoints by area."""

from __future__ import annotations

import logging
import threading
from collections.abc import Iterator
from typing import TYPE_CHECKING, Callable

from ._sse import ServiceEvent, iter_events
from .models import (
    HealthResponse,
    InboundMessage,
    InboundSmsEvent,
    MessageStatusEvent,
    OutboundMessage,
    SendResponse,
    StatusResponse,
    SyncSendResponse,
)

if TYPE_CHECKING:
    from ._client import GreenRelay

logger = logging.getLogger(__name__)


class MessagesResource:
    """Send SMS and read message records."""

    def __init__(self, client: GreenRelay) -> None:
        self._client = client

    def send(self, to: str, body: str) -> SendResponse:
        """Queues a message for delivery and returns immediately (202)."""
        resp = self._client._request("POST", "/api/v1/messages", json_body={"to": to, "body": body})
        return SendResponse.from_dict(resp.json())

    def send_sync(self, to: str, body: str) -> SyncSendResponse:
        """Sends a message and waits for delivery to resolve.

        Falls back to a queued result (``queued=True``) when the server's
        wait window elapses before delivery finishes.
        """
        resp = self._client._request(
            "POST",
            "/api/v1/messages/sync",
            json_body={"to": to, "body": body},
            allow_status={202},
        )
        return SyncSendResponse.from_dict(resp.json(), queued=resp.status_code == 202)

    def get(self, message_id: int) -> OutboundMessage:
        """Fetches a single outbound message by ID."""
        resp = self._client._request("GET", f"/api/v1/messages/{message_id}")
        return OutboundMessage.from_dict(resp.json())

    def list_inbound(
        self, limit: int | None = None, offset: int | None = None
    ) -> list[InboundMessage]:
        """Lists received inbound messages, newest first.

        This is the polling (synchronous) way to read incoming messages.
        """
        params = {}
        if limit is not None:
            params["limit"] = limit
        if offset is not None:
            params["offset"] = offset
        resp = self._client._request("GET", "/api/v1/messages/inbound", params=params)
        return [InboundMessage.from_dict(item) for item in resp.json()]


class HealthResource:
    """Unauthenticated service health and modem status."""

    def __init__(self, client: GreenRelay) -> None:
        self._client = client

    def get(self) -> HealthResponse:
        """Returns overall health. A degraded/unhealthy service still
        responds with a body rather than raising."""
        resp = self._client._request("GET", "/health", allow_status={503})
        return HealthResponse.from_dict(resp.json())

    def status(self) -> StatusResponse:
        """Returns detailed modem status (signal, registration, operator)."""
        resp = self._client._request("GET", "/status")
        return StatusResponse.from_dict(resp.json())


class EventListener:
    """Handle to a background thread streaming events to callbacks.

    Returned by :meth:`EventsResource.listen`. Call :meth:`stop` to end the
    stream and join the worker thread.
    """

    def __init__(self, thread: threading.Thread, stop_event: threading.Event) -> None:
        self._thread = thread
        self._stop_event = stop_event

    @property
    def running(self) -> bool:
        return self._thread.is_alive()

    def stop(self, timeout: float | None = 5.0) -> None:
        """Signals the worker to stop and waits for it to finish."""
        self._stop_event.set()
        self._thread.join(timeout=timeout)


class EventsResource:
    """Real-time Server-Sent Events stream."""

    def __init__(self, client: GreenRelay) -> None:
        self._client = client

    def stream(self) -> Iterator[ServiceEvent]:
        """Yields events as they arrive (the synchronous/iterator way).

        Blocks while waiting for the next event. Iterate in a ``for`` loop
        or call ``next()`` on the returned generator.
        """
        resp = self._client._request("GET", "/api/v1/events", stream=True)
        try:
            yield from iter_events(resp.iter_lines(decode_unicode=False))
        finally:
            resp.close()

    def listen(
        self,
        on_message_status: Callable[[MessageStatusEvent], None] | None = None,
        on_inbound_sms: Callable[[InboundSmsEvent], None] | None = None,
        on_event: Callable[[ServiceEvent], None] | None = None,
        on_error: Callable[[Exception], None] | None = None,
    ) -> EventListener:
        """Streams events in a background thread and dispatches to callbacks.

        This is the callback/event-emitter way. Returns an
        :class:`EventListener`; call ``stop()`` on it to end the stream.
        """
        stop_event = threading.Event()

        def worker() -> None:
            try:
                for event in self.stream():
                    if stop_event.is_set():
                        break
                    if on_event is not None:
                        on_event(event)
                    if on_message_status is not None and isinstance(event.data, MessageStatusEvent):
                        on_message_status(event.data)
                    if on_inbound_sms is not None and isinstance(event.data, InboundSmsEvent):
                        on_inbound_sms(event.data)
            except Exception as exc:  # noqa: BLE001 - surfaced via on_error
                if on_error is not None:
                    on_error(exc)
                else:
                    logger.exception("event listener stopped due to an error")

        thread = threading.Thread(target=worker, name="green-relay-events", daemon=True)
        thread.start()
        return EventListener(thread, stop_event)
