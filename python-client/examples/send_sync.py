"""Send a message and block for the delivery outcome (sync send)."""

from __future__ import annotations

from _common import build_client


def main() -> None:
    client = build_client()
    result = client.messages.send_sync(to="+14155552671", body="Hello, synchronously")

    if result.queued:
        print(f"still queued after the wait window: message {result.id}")
    elif result.status.value == "sent":
        print(f"delivered message {result.id} (reference {result.reference})")
    else:
        print(f"message {result.id} failed")


if __name__ == "__main__":
    main()
