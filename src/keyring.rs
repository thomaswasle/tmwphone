use std::collections::HashMap;

use libsecret::{Schema, SchemaAttributeType, SchemaFlags};

fn schema() -> Schema {
    Schema::new(
        "io.github.thomaswasle.TMWPhone",
        SchemaFlags::NONE,
        HashMap::from([("service", SchemaAttributeType::String)]),
    )
}

const LABEL: &str = "TMWPhone SIP password";

// ── Per-account API (service attribute = account ID) ─────────────────────────

pub fn save_for(account_id: &str, password: &str) -> Result<(), glib::Error> {
    libsecret::password_store_sync(
        Some(&schema()),
        HashMap::from([("service", account_id)]),
        None,
        LABEL,
        password,
        gio::Cancellable::NONE,
    )
}

pub fn load_for(account_id: &str) -> Option<String> {
    libsecret::password_lookup_sync(
        Some(&schema()),
        HashMap::from([("service", account_id)]),
        gio::Cancellable::NONE,
    )
    .ok()
    .flatten()
    .map(|s| s.to_string())
}

pub fn clear_for(account_id: &str) -> Result<(), glib::Error> {
    libsecret::password_clear_sync(
        Some(&schema()),
        HashMap::from([("service", account_id)]),
        gio::Cancellable::NONE,
    )
}

// ── Legacy single-account API (kept for migration) ───────────────────────────

const LEGACY_ATTRS: [(&str, &str); 1] = [("service", "sip-account")];

pub fn load() -> Option<String> {
    libsecret::password_lookup_sync(
        Some(&schema()),
        HashMap::from(LEGACY_ATTRS),
        gio::Cancellable::NONE,
    )
    .ok()
    .flatten()
    .map(|s| s.to_string())
}
