# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`locaclient` is a GNOME SIP softphone for Linux, written in Rust with GTK4 + libadwaita. It uses sofia-sip for SIP signaling (via a C glue layer) and GStreamer for RTP audio.

## Build & Run

```bash
# Install system dependencies (Ubuntu/Debian, one-time)
bash install-deps.sh

# Build
cargo build

# Run (debug build — sets GSETTINGS_SCHEMA_DIR automatically)
cargo run

# Run with logging
RUST_LOG=debug cargo run
```

`build.rs` runs `glib-compile-schemas data/` on every build, so changes to `data/net.loca.Client.gschema.xml` are picked up automatically. It also compiles `src/sip/glue.c` via the `cc` crate.

There are no tests currently.

## Architecture

### Layer overview

```
window.rs          — MainWindow: top-level UI orchestration, owns SipEngine + AudioSession
├── sip/mod.rs     — SipEngine: Rust wrapper around the C SIP stack
│   ├── sip/ffi.rs — unsafe extern "C" declarations
│   └── sip/glue.c — sofia-sip NUA integration (SIP signaling, SDP, digest auth)
├── audio.rs       — AudioSession: GStreamer RTP pipelines (send + recv)
└── widgets/       — Composite GTK4 widgets (CallScreen, Dialpad, SettingsDialog)
```

### SIP layer (`src/sip/`)

- **`glue.c`** is the only place that touches sofia-sip. It manages a `SofiaCtx` struct and fires simple integer events (`SOFIA_EV_*`) to Rust via a C callback.
- The NUA stack runs on the GLib main loop (`su_glib_root_create` + `g_source_attach`), so all `sofia_event_cb` invocations arrive on the GTK main thread.
- SDP is built and parsed manually (`build_audio_sdp`, `extract_rtp_from_sip`) — `NUTAG_MEDIA_ENABLE(0)` disables sofia's own SDP handling.
- Digest auth for INVITE is done manually (`invite_with_digest`) because `nua_authenticate` is broken in libsofia-sip-ua 1.12.11.
- DTMF is sent via SIP INFO (`application/dtmf-relay`), not RFC 2833 RTP.
- **`mod.rs`** converts C events into `SipEvent` enum values sent over an `async_channel`. `MainWindow` spawns a `glib::MainContext::spawn_local` task that reads from this channel.

### Audio layer (`src/audio.rs`)

- `AudioSession::start()` builds two independent GStreamer pipelines: receive and send.
- The receive pipeline is brought to `State::Ready` first to bind the UDP port, then the bound `gio::Socket` is read from `udpsrc` and passed to `udpsink` in the send pipeline. This ensures both pipelines share one local port, satisfying Asterisk's symmetric RTP / comedia requirement.
- `rtpjitterbuffer latency=50` + `autoaudiosink sync=false` avoids a ~30 s startup silence caused by RTP timestamp mismatch.
- Codec is negotiated by the SIP layer and passed as a `u8` (0 = PCMU, 8 = PCMA).

### UI layer

- **`window.rs`** (`MainWindow`) owns `SipEngine` and `AudioSession` in `RefCell`s, connects all widget signals, and drives state transitions in response to `SipEvent`s.
- **`widgets/call_screen.rs`** (`CallScreen`) emits custom GLib signals (`answer-clicked`, `hangup-clicked`, `mute-toggled`, `hold-toggled`, `dtmf-digit`) that `MainWindow` connects to SIP/audio methods.
- Each widget is a GObject subclass using `#[derive(CompositeTemplate)]` bound to a `.ui` file in `data/ui/`.
- CSS for the call screen overlay is loaded in `application.rs` `startup()` via `gtk4::CssProvider`.

### GSettings

Schema: `net.loca.Client` (`data/net.loca.Client.gschema.xml`). Keys: `sip-server`, `sip-username`, `sip-password` (plaintext, dev only — TODO: migrate to libsecret), `sip-display-name`, `sip-port`. Settings are read directly from `gio::Settings` in `window.rs`; the `src/config.rs` wrapper is unused.

## Key constraints

- The entire application runs on the GTK main thread. `SipEngine` is explicitly not `Send`.
- sofia-sip's `su_root` is attached to the default GLib main context — never call sofia-sip APIs from a background thread.
- `udpsrc` must reach `State::Ready` before `udpsink` is constructed, so the socket can be shared (see audio layer above).
- The SIP contact header must not include an explicit port (no `SIPTAG_CONTACT_STR`) — letting NUA auto-generate it avoids Asterisk trying to reach port 0.
