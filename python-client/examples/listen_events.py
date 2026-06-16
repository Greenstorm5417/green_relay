"""Read live events with background callbacks (callback / event-emitter way).

The listener runs on its own thread, so the main thread stays free for other
work. Press Ctrl+C to stop.
"""

from __future__ import annotations

import time

from _common import build_client


def main() -> None:
    client = build_client()

    def on_inbound(event):
        print(f"inbound from {event.from_}: {event.body}")

    def on_status(event):
        print(f"message {event.id} -> {event.status.value}")

    def on_error(exc):
        print(f"stream error: {exc}")

    listener = client.events.listen(
        on_inbound_sms=on_inbound,
        on_message_status=on_status,
        on_error=on_error,
    )
    print("listening in the background (Ctrl+C to stop)...")

    try:
        while listener.running:
            time.sleep(0.5)
    except KeyboardInterrupt:
        print("\nstopping...")
    finally:
        listener.stop()


if __name__ == "__main__":
    main()
