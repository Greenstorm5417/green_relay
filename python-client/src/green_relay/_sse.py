"""Minimal Server-Sent Events parsing for the event stream."""

from __future__ import annotations

import json
from collections.abc import Iterable, Iterator
from dataclasses import dataclass
from typing import Any

from .models import InboundSmsEvent, MessageStatusEvent


@dataclass
class ServiceEvent:
    """A decoded event from the ``/api/v1/events`` stream.

    ``name`` is the SSE event name (``message_status`` or ``inbound_sms``)
    and ``data`` is the parsed event payload.
    """

    name: str
    data: MessageStatusEvent | InboundSmsEvent | dict


def _decode(name: str, payload: Any) -> MessageStatusEvent | InboundSmsEvent | dict:
    if name == "message_status":
        return MessageStatusEvent.from_dict(payload)
    if name == "inbound_sms":
        return InboundSmsEvent.from_dict(payload)
    return payload


def iter_events(lines: Iterable[str | bytes]) -> Iterator[ServiceEvent]:
    """Parses raw SSE lines into decoded :class:`ServiceEvent` values.

    Accepts the line iterator produced by ``requests`` (which strips the
    trailing newline). A blank line terminates the current event.
    """
    name: str | None = None
    data_parts: list = []

    for raw in lines:
        line = raw.decode("utf-8") if isinstance(raw, bytes) else raw

        if line == "":
            if data_parts:
                payload_text = "\n".join(data_parts)
                event_name = name or "message"
                try:
                    payload = json.loads(payload_text)
                except json.JSONDecodeError:
                    payload = payload_text
                yield ServiceEvent(name=event_name, data=_decode(event_name, payload))
            name = None
            data_parts = []
            continue

        if line.startswith(":"):
            # Comment / keep-alive line.
            continue

        if ":" in line:
            field_name, _, value = line.partition(":")
            value = value[1:] if value.startswith(" ") else value
        else:
            field_name, value = line, ""

        if field_name == "event":
            name = value
        elif field_name == "data":
            data_parts.append(value)
