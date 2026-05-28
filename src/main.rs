mod application;
mod audio;
mod config;
mod sip;
mod widgets;
mod window;

use gtk4::prelude::*;

fn main() {
    env_logger::init();

    // When running from a source checkout, point GSettings at our local schema.
    if cfg!(debug_assertions) {
        let schema_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data");
        std::env::set_var("GSETTINGS_SCHEMA_DIR", schema_dir);
    }

    let app = application::App::new();
    std::process::exit(app.run().into());
}
