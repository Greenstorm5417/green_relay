"""Shared test fakes and fixtures.

The tests never touch the network. ``GreenRelay`` accepts a ``session``
argument, so we inject a :class:`FakeSession` that returns canned
:class:`FakeResponse` objects and records every request it was asked to make.
"""

from __future__ import annotations

from typing import Any

import pytest

from green_relay import GreenRelay


class FakeResponse:
    """Minimal stand-in for ``requests.Response``."""

    def __init__(
        self,
        status_code: int = 200,
        json_data: Any = None,
        text: str = "",
        headers: dict[str, str] | None = None,
        lines: list[Any] | None = None,
    ) -> None:
        self.status_code = status_code
        self._json = json_data
        self._has_json = json_data is not None
        self.text = text
        self.headers = headers or {}
        self._lines = lines or []
        self.closed = False

    def json(self) -> Any:
        if not self._has_json:
            raise ValueError("no json body")
        return self._json

    def iter_lines(self, decode_unicode: bool = False):
        yield from self._lines

    def close(self) -> None:
        self.closed = True


class FakeSession:
    """Stand-in for ``requests.Session`` that replays queued responses."""

    def __init__(self) -> None:
        self.headers: dict[str, str] = {}
        self.calls: list[dict[str, Any]] = []
        self._queue: list[Any] = []
        self.closed = False

    def queue(self, *responses: Any) -> FakeSession:
        """Enqueues responses (or exceptions) to return on subsequent calls."""
        self._queue.extend(responses)
        return self

    def request(
        self,
        method: str,
        url: str,
        params: Any = None,
        json: Any = None,
        stream: bool = False,
        timeout: Any = None,
    ) -> FakeResponse:
        self.calls.append(
            {
                "method": method,
                "url": url,
                "params": params,
                "json": json,
                "stream": stream,
                "timeout": timeout,
            }
        )
        if not self._queue:
            raise AssertionError(f"no queued response for request: {method} {url}")
        item = self._queue.pop(0)
        if isinstance(item, Exception):
            raise item
        return item

    def close(self) -> None:
        self.closed = True


@pytest.fixture
def session() -> FakeSession:
    return FakeSession()


@pytest.fixture
def client(session: FakeSession) -> GreenRelay:
    return GreenRelay(
        api_key="test-key",
        base_url="http://localhost:8080",
        session=session,
    )
