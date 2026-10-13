# TMWPhone

A SIP softphone for the GNOME desktop, written in Rust.

![TMWPhone screenshot](data/icons/io.github.thomaswasle.TMWPhone.svg)

## Status

I designed this app for my personal needs. There might be missing a lot of features. Also some features may not work as expected. If you miss something or if you find a bug do not hesitate to open an issue or pr!

## Features

- Outgoing and incoming SIP calls
- Multiple SIP accounts (simultaneous registration; account selector shown when more than one is active)
- UDP, TCP, TLS, WebSocket (WS) and secure WebSocket (WSS) transport (per account; SIP over WebSocket per RFC 7118)
- Configurable server port, WebSocket path and outbound proxy (per account)
- TLS / WSS certificate verification with optional custom CA file
- Optional WebRTC media (DTLS-SRTP + ICE) per account, independent of transport — Opus, PCMU and PCMA, codec chosen in the peer's order
- Hold / resume (re-INVITE with `a=sendonly` / `a=sendrecv`)
- Blind transfer (SIP REFER)
- Attended transfer via consultation call (hold + dial, then complete or cancel; plain-RTP accounts only)
- DTMF via SIP INFO (`application/dtmf-relay`)
- Mute (local audio suppression)
- In-call keypad
- Dial by pressing Enter in the number field
- Quickdial sidebar: configurable, reorderable speed-dial buttons (label + number)
- Call history (Recents) with call-back, persisted across sessions (last 500 calls)
- Ringtone and ringback tones
- Secondary ringer output (ring simultaneously on a second audio device, e.g. speakers while voice goes through headphones)
- Password stored in GNOME Keyring via libsecret
- Native GNOME look using GTK4 + libadwaita

## Requirements

**Runtime**

| Package | Purpose |
|---|---|
| `gstreamer1.0-plugins-good` | RTP, UDP, audio codecs (PCMU/PCMA) |
| `gstreamer1.0-plugins-base` | Audio pipeline base elements |
| `gstreamer1.0-pulseaudio` | Audio I/O via PulseAudio / PipeWire |
| `gstreamer1.0-plugins-bad` | `webrtcbin` (WebRTC media only) |
| `gstreamer1.0-nice` | ICE for `webrtcbin` (WebRTC media only) |

**Build** (Ubuntu / Debian)

```
libgtk-4-dev  libadwaita-1-dev  libsofia-sip-ua-dev  libsofia-sip-ua-glib-dev
libglib2.0-dev  libglib2.0-bin  libgstreamer1.0-dev
libgstreamer-plugins-base1.0-dev  libgstreamer-plugins-bad1.0-dev
gstreamer1.0-nice  libsecret-1-dev  pkg-config  build-essential
```

Install all at once:

```bash
bash install-deps.sh
```

## Building

```bash
cargo build            # debug
cargo build --release  # optimised
```

`build.rs` compiles the sofia-sip C glue layer (`src/sip/glue.c`) and the
GSettings schema automatically on every build.

## Testing

```bash
cargo test
```

Unit tests cover the pure-logic layers (call log, accounts, SIP helpers, SDP
helpers, WebSocket framing). End-to-end tests run a real WebSocket bridge
against an in-process WebSocket server and negotiate two real WebRTC sessions
against each other through the DTLS-SRTP handshake. `cargo test` links the full
sofia-sip stack, so the build dependencies above are required.

## Running

```bash
cargo run                    # debug build, auto-sets GSETTINGS_SCHEMA_DIR
RUST_LOG=debug cargo run     # with logging
```

On first run, click **Configure** in the banner, add a SIP account (server,
username, password, transport), then click **Connect**. Accounts are saved to
`~/.local/share/tmwphone/accounts.json` and passwords to the GNOME Keyring;
subsequent runs show a **Connect** button that registers immediately.

User data lives in `~/.local/share/tmwphone/`:

| File | Contents |
|---|---|
| `accounts.json` | SIP accounts |
| `quickdials.json` | Quickdial entries |
| `calls.log` | Call history |

## Installing from .deb

```bash
dpkg-buildpackage -us -uc -b
sudo dpkg -i ../tmwphone_*.deb
```

## Architecture

```
src/
├── main.rs            — entry point, GSETTINGS_SCHEMA_DIR for dev builds
├── application.rs     — AdwApplication subclass, actions (quit, preferences, about)
├── window.rs          — MainWindow: manages Vec<ActiveEngine>, drives all state
├── accounts.rs        — Account config: load/save JSON, migrate from GSettings
├── keyring.rs         — libsecret helpers (save / load SIP password per account)
├── quickdial.rs       — Quickdial entries: load/save JSON
├── call_log.rs        — CallLog: persistent call history (newest-first, max 500)
├── audio.rs           — AudioSession: two GStreamer pipelines (send + recv RTP)
├── webrtc.rs          — WebrtcSession: webrtcbin media (DTLS-SRTP + ICE)
├── ringer.rs          — Ringer: GStreamer tone generator (incoming ring, ringback)
├── sip/
│   ├── mod.rs         — SipEngine Rust wrapper, SipEvent enum, C callback bridge
│   ├── ffi.rs         — unsafe extern "C" declarations
│   ├── glue.c / .h    — sofia-sip NUA integration (SDP, digest auth, hold, DTMF, transfer)
│   └── wsbridge.rs    — SIP-over-WebSocket bridge (RFC 7118 / RFC 6455)
└── widgets/
    ├── call_screen.rs — overlay shown during a call (answer, hang up, mute, hold, keypad)
    ├── dialpad.rs     — main dialler; account selector DropDown; Enter key dials
    └── settings_dialog.rs — preferences: accounts, quickdial entries, secondary ringer
```

**SIP stack** — sofia-sip runs on the GLib main loop
(`su_glib_root_create` + `g_source_attach`), so all callbacks arrive on the
GTK main thread. SDP is built and parsed manually; sofia's own media handling
is disabled (`NUTAG_MEDIA_ENABLE(0)`). Digest auth for INVITE is implemented
manually because `nua_authenticate` is broken in libsofia-sip-ua 1.12.11.
Transport (UDP/TCP/TLS/WS/WSS), outbound proxy, port, and TLS options are set
at engine creation and require an engine restart to change. Blind and attended
transfer are implemented via SIP REFER / consultation INVITE.

**WebSocket** — the sofia-sip 1.12 package has no WebSocket transport, so WS/WSS
is terminated in Rust: sofia talks plain TCP to a loopback bridge
(`wsbridge.rs`), which acts as its outbound proxy, rewrites the Via/Contact
headers (never the SDP body) and carries each SIP message in a WebSocket frame.
The Contact uses an unroutable `<token>.invalid` host, as RFC 7118 requires, so
in-dialog requests come back over the established connection.

**WebRTC media** — when enabled on an account, `webrtcbin` owns the SDP and the
SIP layer carries it verbatim. Opus, PCMU and PCMA are offered on one m-line;
the send chain is built only after negotiation, once the peer's codec choice is
known. ICE is vanilla (gathering completes before the offer/answer is sent),
since SIP has no carrier for trickled candidates.

**Audio** — The receive pipeline (`udpsrc`) is brought to `State::Ready` first
to bind the local UDP socket; that socket is then shared with `udpsink` in the
send pipeline. This satisfies Asterisk's symmetric-RTP / comedia requirement.
`rtpjitterbuffer latency=50` and `autoaudiosink sync=false` prevent the ~30 s
startup silence caused by RTP timestamp mismatch.

## License

Apache-2.0 © 2026 Thomas Müller-Wasle
