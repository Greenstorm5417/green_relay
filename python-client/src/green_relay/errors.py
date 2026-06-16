"""Error type raised by the Green Relay client.

Connection, timeout, and other transport problems propagate as the standard
``requests`` exceptions. Invalid arguments raise ``ValueError``. The only
custom error is :class:`APIError`, which carries the structured error body the
service returns on a non-success response.
"""

from __future__ import annotations


class APIError(Exception):
    """The server returned a non-success HTTP status code.

    Mirrors the service's JSON error body (``error`` message plus offending
    ``fields``). ``retry_after`` holds the seconds to wait before retrying on
    429 and 503 responses, when present.
    """

    def __init__(
        self,
        status_code: int,
        error: str,
        fields: list[str] | None = None,
        retry_after: int | None = None,
    ) -> None:
        self.status_code = status_code
        self.error = error
        self.fields = fields or []
        self.retry_after = retry_after
        detail = error
        if self.fields:
            detail = f"{error} (fields: {', '.join(self.fields)})"
        super().__init__(f"[{status_code}] {detail}")
