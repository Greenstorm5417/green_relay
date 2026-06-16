"""Queue a message for delivery and return immediately (async send)."""

from __future__ import annotations

from _common import build_client


def main() -> None:
    client = build_client()
    resp = client.messages.send(to="+14155552671", body="Hello from Green Relay")
    print(f"queued message {resp.id} ({resp.status.value}, {resp.parts} part(s))")

    # Poll the record to see how delivery progressed.
    msg = client.messages.get(resp.id)
    print(f"current status: {msg.status.value}")


if __name__ == "__main__":
    main()
