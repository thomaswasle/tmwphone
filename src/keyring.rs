use std::collections::HashMap;

use libsecret::{Schema, SchemaAttributeType, SchemaFlags};

fn schema() -> Schema {
    Schema::new(
        "net.loca.TMWPhone",
        SchemaFlags::NONE,
        HashMap::from([("service", SchemaAttributeType::String)]),
    )
}

const ATTRS: [(&str, &str); 1] = [("service", "sip-account")];
const LABEL: &str = "TMWPhone SIP password";

pub fn save(password: &str) -> Result<(), glib::Error> {
    libsecret::password_store_sync(
        Some(&schema()),
        HashMap::from(ATTRS),
        None,
        LABEL,
        password,
        gio::Cancellable::NONE,
    )
}

pub fn load() -> Option<String> {
    libsecret::password_lookup_sync(
        Some(&schema()),
        HashMap::from(ATTRS),
        gio::Cancellable::NONE,
    )
    .ok()
    .flatten()
    .map(|s| s.to_string())
}

pub fn clear() -> Result<(), glib::Error> {
    libsecret::password_clear_sync(
        Some(&schema()),
        HashMap::from(ATTRS),
        gio::Cancellable::NONE,
    )
}
