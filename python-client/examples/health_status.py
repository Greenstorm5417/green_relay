"""Check service health and modem status (unauthenticated endpoints)."""

from __future__ import annotations

from _common import build_client


def main() -> None:
    client = build_client()

    health = client.health.get()
    print(f"health: {health.health}")
    print(f"serial connected: {health.serial_connected}")
    print(f"SIM status: {health.sim_status}")

    status = client.health.status()
    print(f"signal: {status.signal_percent}%")
    print(f"operator: {status.operator}")
    if status.unavailable:
        print(f"unavailable: {', '.join(status.unavailable)}")


if __name__ == "__main__":
    main()
