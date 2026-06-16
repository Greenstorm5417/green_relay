# green-relay (Python client)

A small, typed Python client for the Green Relay SMS microservice API. The
design follows the same shape as the OpenAI Python client: one `GreenRelay`
object configured with a base URL and API key, with endpoints grouped into
resource namespaces (`messages`, `health`, `events`).

## Install

```bash
uv sync
```

The client uses [`requests`](https://requests.readthedocs.io/) for HTTP.

## Quick start

```python
from green_relay import GreenRelay

client = GreenRelay(api_key="your-key", base_url="http://localhost:8080")

# Queue a message (returns immediately, 202)
resp = client.messages.send(to="+14155552671", body="Hello")
print(resp.id, resp.status, resp.parts)

# Send and wait for the delivery outcome
result = client.messages.send_sync(to="+14155552671", body="Hello")
if result.queued:
    print("still queued:", result.id)
else:
    print("delivered:", result.status, result.reference)

# Look up an outbound message
msg = client.messages.get(resp.id)
print(msg.status, msg.error_code)
```

## Reading messages: two ways

### 1. Synchronous

Poll inbound messages, or iterate the event stream directly:

```python
# Polling
for inbound in client.messages.list_inbound(limit=50):
    print(inbound.from_number, inbound.body)

# Blocking iterator over the live event stream
for event in client.events.stream():
    print(event.name, event.data)
```

### 2. Callback / event listener

Run the stream in a background thread and dispatch to callbacks:

```python
def on_sms(event):
    print("inbound:", event.from_, event.body)

def on_status(event):
    print("status:", event.id, event.status)

listener = client.events.listen(
    on_inbound_sms=on_sms,
    on_message_status=on_status,
)

# ... do other work ...

listener.stop()
```

## Health and status

```python
health = client.health.get()        # works even when degraded/unhealthy
print(health.health, health.serial_connected)

status = client.health.status()
print(status.signal_percent, status.operator)
```

## Error handling

Transport problems (connection refused, timeouts, DNS failures) propagate as
the standard [`requests`](https://requests.readthedocs.io/en/latest/api/#exceptions)
exceptions. Invalid arguments raise `ValueError`. A non-success HTTP response
raises a single `APIError` that mirrors the service's JSON error body (`error`
message + offending `fields`), with `retry_after` populated on 429/503.

```python
import requests
from green_relay import APIError

try:
    client.messages.send(to="bad", body="hi")
except APIError as exc:
    print(exc.status_code, exc.error, exc.fields)
    if exc.retry_after is not None:
        print("retry after", exc.retry_after, "seconds")
except requests.RequestException as exc:
    print("could not reach the server:", exc)
```

## Logging

The library follows the standard convention: it attaches a `NullHandler` and
emits only `DEBUG` request logs under the `green_relay` logger. Configure
logging in your application to see them:

```python
import logging
logging.basicConfig(level=logging.DEBUG)
```

## Development

This project uses [uv](https://docs.astral.sh/uv/) for dependency management
and [ruff](https://docs.astral.sh/ruff/) for linting and formatting. Run
everything from the `python-client` directory.

Install dependencies (including dev tools):

```powershell
uv sync --all-groups
```

Lint, format, and test:

```powershell
uv run ruff check .          # lint
uv run ruff check --fix .    # lint and auto-fix
uv run ruff format .         # format
uv run ruff format --check . # verify formatting (used in CI)
uv run pytest                # run the test suite
```

CI runs all of these on pushes and pull requests that touch `python-client/`
via `.github/workflows/python-client.yml`. See [`tests/README.md`](tests/README.md)
for the test layout and [`examples/README.md`](examples/README.md) for runnable
examples.

## Releasing to PyPI

Publishing is automated by `.github/workflows/python-client-publish.yml`, which
builds the sdist and wheel with `uv build` and uploads them to PyPI using
[Trusted Publishing](https://docs.pypi.org/trusted-publishers/) (OIDC, no stored
API token).

One-time setup: on PyPI, add a trusted publisher for the `green-relay` project
pointing at this repository, the `python-client-publish.yml` workflow, and the
`pypi` environment.

To cut a release:

1. Bump the version in `pyproject.toml` (`uv version <new-version>` updates it).
2. Commit the bump.
3. Tag it with a `python-v` prefix matching the version and push the tag:

   ```powershell
   git tag python-v0.1.0
   git push origin python-v0.1.0
   ```

The workflow verifies the tag matches the `pyproject.toml` version, re-runs the
lint/format/test checks, builds, and publishes. It can also be run manually via
the Actions tab (`workflow_dispatch`).
