"""Read live events with the blocking iterator (sync way #2).

Iterates the event stream directly. Press Ctrl+C to stop.
"""

from __future__ import annotations

from _common import build_client
from green_relay.models import InboundSmsEvent, MessageStatusEvent


def main() -> None:
    client = build_client()
    print("listening for events (Ctrl+C to stop)...")

    try:
        for event in client.events.stream():
            if isinstance(event.data, InboundSmsEvent):
                print(f"inbound from {event.data.from_}: {event.data.body}")
            elif isinstance(event.data, MessageStatusEvent):
                print(f"message {event.data.id} -> {event.data.status.value}")
            else:
                print(f"{event.name}: {event.data}")
    except KeyboardInterrupt:
        print("\nstopped")


if __name__ == "__main__":
    main()
