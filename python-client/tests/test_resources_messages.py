"""Regression tests for the messages resource."""

from __future__ import annotations

from conftest import FakeResponse, FakeSession
from green_relay import GreenRelay, MessageStatus


def test_send_returns_send_response(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(202, json_data={"id": 5, "status": "queued", "parts": 1}))
    resp = client.messages.send(to="+14155552671", body="hello")
    assert resp.id == 5
    assert resp.status is MessageStatus.QUEUED
    assert resp.parts == 1


def test_send_posts_to_messages_endpoint(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(202, json_data={"id": 1, "status": "queued", "parts": 1}))
    client.messages.send(to="+1234567", body="hi")
    call = session.calls[0]
    assert call["method"] == "POST"
    assert call["url"].endswith("/api/v1/messages")
    assert call["json"] == {"to": "+1234567", "body": "hi"}


def test_send_sync_resolved_returns_200(client: GreenRelay, session: FakeSession):
    session.queue(
        FakeResponse(200, json_data={"id": 9, "status": "sent", "reference": "25", "parts": 1})
    )
    resp = client.messages.send_sync(to="+14155552671", body="hello")
    assert resp.queued is False
    assert resp.status is MessageStatus.SENT
    assert resp.reference == "25"


def test_send_sync_queued_fallback_returns_202(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(202, json_data={"id": 9, "status": "queued", "parts": 1}))
    resp = client.messages.send_sync(to="+14155552671", body="hello")
    assert resp.queued is True
    assert resp.reference is None


def test_send_sync_targets_sync_endpoint(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(200, json_data={"id": 1, "status": "sent", "parts": 1}))
    client.messages.send_sync(to="+1234567", body="hi")
    assert session.calls[0]["url"].endswith("/api/v1/messages/sync")


def test_get_returns_outbound_message(client: GreenRelay, session: FakeSession):
    session.queue(
        FakeResponse(
            200,
            json_data={
                "id": 7,
                "to_number": "+14155552671",
                "body": "hello",
                "status": "sent",
                "part_count": 1,
                "msg_reference": "42",
                "error_code": None,
                "created_at": "2024-01-02T03:04:05Z",
                "updated_at": "2024-01-02T03:04:05Z",
            },
        )
    )
    msg = client.messages.get(7)
    assert msg.id == 7
    assert msg.status is MessageStatus.SENT
    assert session.calls[0]["url"].endswith("/api/v1/messages/7")


def test_list_inbound_returns_list(client: GreenRelay, session: FakeSession):
    session.queue(
        FakeResponse(
            200,
            json_data=[
                {
                    "id": 1,
                    "from_number": "+14155550000",
                    "body": "a",
                    "received_at": "2024-01-02T03:04:05Z",
                },
                {
                    "id": 2,
                    "from_number": "+14155550001",
                    "body": "b",
                    "received_at": "2024-01-02T03:04:06Z",
                },
            ],
        )
    )
    messages = client.messages.list_inbound()
    assert len(messages) == 2
    assert messages[0].from_number == "+14155550000"
    assert messages[1].body == "b"


def test_list_inbound_empty(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(200, json_data=[]))
    assert client.messages.list_inbound() == []


def test_list_inbound_passes_pagination(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(200, json_data=[]))
    client.messages.list_inbound(limit=50, offset=100)
    assert session.calls[0]["params"] == {"limit": 50, "offset": 100}


def test_list_inbound_omits_unset_pagination(client: GreenRelay, session: FakeSession):
    session.queue(FakeResponse(200, json_data=[]))
    client.messages.list_inbound()
    assert session.calls[0]["params"] == {}
