"""Green Relay SMS microservice Python client."""

from __future__ import annotations

import logging

from ._client import GreenRelay
from ._sse import ServiceEvent
from .errors import APIError
from .models import (
    HealthResponse,
    InboundMessage,
    InboundSmsEvent,
    MessageStatus,
    MessageStatusEvent,
    OutboundMessage,
    SendResponse,
    StatusResponse,
    SyncSendResponse,
)
from .resources import EventListener

# Library logging convention: attach a no-op handler so importing the
# client never emits warnings; applications configure logging themselves.
logging.getLogger(__name__).addHandler(logging.NullHandler())

__version__ = "0.1.0"

__all__ = [
    "GreenRelay",
    "ServiceEvent",
    "EventListener",
    "MessageStatus",
    "SendResponse",
    "SyncSendResponse",
    "OutboundMessage",
    "InboundMessage",
    "HealthResponse",
    "StatusResponse",
    "MessageStatusEvent",
    "InboundSmsEvent",
    "APIError",
]
