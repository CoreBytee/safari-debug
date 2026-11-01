# safari-debug

Debug Safari on a USB-connected iPhone from Linux, without a Mac.

`safari-debug` is a single Rust binary that:

1. talks to `usbmuxd` / lockdownd over USB,
2. opens the device's `com.apple.webinspector` service,
3. translates between the Apple WebInspector RPC protocol and a normal browser WebSocket, and
4. serves the WebKit Web Inspector UI locally and proxies its WebSocket connection.

The result is a local web page where you can inspect Safari tabs, use the Elements,
Console, Sources, Network and other panels, all from Chrome/Chromium, Edge, or Firefox.
The UI, API, and WebSocket are served from the same origin, so no CORS extension is needed.

I tested this against an iPhone running iOS 27.2.

## Requirements

- Linux (the USB/lockdown stack is pure Rust; no `libimobiledevice` development
  packages are needed)
- `usbmuxd` running (`systemctl status usbmuxd` or check `/var/run/usbmuxd`)
- `git` on first run (to download the WebKit inspector assets)
- A modern browser
- An iPhone with **Settings → Safari → Advanced → Web Inspector** enabled
- The iPhone unlocked and trusted on this computer

## Build

```sh
cargo build --release
```

The binary is `target/release/safari-debug`.

## Quick start

```sh
# Optional: download the Web Inspector UI once into ~/.cache/safari-debug
./target/release/safari-debug frontend install

# Start the proxy and the local web server
./target/release/safari-debug serve
```

On first `serve`, if the frontend is missing, it is downloaded automatically.
Then open <http://127.0.0.1:9223> and click **Inspect** next to a Safari tab.

If you prefer to keep the frontend somewhere else:

```sh
./target/release/safari-debug serve \
  --frontend-dir /path/to/WebKit/Source/WebInspectorUI/UserInterface
```

## Commands

```
safari-debug serve [OPTIONS]
    -b, --bind <ADDR>       Listen address (default 127.0.0.1:9223)
    -d, --device <UDID>     Only proxy this device
        --frontend-dir <DIR> Use an existing WebKit UserInterface checkout
        --no-open           Do not launch a browser
        --no-auto-install   Fail instead of downloading the frontend
        --webkit-ref <REF>  WebKit ref to download (default: main)

safari-debug frontend install [--tag <REF>] [--force]
safari-debug frontend path
safari-debug devices
```

`safari-debug` with no subcommand is the same as `safari-debug serve`.

### Choosing a WebKit version

The frontend defaults to WebKit `main`. For a particular iOS release you can
install a matching WebKit release tag instead:

```sh
safari-debug frontend install \
  --tag releases/Apple/Safari-18.3-iOS-18.3 --force
```

Tags live at <https://github.com/WebKit/WebKit/tags>. The server automatically
picks the newest protocol file in `Protocol/Legacy/iOS` that is not newer than
the connected device's iOS version, so you normally do not need to care.

## Endpoints

| Path | Purpose |
| --- | --- |
| `/` | Device and tab list |
| `/api/devices` | JSON snapshot of devices and pages |
| `/api/refresh` | Ask the device to refresh its page listing |
| `/json` | iwdp-style device list |
| `/json/list` | iwdp-style page list |
| `/inspector/<udid>/<page>/` | WebKit Inspector UI |
| `/ws/<udid>/<page>` | Browser ↔ device WebSocket bridge |

The WebSocket endpoint speaks the WebKit Inspector Protocol directly. The
WebKit UI is served unmodified from the WebKit source tree, plus a small
browser-compatibility shim (`assets/additions.js` / `assets/additions.css`).

## How it works

```
iPhone
  │ USB
usbmuxd
  │
lockdownd ── start com.apple.webinspector
  │
webinspectord ── 4-byte length + binary plist RPC
  │
safari-debug
  │  page listing / forwardSocketSetup / forwardSocketData
  ▼
WebSocket ── WebKit Web Inspector UI in your browser
```

The important part is the WebInspector RPC layer:

- `_rpc_reportIdentifier:` identifies this inspector connection.
- `_rpc_forwardGetListing:` asks for an app's debuggable pages.
- `_rpc_applicationSentListing:` delivers the page list.
- `_rpc_forwardSocketSetup:` claims a page and assigns it a sender UUID.
- `_rpc_forwardSocketData:` carries the browser's JSON inspector messages.
- `_rpc_applicationSentData:` carries the page's JSON response/events back.
- `_rpc_forwardDidClose:` releases the page when the browser disconnects.

Modern iOS versions use raw binary plists with a 4-byte big-endian length
prefix. The proxy implements that framing directly.

A sparse WebInspectorUI checkout does not contain files that WebKit normally
generates during its build. The tool synthesizes
`Models/NativeFunctionParameters.js` from the checked-in
`NativeFunctionParameters-overrides.json`, so the inspector does not request a
missing script.

## Troubleshooting

**No device appears**

- Unlock the iPhone and tap **Trust** when prompted.
- Check `ls /var/run/usbmuxd` and `systemctl status usbmuxd`.
- If the landing page says **usbmuxd is not available**, reconnect the phone
  (udev normally starts usbmuxd) or run `sudo systemctl start usbmuxd`.
- Try unplugging and replugging the cable.
- `safari-debug devices` should print the phone and its current tabs.

**I unplugged the phone and the log stopped / the UI says no device**

That is expected. The proxy removes the device when usbmuxd no longer reports
it, cancels its WebInspector session, and waits for it to be plugged back in.
Transport disconnects such as `Connection refused` or `peer closed connection`
are logged at debug level; actionable failures (for example Web Inspector being
disabled) warn once per failure state instead of retrying forever. Run with
`--verbose` if you want to see the transport details.

**Device appears but no Safari tabs**

- Open a web page in Safari on the phone.
- Enable **Settings → Safari → Advanced → Web Inspector**.
- Keep the phone unlocked while debugging.
- Press **Refresh** on the landing page, or restart `serve`.

**“Web Inspector encountered an internal error”**

- Update the frontend: `safari-debug frontend install --force`.
- Make sure you are using a reasonably recent WebKit tag. The default `main`
  is usually best for current iOS versions.
- If you use Firefox, the included `assets/additions.js` polyfills
  `scrollIntoViewIfNeeded`, `isInsertionCaretInside`, and legacy CSS value APIs;
  Chrome/Chromium needs fewer shims.

**Firefox logs “Assertion failed: null” from TabBarItem.js**

The WebKit UI restores open tabs from a cookie. Stale state from an older
frontend can produce benign assertions. Clear site data for
`127.0.0.1:9223`, or open the inspector in a private window. The included
`isInsertionCaretInside` shim prevents the more serious focus crash that used
to follow these assertions in Firefox.

**Remote access / HTTPS**

The WebKit browser frontend connects with `ws://`, so use plain HTTP and an
SSH tunnel instead of exposing the port publicly:

```sh
ssh -L 9223:127.0.0.1:9223 user@machine
```

Then open <http://127.0.0.1:9223> locally.

**Page listing is slow or stale**

Click **Refresh** or `POST /api/refresh`. The proxy also polls usbmuxd every
two seconds and reconnects to `webinspectord` with backoff.

## Limitations

- Tested with iOS 27.2. Modern iOS uses raw binary-plist framing; very old iOS
  versions that wrap messages in `WIRPartialMessageKey`/`WIRFinalMessageKey`
  are not supported.
- The WebKit frontend connects with `ws://`, so serve it over HTTP. For remote
  access use an SSH tunnel rather than exposing the port.
- One browser tab can inspect a given Safari page at a time. Opening the same
  page in another tab may steal the inspector session.
- Some WebKit-specific inspector panels are less polished in Chromium/Firefox;
  the bundled shims fix the common cases.

## Development

```sh
cargo run -- devices
cargo run -- serve --frontend-dir /path/to/WebKit/Source/WebInspectorUI/UserInterface
RUST_LOG=safari_debug=debug cargo run -- serve
```

The main modules are:

- `src/device.rs` — usbmuxd discovery, per-device session, page/app state
- `src/webinspector.rs` — plist framing and RPC helpers
- `src/server.rs` — HTTP endpoints, static frontend, WebSocket bridge
- `src/frontend.rs` — WebKit frontend download/cache and protocol selection
- `assets/` — landing page and browser-compatibility shims

## Attribution

The WebKit Inspector Protocol implementation was written from scratch, using
[google/ios-webkit-debug-proxy](https://github.com/google/ios-webkit-debug-proxy)
(BSD-3-Clause) as the protocol reference. The idea of pairing the WebKit UI
with a small Rust proxy was inspired by
[besties/ios-safari-remote-debug](https://git.gay/besties/ios-safari-remote-debug)
and
[HimbeersaftLP/ios-safari-remote-debug-kit](https://github.com/HimbeersaftLP/ios-safari-remote-debug-kit).

The WebKit Web Inspector assets are **not** bundled with this project. They are
downloaded from the WebKit repository on first use and remain subject to
WebKit's license and copyright notices.
