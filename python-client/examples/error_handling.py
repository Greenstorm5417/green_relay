"""Demonstrate handling APIError and transport errors."""

from __future__ import annotations

import requests

from _common import build_client
from green_relay import APIError


def main() -> None:
    client = build_client()

    try:
        # An invalid phone number triggers a 400 with offending fields.
        client.messages.send(to="not-a-number", body="hi")
    except APIError as exc:
        print(f"API error {exc.status_code}: {exc.error}")
        if exc.fields:
            print(f"  offending fields: {', '.join(exc.fields)}")
        if exc.retry_after is not None:
            print(f"  retry after {exc.retry_after}s")
    except requests.RequestException as exc:
        print(f"could not reach the server: {exc}")


if __name__ == "__main__":
    main()
