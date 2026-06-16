"""Regression tests for the GreenRelay client request/error plumbing."""

from __future__ import annotations

import pytest
import requests

from conftest import FakeResponse, FakeSession
from green_relay import APIError, GreenRelay


def test_requires_api_key():
    with pytest.raises(ValueError):
        GreenRelay(api_key="", base_url="http://localhost")


def test_requires_base_url():
    with pytest.raises(ValueError):
        GreenRelay(api_key="key", base_url="")


def test_base_url_trailing_slash_is_stripped():
    client = GreenRelay(api_key="k", base_url="http://localhost:8080/", session=FakeSession())
    assert client.base_url == "http://localhost:8080"


def test_api_key_header_is_set(session: FakeSession):
    GreenRelay(api_key="secret", base_url="http://localhost", session=session)
    assert session.headers["x-api-key"] == "secret"
    assert session.headers["Accept"] == "application/json"


def test_request_builds_url_and_passes_params(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(200, json_data={"ok": True}))
    client._request("GET", "/foo", params={"limit": 5})
    call = session.calls[0]
    assert call["url"] == "http://localhost:8080/foo"
    assert call["params"] == {"limit": 5}
    assert call["method"] == "GET"


def test_request_passes_json_body(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(202, json_data={}))
    client._request("POST", "/foo", json_body={"a": 1})
    assert session.calls[0]["json"] == {"a": 1}


def test_request_raises_api_error_with_body(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(400, json_data={"error": "bad", "fields": ["to"]}))
    with pytest.raises(APIError) as exc_info:
        client._request("POST", "/foo")
    err = exc_info.value
    assert err.status_code == 400
    assert err.error == "bad"
    assert err.fields == ["to"]


def test_request_parses_retry_after_header(client: GreenRelay, session: FakeSession):
    session.queue(
        FakeResponse(
            429,
            json_data={"error": "rate limit exceeded", "fields": []},
            headers={"Retry-After": "42"},
        )
    )
    with pytest.raises(APIError) as exc_info:
        client._request("GET", "/foo")
    assert exc_info.value.retry_after == 42


def test_request_ignores_non_integer_retry_after(client: GreenRelay, session: FakeSession):
    session.queue(
        FakeResponse(
            503,
            json_data={"error": "not ready", "fields": []},
            headers={"Retry-After": "soon"},
        )
    )
    with pytest.raises(APIError) as exc_info:
        client._request("GET", "/foo")
    assert exc_info.value.retry_after is None


def test_request_error_falls_back_to_text_when_not_json(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(500, text="boom"))
    with pytest.raises(APIError) as exc_info:
        client._request("GET", "/foo")
    assert exc_info.value.error == "boom"
    assert exc_info.value.status_code == 500


def test_allow_status_returns_response_instead_of_raising(client: GreenRelay, session: FakeSession):
    resp = FakeResponse(503, json_data={"health": "unhealthy"})
    session.queue(resp)
    returned = client._request("GET", "/health", allow_status={503})
    assert returned is resp


def test_transport_errors_propagate_unwrapped(client: GreenRelay, session: FakeSession):
    session.queue(requests.ConnectionError("refused"))
    with pytest.raises(requests.ConnectionError):
        client._request("GET", "/foo")


def test_stream_request_uses_no_timeout(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(200, lines=[]))
    client._request("GET", "/events", stream=True)
    assert session.calls[0]["timeout"] is None
    assert session.calls[0]["stream"] is True


def test_non_stream_request_uses_timeout(session: FakeSession):
    client = GreenRelay(api_key="k", base_url="http://localhost", timeout=12.5, session=session)
    session.queue(FakeResponse(200, json_data={}))
    client._request("GET", "/foo")
    assert session.calls[0]["timeout"] == 12.5


def test_context_manager_closes_session(session: FakeSession):
    with GreenRelay(api_key="k", base_url="http://localhost", session=session) as client:
        assert client.api_key == "k"
    assert session.closed is True


def test_close_closes_session(client: GreenRelay, session: FakeSession):
    client.close()
    assert session.closed is True
