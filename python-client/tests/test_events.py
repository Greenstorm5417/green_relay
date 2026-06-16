"""Regression tests for the events resource (stream + listen)."""

from __future__ import annotations

import threading

import requests

from conftest import FakeResponse, FakeSession
from green_relay import GreenRelay
from green_relay.models import InboundSmsEvent, MessageStatusEvent

STREAM_LINES = [
    "event: inbound_sms",
    'data: {"id": 1, "from": "+14155550123", "body": "hi"}',
    "",
    "event: message_status",
    'data: {"id": 2, "status": "sent", "reference": "42"}',
    "",
]


def test_stream_yields_decoded_events(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(200, lines=STREAM_LINES))
    events = list(client.events.stream())
    assert [e.name for e in events] == ["inbound_sms", "message_status"]
    assert isinstance(events[0].data, InboundSmsEvent)
    assert isinstance(events[1].data, MessageStatusEvent)


def test_stream_requests_events_endpoint_with_stream_flag(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(200, lines=STREAM_LINES))
    list(client.events.stream())
    call = session.calls[0]
    assert call["url"].endswith("/api/v1/events")
    assert call["stream"] is True


def test_stream_closes_response_when_exhausted(client: GreenRelay, session: FakeSession):
    resp = FakeResponse(200, lines=STREAM_LINES)
    session.queue(resp)
    list(client.events.stream())
    assert resp.closed is True


def test_listen_dispatches_to_typed_callbacks(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(200, lines=STREAM_LINES))
    inbound = []
    status = []
    listener = client.events.listen(
        on_inbound_sms=inbound.append,
        on_message_status=status.append,
    )
    listener.stop()  # joins the worker thread
    assert len(inbound) == 1
    assert inbound[0].from_ == "+14155550123"
    assert len(status) == 1
    assert status[0].reference == "42"


def test_listen_calls_on_event_for_every_event(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(200, lines=STREAM_LINES))
    seen = []
    listener = client.events.listen(on_event=seen.append)
    listener.stop()
    assert [e.name for e in seen] == ["inbound_sms", "message_status"]


def test_listen_reports_errors_to_on_error(client: GreenRelay, session: FakeSession):
    session.queue(requests.ConnectionError("refused"))
    errors = []
    done = threading.Event()

    def on_error(exc):
        errors.append(exc)
        done.set()

    listener = client.events.listen(on_error=on_error)
    done.wait(timeout=5)
    listener.stop()
    assert len(errors) == 1
    assert isinstance(errors[0], requests.ConnectionError)


def test_listener_running_property(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(200, lines=STREAM_LINES))
    listener = client.events.listen(on_event=lambda e: None)
    listener.stop()
    assert listener.running is False
