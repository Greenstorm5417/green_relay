"""Read received messages by polling the inbound endpoint (sync way #1)."""

from __future__ import annotations

from _common import build_client


def main() -> None:
    client = build_client()
    messages = client.messages.list_inbound(limit=20)

    if not messages:
        print("no inbound messages")
        return

    for msg in messages:
        when = msg.received_at.isoformat() if msg.received_at else "unknown time"
        print(f"[{when}] {msg.from_number}: {msg.body}")


if __name__ == "__main__":
    main()
