# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`tmwphone` (TMWPhone) is a GNOME SIP softphone for Linux, written in Rust with GTK4 + libadwaita. It uses sofia-sip for SIP signaling (via a C glue layer) and GStreamer for RTP audio.

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

`build.rs` runs `glib-compile-schemas data/` on every build, so changes to `data/io.github.thomaswasle.TMWPhone.gschema.xml` are picked up automatically. It also compiles `src/sip/glue.c` via the `cc` crate.

Unit tests live in `#[cfg(test)]` modules inside the source files and run with `cargo test`. They cover the pure-logic layers — `call_log.rs` (parsing, display helpers, save/load round-trip), `accounts.rs` (transport mapping, serde defaults, legacy port migration), and `sip/mod.rs` (`friendly_call_failure`, `parse_media_aux`). The GTK/sofia/GStreamer layers are not unit-tested. Note `cargo test` still links the full sofia-sip C stack, so the system deps from `install-deps.sh` are required.

## Architecture

### Layer overview

```
window.rs          — MainWindow: top-level UI orchestration, manages Vec<ActiveEngine>
├── sip/mod.rs     — SipEngine: Rust wrapper around the C SIP stack
│   ├── sip/ffi.rs — unsafe extern "C" declarations
│   └── sip/glue.c — sofia-sip NUA integration (SIP signaling, SDP, digest auth)
├── audio.rs       — AudioSession: GStreamer RTP pipelines (send + recv)
├── ringer.rs      — Ringer: GStreamer tone-generator for incoming/ringback tones
├── call_log.rs    — CallLog: persistent call history (newest-first, max 500 records)
├── accounts.rs    — Account config: load/save JSON, migrate from GSettings
├── keyring.rs     — libsecret wrapper for SIP password storage
└── widgets/       — Composite GTK4 widgets (CallScreen, Dialpad, SettingsDialog)
```

### SIP layer (`src/sip/`)

- **`glue.c`** is the only place that touches sofia-sip. It manages a `SofiaCtx` struct and fires simple integer events (`SOFIA_EV_*`) to Rust via a C callback.
- The NUA stack runs on the GLib main loop (`su_glib_root_create` + `g_source_attach`), so all `sofia_event_cb` invocations arrive on the GTK main thread.
- Transport (UDP / TCP / TLS), outbound proxy, server port, and TLS options are set at context creation time via `sofia_ctx_create()` and cannot be changed without destroying and recreating the engine.
- For TLS, the `tls_verify` flag controls certificate verification; `tls_ca_file` (if non-empty) overrides the system CA store.
- SDP is built and parsed manually (`build_audio_sdp`, `extract_rtp_from_sip`) — `NUTAG_MEDIA_ENABLE(0)` disables sofia's own SDP handling.
- Digest auth for INVITE is done manually (`invite_with_digest`) because `nua_authenticate` is broken in libsofia-sip-ua 1.12.11.
- DTMF is sent via SIP INFO (`application/dtmf-relay`), not RFC 2833 RTP.
- Blind transfer uses SIP REFER (`sofia_blind_transfer`). Attended transfer works via a consultation call: `sofia_start_consultation` puts the primary call on hold and dials a second leg; `sofia_complete_transfer` issues REFER to connect the two parties; `sofia_cancel_consultation` hangs up the consult leg and resumes the held call. Consultation events (`SOFIA_EV_CONSULT_*`) mirror the primary call events.
- An in-progress consultation leg is dropped (`drop_consult_leg`, BYE if established else CANCEL) whenever the primary call goes away so it cannot orphan — on explicit local hangup (`sofia_hangup`) and when the primary's own dialog ends (`nua_i_bye`/`nua_i_error` primary branch). The one exception is an attended transfer in flight: `sofia_complete_transfer` sets `transfer_in_progress`, which suppresses the self-BYE because the Replaces in the REFER atomically terminates the consult leg — a parallel BYE would race it. The flag is cleared when the transfer concludes (NOTIFY sipfrag, or REFER failure) and defensively reset at every call/consult entry point (`sofia_call`, `sofia_answer`, `sofia_start_consultation`).
- **`mod.rs`** converts C events into `SipEvent` enum values and invokes a closure directly on the GTK main thread (sofia NUA callbacks arrive on the main thread via the GLib event loop).

### Audio layer (`src/audio.rs`)

- `AudioSession::start()` builds two independent GStreamer pipelines: receive and send.
- The receive pipeline is brought to `State::Ready` first to bind the UDP port, then the bound `gio::Socket` is read from `udpsrc` and passed to `udpsink` in the send pipeline. This ensures both pipelines share one local port, satisfying Asterisk's symmetric RTP / comedia requirement.
- `rtpjitterbuffer latency=50` + `autoaudiosink sync=false` avoids a ~30 s startup silence caused by RTP timestamp mismatch.
- Codec is negotiated by the SIP layer and passed as a `u8` (0 = PCMU, 8 = PCMA).

### Ringer (`src/ringer.rs`)

- `Ringer` owns a GStreamer pipeline (`audiotestsrc → volume → audioconvert → audioresample → autoaudiosink`) and drives a cadence via chained `glib::timeout_add_local` callbacks that toggle the `volume` element.
- `Ringer::start_incoming()` — 440 Hz, cadence 400/200/400/2000 ms (on/off/on/off).
- `Ringer::start_ringback()` — 425 Hz, cadence 1000/3000 ms (on/off).
- RAII: `Drop` sets `alive = false` (stops the cadence loop) and sets the pipeline to `State::Null`. `MainWindow` holds `ringer: RefCell<Option<Ringer>>`; setting it to `None` stops the tone immediately.
- Do **not** set `is-live=true` on `audiotestsrc` or `sync=false` on `autoaudiosink` — both cause glitching on a local tone generator (unlike the RTP pipelines where `sync=false` is required).

### Accounts (`src/accounts.rs`)

- `Account` struct fields: `id` (hex timestamp + counter), `display_name`, `username`, `server`, `port` (u16, default 5060), `proxy` (outbound proxy host, empty = none), `transport` (`Transport` enum: `Udp` / `Tcp` / `Tls`, default `Udp`), `tls_verify` (bool), `tls_ca_file` (path to PEM CA, used when `tls_verify` is true and `transport == Tls`), `register_on_startup`.
- `Transport::default_port()` returns 5061 for TLS, 5060 otherwise. `Transport::as_c_int()` maps to the `TRANSPORT_*` constants in `glue.h`.
- Persisted as JSON at `~/.local/share/tmwphone/accounts.json`. `load()` / `save()` are the only public API besides `Account::new()` and `Account::label()`.
- `load()` includes a migration path for old entries where `port` was embedded in `server` as `"host:port"` — it splits them on the first load.
- On first run (no accounts.json), `migrate_from_gsettings()` reads `sip-username`, `sip-server`, `sip-display-name`, `sip-port` from GSettings, builds a single `Account`, saves it, and returns it. The GSettings SIP keys are kept only for this one-time migration.
- `window.rs` maintains `active_engines: RefCell<Vec<ActiveEngine>>` — one `SipEngine` per registered account, identified by `account_id`.

### Call log (`src/call_log.rs`)

- `CallLog` persists call records to `~/.local/share/tmwphone/calls.log` (pipe-delimited: `timestamp|direction|status|number|duration_secs`). Newest-first; capped at 500 entries.
- `CallLog::push()` inserts at index 0 and immediately rewrites the file.
- Display helpers: `display_name(raw)` extracts a human-readable label from a raw From-URI or dialled number; `callable(raw)` extracts the dialable address for call-back; `format_time` and `format_duration` format timestamps and durations for the UI.

### Keyring (`src/keyring.rs`)

- Thin wrapper around `libsecret` (`libsecret::password_store/lookup/clear_sync`) using schema `io.github.thomaswasle.TMWPhone` with attribute `service = "sip-account"`.
- `keyring::save`, `keyring::load`, `keyring::clear` — called from `SettingsDialog` (save/load) and `MainWindow` (load on startup).
- The SIP password is **no longer stored in GSettings** — the `sip-password` key was removed from the schema.

### UI layer

- **`window.rs`** (`MainWindow`) manages `Vec<ActiveEngine>` (one `SipEngine` per account), one optional `AudioSession` for the primary call, and one optional `consult_session: RefCell<Option<AudioSession>>` for the consultation leg during attended transfer.
- **`widgets/call_screen.rs`** (`CallScreen`) emits custom GLib signals: `answer-clicked`, `hangup-clicked`, `mute-toggled`, `hold-toggled`, `dtmf-digit`; and transfer/consultation signals: `transfer-blind-requested(number)`, `consult-requested(number)`, `transfer-complete-requested`, `consult-cancel-requested`. The transfer UI (entry + blind/consult buttons) is revealed by a `transfer_button` toggle; `consult_revealer` shows complete/cancel buttons during a live consultation.
- **`widgets/dialpad.rs`** (`Dialpad`) emits `call-requested(number, account_id)`. The account selector `DropDown` is hidden when only one account is registered. Pressing Enter in the number entry triggers dialling via `on_entry_activate` → `on_call_clicked_inner` (same path as the call button).
- Each widget is a GObject subclass using `#[derive(CompositeTemplate)]` bound to a `.ui` file in `data/ui/`.
- CSS for the call screen overlay is loaded in `application.rs` `startup()` via `gtk4::CssProvider`.

### GSettings

Schema: `io.github.thomaswasle.TMWPhone` (`data/io.github.thomaswasle.TMWPhone.gschema.xml`). Keys: `sip-server`, `sip-username`, `sip-display-name`, `sip-port` (all kept only for one-time migration to `accounts.json`), `audio-input-device` (integer, -1 = system default), `audio-output-device` (integer, -1 = system default). SIP account configuration is now stored in `~/.local/share/tmwphone/accounts.json` (see Accounts section). SIP passwords are stored in the system keyring via `src/keyring.rs`. The `src/config.rs` wrapper is unused.

## Key constraints

- The entire application runs on the GTK main thread. `SipEngine` is explicitly not `Send`.
- sofia-sip's `su_root` is attached to the default GLib main context — never call sofia-sip APIs from a background thread.
- `udpsrc` must reach `State::Ready` before `udpsink` is constructed, so the socket can be shared (see audio layer above).
- The SIP Contact header (built by `build_contact`, sent via `SIPTAG_CONTACT_STR` on every INVITE) **must** carry the bound SIP transport port (`ctx->local_sip_port`, the ephemeral port chosen for `NUTAG_URL`). NUA binds to an ephemeral port, not 5060, so a port-less Contact makes the peer route in-dialog requests — especially a remote-initiated BYE — to `:5060`, where nothing listens; the BYE is lost and the call never ends locally. Earlier code omitted the Contact entirely (sofia 1.13 drops it on some paths) or sent it without a port — both are wrong.
