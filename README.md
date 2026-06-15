# Green Relay — SMS Microservice

Green Relay turns a [Waveshare SIM7600G-H 4G DONGLE](https://www.waveshare.com/wiki/SIM7600G-H_4G_DONGLE)
into a self-hosted SMS REST API. It is an asynchronous Rust daemon that sends
and receives SMS through the dongle over AT commands and exposes an
API-key-authenticated REST API, a real-time Server-Sent Events stream, a
synchronous send endpoint, and an Argon2-secured admin dashboard, persisting
state in SQLite. It is designed to run as a `systemd` service on a Raspberry Pi.

Repository: <https://github.com/Greenstorm5417/green_relay>
API docs (Swagger UI): <https://greenstorm5417.github.io/green_relay/>

## Hardware: SIM7600G-H 4G DONGLE

The [SIM7600G-H 4G DONGLE](https://www.waveshare.com/wiki/SIM7600G-H_4G_DONGLE)
is a global-band LTE Cat-4 modem on a USB stick with a SIM slot. When plugged
in (directly or via the dongle's USB cable) it enumerates as a set of USB
serial (CDC-ACM) devices on Linux — typically `/dev/ttyUSB2` is the AT-command
port. The service is built entirely around that interface:

- **One owner of the port.** A single Modem Manager task opens the configured
  serial device at 115200 baud and serializes every AT exchange, so at most one
  command is ever outstanding. This avoids the data races that plague modems
  shared by multiple callers.
- **REST in, AT commands out.** Send requests are validated, segmented if
  longer than one SMS, and transmitted with `AT+CMGF=1` (text mode) followed by
  `AT+CMGS`. Delivery results (`+CMGS`, `+CMS/+CME ERROR`) update the message
  record. Inbound messages arrive as `+CMTI` unsolicited result codes, which the
  manager reads with `AT+CMGR` and deletes with `AT+CMGD` only after the message
  is durably persisted.
- **Health from the modem.** `/health` and `/status` surface SIM state
  (`AT+CPIN?`), registration (`AT+CREG?`), signal (`AT+CSQ`), and operator
  (`AT+COPS?`) so you can monitor the link.
- **Resilient connection.** If the serial device disappears the manager
  reconnects with exponential backoff and re-runs the SMS initialization
  sequence.

Insert an activated SIM (PIN disabled, or set it in config), plug the dongle in,
point `SERIAL_PORT` at the AT device (default `/dev/ttyUSB2`), and the REST API
is your SMS gateway. See the Waveshare wiki for driver/enumeration details and
antenna setup.

## Layout

- `service/` — the Rust service (binary + library, package `green_relay`).
- `web-ui/` — optional Next.js admin UI (bundled via the `web-ui` cargo feature).
- `scripts/` — build/packaging helpers.
- `docs/` — Bun project that builds a self-contained Swagger UI site (published to GitHub Pages).

## Building

```sh
git clone https://github.com/Greenstorm5417/green_relay.git
cd green_relay
# Debug build
scripts/build.sh
# Release build
scripts/build.sh --release
# Release build with the bundled web UI (requires Bun)
scripts/build.sh --release --web-ui
```

On Windows use `scripts/build.ps1` with the same switches.

Linux builds require `libudev-dev` and `pkg-config` (the serial-port dependency
links libudev).

## API documentation

The service serves its OpenAPI 3.1 document at `/api-docs/openapi.json`. Only
the public routes are documented; the admin dashboard is excluded. Emit the
spec without running the server:

```sh
cd service && cargo run --bin green_relay -- openapi > openapi.json
```

On each tagged release the CI pipeline regenerates the spec and publishes a
Swagger UI to GitHub Pages.

## Observability

The daemon is built to be monitored remotely through three surfaces:

- **Structured logs.** Every event is emitted as a single JSON line to stdout
  (captured by `journald` under systemd). Set `LOG_DIR` to also write rotating
  log files; `LOG_ROTATION` (`MINUTELY`/`HOURLY`/`DAILY`/`NEVER`) and
  `LOG_MAX_FILES` control rollover and retention. `LOG_LEVEL` sets the minimum
  severity. File writes go through a non-blocking writer, off the request path.
- **Health and status.** `GET /health` returns `200` (healthy/degraded) or
  `503` (unhealthy) with a JSON body; `GET /status` reports signal, registration,
  and operator. Both are unauthenticated.
- **Prometheus metrics.** `GET /metrics` exposes counters and gauges in
  Prometheus text exposition format (`text/plain; version=0.0.4`):

  | Metric | Type | Meaning |
  | --- | --- | --- |
  | `green_relay_auth_failures_total` | counter | Rejected auth attempts (invalid, unknown, or locked out) |
  | `green_relay_rate_limited_total` | counter | Requests rejected by the per-key rate limiter |
  | `green_relay_messages_accepted_total` | counter | Outbound messages accepted and queued |
  | `green_relay_messages_sent_total` | counter | Outbound messages confirmed sent |
  | `green_relay_messages_failed_total` | counter | Outbound messages that failed delivery |
  | `green_relay_modem_serial_connected` | gauge | Serial port connected (1) or not (0) |
  | `green_relay_modem_registered` | gauge | Registered to a network (1) or not (0) |
  | `green_relay_modem_signal_percent` | gauge | Signal strength 0–100 (absent when unknown) |

  Example Prometheus scrape config:

  ```yaml
  scrape_configs:
    - job_name: green_relay
      static_configs:
        - targets: ["127.0.0.1:8080"]
  ```

  `/metrics` is unauthenticated, like `/health` and `/status`. Scrape it over a
  trusted/internal network, or restrict access at the reverse proxy or with a
  firewall rule; do not expose it directly to the public internet.

## Releases and packaging

Pushing a `v*` tag triggers the release pipeline, which:

1. Cross-builds binaries and `.deb` packages for `x86_64`, `aarch64` (64-bit
   Raspberry Pi OS), and `armv7` (32-bit, best-effort).
2. Publishes the artifacts to a GitHub Release.
3. Regenerates the OpenAPI spec and deploys the docs to GitHub Pages.

### Installing on a Raspberry Pi

```sh
sudo dpkg -i green_relay_<version>_arm64.deb
sudo cp /etc/green_relay/config.example.yaml /etc/green_relay/config.yaml
sudo "$EDITOR" /etc/green_relay/config.yaml
sudo systemctl enable --now green_relay
```

The service runs as a hardened transient user (`DynamicUser`) with its SQLite
database under `/var/lib/green_relay`, and joins the `dialout` group to reach
the modem's serial device.

## Continuous integration

Pushes to `main` run `cargo fmt` (auto-committed back), `cargo clippy`, and
the full test suite. A supply-chain gate (`cargo deny check` — advisories,
yanked crates, licenses, and sources) and a coverage report
(`cargo llvm-cov`, uploaded as an artifact) run alongside. See
`.github/workflows/`.
