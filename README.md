# Green Relay — SMS Microservice

An asynchronous Rust daemon that sends and receives SMS through a Waveshare
SIM7600X 4G USB modem over AT commands. It exposes an API-key-authenticated
REST API, a real-time Server-Sent Events stream, a synchronous send endpoint,
and an Argon2-secured admin dashboard, persisting state in SQLite. It is
designed to run as a `systemd` service on a Raspberry Pi.

Repository: <https://github.com/Greenstorm5417/green_relay>
API docs (Swagger UI): <https://greenstorm5417.github.io/green_relay/>

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

<!-- CI trigger -->
