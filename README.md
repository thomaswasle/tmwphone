# TMWPhone

A SIP softphone for the GNOME desktop, written in Rust.

![TMWPhone screenshot](data/icons/net.loca.TMWPhone.svg)

## Features

- Outgoing and incoming SIP calls
- Multiple SIP accounts (simultaneous registration; account selector shown when more than one is active)
- UDP, TCP, and TLS transport (per account)
- Configurable server port and outbound proxy (per account)
- TLS certificate verification with optional custom CA file
- Hold / resume (re-INVITE with `a=sendonly` / `a=sendrecv`)
- Blind transfer (SIP REFER)
- Attended transfer via consultation call (hold + dial, then complete or cancel)
- DTMF via SIP INFO (`application/dtmf-relay`)
- Mute (local audio suppression)
- In-call keypad
- Dial by pressing Enter in the number field
- Password stored in GNOME Keyring via libsecret
- Native GNOME look using GTK4 + libadwaita

## Requirements

**Runtime**

| Package | Purpose |
|---|---|
| `gstreamer1.0-plugins-good` | RTP, UDP, audio codecs (PCMU/PCMA) |
| `gstreamer1.0-plugins-base` | Audio pipeline base elements |
| `gstreamer1.0-pulseaudio` | Audio I/O via PulseAudio / PipeWire |

**Build** (Ubuntu / Debian)

```
libgtk-4-dev  libadwaita-1-dev  libsofia-sip-ua-dev  libsofia-sip-ua-glib-dev
libglib2.0-dev  libglib2.0-bin  libgstreamer1.0-dev
libgstreamer-plugins-base1.0-dev  libsecret-1-dev  pkg-config
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

## Running

```bash
cargo run                    # debug build, auto-sets GSETTINGS_SCHEMA_DIR
RUST_LOG=debug cargo run     # with logging
```

On first run, click **Configure** in the banner, enter your SIP server,
username, and password, then click **Connect**. Credentials are saved:
subsequent runs show a **Connect** button that registers immediately.

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
├── audio.rs           — AudioSession: two GStreamer pipelines (send + recv RTP)
├── sip/
│   ├── mod.rs         — SipEngine Rust wrapper, SipEvent enum, C callback bridge
│   ├── ffi.rs         — unsafe extern "C" declarations
│   └── glue.c         — sofia-sip NUA integration (SDP, digest auth, hold, DTMF)
└── widgets/
    ├── call_screen.rs — overlay shown during a call (answer, hang up, mute, hold, keypad)
    ├── dialpad.rs     — main dialler; account selector DropDown; Enter key dials
    └── settings_dialog.rs — SIP account preferences (add / edit / remove accounts)
```

**SIP stack** — sofia-sip runs on the GLib main loop
(`su_glib_root_create` + `g_source_attach`), so all callbacks arrive on the
GTK main thread. SDP is built and parsed manually; sofia's own media handling
is disabled (`NUTAG_MEDIA_ENABLE(0)`). Digest auth for INVITE is implemented
manually because `nua_authenticate` is broken in libsofia-sip-ua 1.12.11.
Transport (UDP/TCP/TLS), outbound proxy, port, and TLS options are set at
engine creation and require an engine restart to change. Blind and attended
transfer are implemented via SIP REFER / consultation INVITE.

**Audio** — The receive pipeline (`udpsrc`) is brought to `State::Ready` first
to bind the local UDP socket; that socket is then shared with `udpsink` in the
send pipeline. This satisfies Asterisk's symmetric-RTP / comedia requirement.
`rtpjitterbuffer latency=50` and `autoaudiosink sync=false` prevent the ~30 s
startup silence caused by RTP timestamp mismatch.

## License

Apache-2.0 © 2026 Thomas Müller-Wasle
