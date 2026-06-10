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
the full test suite. See `.github/workflows/`.
