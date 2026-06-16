"""Typed data models mirroring the Green Relay API payloads."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from typing import Any


class MessageStatus(str, Enum):
    """Delivery status of a message."""

    QUEUED = "queued"
    SENT = "sent"
    FAILED = "failed"


def _parse_dt(value: str | None) -> datetime | None:
    if value is None:
        return None
    text = value.replace("Z", "+00:00")
    return datetime.fromisoformat(text)


@dataclass
class SendResponse:
    """Returned when a message is accepted and queued for delivery."""

    id: int
    status: MessageStatus
    parts: int

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> SendResponse:
        return cls(
            id=data["id"],
            status=MessageStatus(data["status"]),
            parts=data["parts"],
        )


@dataclass
class SyncSendResponse:
    """Returned by the synchronous send once delivery resolves.

    ``queued`` is True when the wait window elapsed before delivery
    finished and the service fell back to a queued response.
    """

    id: int
    status: MessageStatus
    parts: int
    reference: str | None = None
    queued: bool = False

    @classmethod
    def from_dict(cls, data: dict[str, Any], queued: bool) -> SyncSendResponse:
        return cls(
            id=data["id"],
            status=MessageStatus(data["status"]),
            parts=data["parts"],
            reference=data.get("reference"),
            queued=queued,
        )


@dataclass
class OutboundMessage:
    """A persisted outbound SMS record."""

    id: int
    to_number: str
    body: str
    status: MessageStatus
    part_count: int
    msg_reference: str | None
    error_code: str | None
    created_at: datetime | None
    updated_at: datetime | None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> OutboundMessage:
        return cls(
            id=data["id"],
            to_number=data["to_number"],
            body=data["body"],
            status=MessageStatus(data["status"]),
            part_count=data["part_count"],
            msg_reference=data.get("msg_reference"),
            error_code=data.get("error_code"),
            created_at=_parse_dt(data.get("created_at")),
            updated_at=_parse_dt(data.get("updated_at")),
        )


@dataclass
class InboundMessage:
    """A persisted inbound SMS record."""

    id: int
    from_number: str
    body: str
    received_at: datetime | None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> InboundMessage:
        return cls(
            id=data["id"],
            from_number=data["from_number"],
            body=data["body"],
            received_at=_parse_dt(data.get("received_at")),
        )


@dataclass
class HealthResponse:
    """Overall service health and serial/SIM connection state."""

    health: str
    serial_connected: bool
    sim_status: str

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> HealthResponse:
        return cls(
            health=data["health"],
            serial_connected=data["serial_connected"],
            sim_status=data["sim_status"],
        )


@dataclass
class StatusResponse:
    """Detailed modem status: signal, registration, and operator."""

    signal_percent: int | None
    registered: bool | None
    operator: str | None
    unavailable: list[str] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> StatusResponse:
        return cls(
            signal_percent=data.get("signal_percent"),
            registered=data.get("registered"),
            operator=data.get("operator"),
            unavailable=list(data.get("unavailable", [])),
        )


@dataclass
class MessageStatusEvent:
    """An outbound message transitioned to a terminal status."""

    id: int
    status: MessageStatus
    reference: str | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> MessageStatusEvent:
        return cls(
            id=data["id"],
            status=MessageStatus(data["status"]),
            reference=data.get("reference"),
        )


@dataclass
class InboundSmsEvent:
    """A new inbound message was received and persisted."""

    id: int
    from_: str
    body: str

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> InboundSmsEvent:
        return cls(
            id=data["id"],
            from_=data["from"],
            body=data["body"],
        )
