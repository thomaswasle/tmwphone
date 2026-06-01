mod application;
mod audio;
mod call_log;
mod config;
mod keyring;
mod ringer;
mod sip;
mod widgets;
mod window;

use gtk4::{glib, prelude::*};

fn main() {
    env_logger::init();

    // When running from a source checkout, point GSettings at our local schema.
    if cfg!(debug_assertions) {
        let schema_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data");
        std::env::set_var("GSETTINGS_SCHEMA_DIR", schema_dir);
    }

    let app = application::App::new();

    // std::process::exit() skips Rust destructors, so SipEngine::drop() and
    // sofia_ctx_destroy() never run, and REGISTER Expires:0 is never sent to
    // the registrar.  Asterisk keeps the old registration binding (old port)
    // alive.  The next run uses a new ephemeral port, but Asterisk routes
    // incoming calls to the dead old port — causing the binary per-session
    // failure where every call in the new session is silently dropped.
    //
    // Fix: intercept SIGINT (Ctrl+C) and SIGTERM with GLib signal handlers
    // that call app.quit().  app.quit() triggers the GTK shutdown sequence:
    // windows are destroyed, SipEngine is dropped, sofia sends REGISTER
    // Expires:0, and Asterisk removes the old binding cleanly.
    // SIGINT = 2, SIGTERM = 15 on all POSIX systems.
    for &sig in &[2i32, 15i32] {
        let app_weak = app.downgrade();
        glib::unix_signal_add_local(sig, move || {
            if let Some(app) = app_weak.upgrade() {
                app.quit();
            }
            glib::ControlFlow::Break
        });
    }

    let exit_code: i32 = app.run().into();

    // Drop the application explicitly BEFORE calling std::process::exit().
    // process::exit() bypasses Rust destructors, so without this drop(),
    // SipEngine::drop() → sofia_ctx_destroy() → nua_shutdown() never runs,
    // and REGISTER Expires:0 is never sent to the registrar.  Asterisk then
    // keeps all stale bindings (old ports from previous runs) alive until
    // their natural expiry, routing incoming calls to dead ports.
    //
    // drop(app) here decrements the GtkApplication ref count to zero,
    // triggering synchronous GObject finalization: Application → Window →
    // SipEngine → sofia_ctx_destroy(), which pumps the GLib main context
    // manually for up to 2 s to complete the REGISTER Expires:0 exchange.
    drop(app);

    std::process::exit(exit_code);
}
