"""Regression tests for payload parsing in models.py."""

from __future__ import annotations

from datetime import datetime, timezone

import pytest

from green_relay.models import (
    HealthResponse,
    InboundMessage,
    InboundSmsEvent,
    MessageStatus,
    MessageStatusEvent,
    OutboundMessage,
    SendResponse,
    StatusResponse,
    SyncSendResponse,
    _parse_dt,
)


def test_message_status_values():
    assert MessageStatus.QUEUED.value == "queued"
    assert MessageStatus.SENT.value == "sent"
    assert MessageStatus.FAILED.value == "failed"
    # str-enum allows direct equality with the wire value.
    assert MessageStatus("sent") == MessageStatus.SENT
    assert MessageStatus.SENT == "sent"


def test_parse_dt_handles_z_suffix():
    dt = _parse_dt("2024-01-02T03:04:05Z")
    assert dt == datetime(2024, 1, 2, 3, 4, 5, tzinfo=timezone.utc)


def test_parse_dt_handles_explicit_offset():
    dt = _parse_dt("2024-01-02T03:04:05+00:00")
    assert dt == datetime(2024, 1, 2, 3, 4, 5, tzinfo=timezone.utc)


def test_parse_dt_handles_none():
    assert _parse_dt(None) is None


def test_send_response_from_dict():
    resp = SendResponse.from_dict({"id": 42, "status": "queued", "parts": 2})
    assert resp.id == 42
    assert resp.status is MessageStatus.QUEUED
    assert resp.parts == 2


def test_sync_send_response_resolved():
    resp = SyncSendResponse.from_dict(
        {"id": 1, "status": "sent", "reference": "25", "parts": 1}, queued=False
    )
    assert resp.status is MessageStatus.SENT
    assert resp.reference == "25"
    assert resp.queued is False


def test_sync_send_response_queued_fallback():
    # The 202 body is a SendResponse with no reference field.
    resp = SyncSendResponse.from_dict({"id": 9, "status": "queued", "parts": 1}, queued=True)
    assert resp.queued is True
    assert resp.reference is None


def test_outbound_message_full():
    msg = OutboundMessage.from_dict(
        {
            "id": 7,
            "to_number": "+14155552671",
            "body": "hello",
            "status": "failed",
            "part_count": 1,
            "msg_reference": None,
            "error_code": "500",
            "created_at": "2024-01-02T03:04:05Z",
            "updated_at": "2024-01-02T03:04:06Z",
        }
    )
    assert msg.id == 7
    assert msg.to_number == "+14155552671"
    assert msg.status is MessageStatus.FAILED
    assert msg.error_code == "500"
    assert msg.msg_reference is None
    assert msg.created_at < msg.updated_at


def test_outbound_message_missing_optionals():
    msg = OutboundMessage.from_dict(
        {
            "id": 1,
            "to_number": "+1234567",
            "body": "x",
            "status": "sent",
            "part_count": 1,
        }
    )
    assert msg.msg_reference is None
    assert msg.error_code is None
    assert msg.created_at is None
    assert msg.updated_at is None


def test_inbound_message_from_dict():
    msg = InboundMessage.from_dict(
        {
            "id": 3,
            "from_number": "+14155550000",
            "body": "incoming",
            "received_at": "2024-01-02T03:04:05Z",
        }
    )
    assert msg.from_number == "+14155550000"
    assert msg.body == "incoming"
    assert msg.received_at is not None


def test_health_response_from_dict():
    health = HealthResponse.from_dict(
        {"health": "degraded", "serial_connected": True, "sim_status": "ready"}
    )
    assert health.health == "degraded"
    assert health.serial_connected is True
    assert health.sim_status == "ready"


def test_status_response_with_unavailable():
    status = StatusResponse.from_dict(
        {
            "signal_percent": None,
            "registered": None,
            "operator": None,
            "unavailable": ["signal", "registration", "operator"],
        }
    )
    assert status.signal_percent is None
    assert status.unavailable == ["signal", "registration", "operator"]


def test_status_response_defaults_unavailable_to_empty():
    status = StatusResponse.from_dict(
        {"signal_percent": 75, "registered": True, "operator": "Carrier"}
    )
    assert status.unavailable == []
    assert status.signal_percent == 75
    assert status.operator == "Carrier"


def test_message_status_event_from_dict():
    event = MessageStatusEvent.from_dict({"id": 1, "status": "sent", "reference": "42"})
    assert event.id == 1
    assert event.status is MessageStatus.SENT
    assert event.reference == "42"


def test_message_status_event_without_reference():
    event = MessageStatusEvent.from_dict({"id": 2, "status": "failed"})
    assert event.reference is None
    assert event.status is MessageStatus.FAILED


def test_inbound_sms_event_maps_from_field():
    event = InboundSmsEvent.from_dict({"id": 7, "from": "+14155550123", "body": "hi"})
    # The wire field "from" is a Python keyword, exposed as "from_".
    assert event.from_ == "+14155550123"
    assert event.body == "hi"


def test_unknown_status_raises():
    with pytest.raises(ValueError):
        SendResponse.from_dict({"id": 1, "status": "bogus", "parts": 1})
