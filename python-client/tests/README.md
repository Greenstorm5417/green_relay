# Tests

Regression suite for the Green Relay client. The tests are fully offline:
`GreenRelay` accepts a `session` argument, so a `FakeSession` (defined in
`conftest.py`) replays canned responses and records every request instead of
touching the network.

## Run

From the `python-client` directory:

```powershell
uv run pytest
```

Run a single file or test:

```powershell
uv run pytest tests/test_client.py
uv run pytest tests/test_events.py::test_listen_dispatches_to_typed_callbacks
```

## Layout

| File | Covers |
| --- | --- |
| `conftest.py` | `FakeSession` / `FakeResponse` fakes and shared fixtures |
| `test_models.py` | Payload parsing, datetime handling, enum mapping |
| `test_sse.py` | Server-Sent Events parsing edge cases |
| `test_errors.py` | `APIError` formatting and attributes |
| `test_client.py` | URL building, headers, error mapping, retry-after, timeouts |
| `test_resources_messages.py` | send / send_sync / get / list_inbound |
| `test_resources_health.py` | health (incl. 503 body) and status |
| `test_events.py` | stream iterator and background callback listener |
