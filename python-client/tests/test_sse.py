"""Regression tests for the Server-Sent Events parser."""

from __future__ import annotations

from green_relay._sse import ServiceEvent, iter_events
from green_relay.models import InboundSmsEvent, MessageStatusEvent


def test_single_message_status_event():
    lines = [
        "event: message_status",
        'data: {"id": 1, "status": "sent", "reference": "42"}',
        "",
    ]
    events = list(iter_events(lines))
    assert len(events) == 1
    assert events[0].name == "message_status"
    assert isinstance(events[0].data, MessageStatusEvent)
    assert events[0].data.status.value == "sent"


def test_single_inbound_sms_event():
    lines = [
        "event: inbound_sms",
        'data: {"id": 7, "from": "+14155550123", "body": "hi"}',
        "",
    ]
    events = list(iter_events(lines))
    assert isinstance(events[0].data, InboundSmsEvent)
    assert events[0].data.from_ == "+14155550123"


def test_multiple_events_in_sequence():
    lines = [
        "event: inbound_sms",
        'data: {"id": 1, "from": "+1234567", "body": "a"}',
        "",
        "event: message_status",
        'data: {"id": 2, "status": "queued"}',
        "",
    ]
    events = list(iter_events(lines))
    assert [e.name for e in events] == ["inbound_sms", "message_status"]


def test_keep_alive_comment_lines_are_ignored():
    lines = [
        ": keep-alive",
        "event: message_status",
        'data: {"id": 1, "status": "sent"}',
        "",
        ":",
    ]
    events = list(iter_events(lines))
    assert len(events) == 1


def test_bytes_input_is_decoded():
    lines = [
        b"event: inbound_sms",
        b'data: {"id": 1, "from": "+1", "body": "b"}',
        b"",
    ]
    events = list(iter_events(lines))
    assert isinstance(events[0].data, InboundSmsEvent)


def test_multiline_data_is_joined():
    lines = [
        "event: message_status",
        'data: {"id": 1,',
        'data:  "status": "sent"}',
        "",
    ]
    events = list(iter_events(lines))
    assert isinstance(events[0].data, MessageStatusEvent)
    assert events[0].data.id == 1


def test_incomplete_event_without_trailing_blank_is_not_emitted():
    lines = [
        "event: message_status",
        'data: {"id": 1, "status": "sent"}',
    ]
    events = list(iter_events(lines))
    assert events == []


def test_unknown_event_name_returns_raw_dict():
    lines = [
        "event: something_else",
        'data: {"foo": "bar"}',
        "",
    ]
    events = list(iter_events(lines))
    assert events[0].name == "something_else"
    assert events[0].data == {"foo": "bar"}


def test_data_without_event_name_defaults_to_message():
    lines = [
        'data: {"foo": 1}',
        "",
    ]
    events = list(iter_events(lines))
    assert events[0].name == "message"
    assert events[0].data == {"foo": 1}


def test_non_json_data_falls_back_to_text():
    lines = [
        "event: something_else",
        "data: plain text payload",
        "",
    ]
    events = list(iter_events(lines))
    assert events[0].data == "plain text payload"


def test_service_event_is_dataclass():
    evt = ServiceEvent(name="message", data={"x": 1})
    assert evt.name == "message"
    assert evt.data == {"x": 1}
