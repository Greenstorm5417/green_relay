"""Regression tests for the health resource."""

from __future__ import annotations

from conftest import FakeResponse, FakeSession
from green_relay import GreenRelay


def test_health_get_healthy(client: GreenRelay, session: FakeSession):
    session.queue(
        FakeResponse(
            200,
            json_data={"health": "healthy", "serial_connected": True, "sim_status": "ready"},
        )
    )
    health = client.health.get()
    assert health.health == "healthy"
    assert health.serial_connected is True
    assert session.calls[0]["url"].endswith("/health")


def test_health_get_unhealthy_503_still_parses(client: GreenRelay, session: FakeSession):
    # A 503 from /health carries a valid HealthResponse body and must not raise.
    session.queue(
        FakeResponse(
            503,
            json_data={
                "health": "unhealthy",
                "serial_connected": False,
                "sim_status": "unknown",
            },
        )
    )
    health = client.health.get()
    assert health.health == "unhealthy"
    assert health.serial_connected is False


def test_status_returns_status_response(client: GreenRelay, session: FakeSession):
    session.queue(
        FakeResponse(
            200,
            json_data={
                "signal_percent": 75,
                "registered": True,
                "operator": "Carrier",
                "unavailable": [],
            },
        )
    )
    status = client.health.status()
    assert status.signal_percent == 75
    assert status.operator == "Carrier"
    assert session.calls[0]["url"].endswith("/status")


def test_status_with_unavailable_fields(client: GreenRelay, session: FakeSession):
    session.queue(
        FakeResponse(
            200,
            json_data={
                "signal_percent": None,
                "registered": None,
                "operator": None,
                "unavailable": ["signal", "registration", "operator"],
            },
        )
    )
    status = client.health.status()
    assert status.unavailable == ["signal", "registration", "operator"]
