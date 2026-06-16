"""Shared setup for the example scripts.

Reads the base URL and API key from environment variables so the examples
never hard-code credentials:

    set GREEN_RELAY_BASE_URL=http://localhost:8080
    set GREEN_RELAY_API_KEY=your-key
"""

from __future__ import annotations

import os
import sys

from green_relay import GreenRelay


def build_client() -> GreenRelay:
    base_url = os.environ.get("GREEN_RELAY_BASE_URL")
    api_key = os.environ.get("GREEN_RELAY_API_KEY")

    if not base_url or not api_key:
        sys.exit("Set GREEN_RELAY_BASE_URL and GREEN_RELAY_API_KEY before running this example.")

    return GreenRelay(api_key=api_key, base_url=base_url)
