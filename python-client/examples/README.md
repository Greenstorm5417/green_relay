# Examples

Runnable scripts demonstrating each part of the client. They read connection
details from environment variables so nothing is hard-coded.

## Setup

PowerShell:

```powershell
$env:GREEN_RELAY_BASE_URL = "http://localhost:8080"
$env:GREEN_RELAY_API_KEY  = "your-key"
```

cmd:

```cmd
set GREEN_RELAY_BASE_URL=http://localhost:8080
set GREEN_RELAY_API_KEY=your-key
```

Then run any example with uv (from the `python-client` directory):

```powershell
uv run examples/send_message.py
```

## Index

| Script | What it shows |
| --- | --- |
| `send_message.py` | Queue a message (async send, 202) and read its status back |
| `send_sync.py` | Send and block for the delivery outcome |
| `list_inbound.py` | Read received messages by polling (sync way to get messages) |
| `stream_events.py` | Read live events with the blocking iterator (sync way) |
| `listen_events.py` | Read live events with background callbacks (event-emitter way) |
| `health_status.py` | Check service health and modem status |
| `error_handling.py` | Handle `APIError` and transport errors |

## Two ways to get messages

- **Synchronous:** `list_inbound.py` (polling) and `stream_events.py` (blocking
  iterator over the event stream).
- **Callback / event emitter:** `listen_events.py` runs the stream on a
  background thread and dispatches each event to your callbacks.
