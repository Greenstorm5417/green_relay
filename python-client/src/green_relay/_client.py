"""The Green Relay API client."""

from __future__ import annotations

import logging
from typing import Any

import requests

from .errors import APIError
from .resources import EventsResource, HealthResource, MessagesResource

logger = logging.getLogger(__name__)

DEFAULT_TIMEOUT = 30.0


class GreenRelay:
    """Client for the Green Relay SMS microservice.

    Example::

        from green_relay import GreenRelay

        client = GreenRelay(api_key="...", base_url="http://localhost:8080")
        resp = client.messages.send(to="+14155552671", body="Hello")
        print(resp.id, resp.status)

    Endpoints are grouped into resource namespaces:
    ``messages``, ``health``, and ``events``.
    """

    def __init__(
        self,
        api_key: str,
        base_url: str,
        timeout: float = DEFAULT_TIMEOUT,
        session: requests.Session | None = None,
    ) -> None:
        if not api_key:
            raise ValueError("api_key is required")
        if not base_url:
            raise ValueError("base_url is required")

        self.api_key = api_key
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        self._session = session or requests.Session()
        self._session.headers.update(
            {
                "x-api-key": api_key,
                "Accept": "application/json",
                "User-Agent": "green-relay-python/0.1.0",
            }
        )

        self.messages = MessagesResource(self)
        self.health = HealthResource(self)
        self.events = EventsResource(self)

    def _request(
        self,
        method: str,
        path: str,
        params: dict[str, Any] | None = None,
        json_body: dict[str, Any] | None = None,
        stream: bool = False,
        allow_status: set[int] | None = None,
    ) -> requests.Response:
        """Sends a request and returns the response, raising on errors.

        ``allow_status`` lists non-2xx codes that should be returned to the
        caller instead of raising (used by endpoints where a non-success
        code still carries a meaningful body).
        """
        url = f"{self.base_url}{path}"
        timeout = None if stream else self.timeout
        logger.debug("%s %s", method, url)

        response = self._session.request(
            method,
            url,
            params=params,
            json=json_body,
            stream=stream,
            timeout=timeout,
        )

        allowed = allow_status or set()
        if response.status_code >= 400 and response.status_code not in allowed:
            raise self._build_error(response)

        return response

    @staticmethod
    def _build_error(response: requests.Response):
        error_message = f"HTTP {response.status_code}"
        fields = []
        try:
            body = response.json()
            error_message = body.get("error", error_message)
            fields = body.get("fields", [])
        except ValueError:
            text = response.text.strip()
            if text:
                error_message = text

        retry_after = None
        header = response.headers.get("Retry-After")
        if header is not None:
            try:
                retry_after = int(header)
            except ValueError:
                retry_after = None

        return APIError(response.status_code, error_message, fields, retry_after)

    def close(self) -> None:
        """Closes the underlying HTTP session."""
        self._session.close()

    def __enter__(self) -> GreenRelay:
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.close()
