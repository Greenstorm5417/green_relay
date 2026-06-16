"""Regression tests for the APIError type."""

from __future__ import annotations

from green_relay import APIError


def test_api_error_attributes():
    err = APIError(429, "rate limit exceeded", fields=[], retry_after=30)
    assert err.status_code == 429
    assert err.error == "rate limit exceeded"
    assert err.fields == []
    assert err.retry_after == 30


def test_api_error_message_without_fields():
    err = APIError(401, "unauthorized")
    assert str(err) == "[401] unauthorized"
    assert err.fields == []
    assert err.retry_after is None


def test_api_error_message_includes_fields():
    err = APIError(400, "missing required fields", fields=["to", "body"])
    assert str(err) == "[400] missing required fields (fields: to, body)"


def test_api_error_is_exception():
    err = APIError(500, "internal server error")
    assert isinstance(err, Exception)


def test_api_error_can_be_raised_and_caught():
    try:
        raise APIError(503, "not ready", retry_after=5)
    except APIError as exc:
        assert exc.retry_after == 5
        assert exc.status_code == 503
