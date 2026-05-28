fn main() {
    // ── GSettings schema ──────────────────────────────────────────────────────
    println!("cargo:rerun-if-changed=data/net.loca.Client.gschema.xml");
    let ok = std::process::Command::new("glib-compile-schemas")
        .arg("data/")
        .status()
        .expect("glib-compile-schemas not found — install libglib2.0-bin");
    assert!(ok.success(), "glib-compile-schemas failed");

    // ── C glue layer ──────────────────────────────────────────────────────────
    println!("cargo:rerun-if-changed=src/sip/glue.c");
    println!("cargo:rerun-if-changed=src/sip/glue.h");

    let sofia = pkg_config::probe_library("sofia-sip-ua")
        .expect("sofia-sip-ua not found — install libsofia-sip-ua-dev");
    let sofia_glib = pkg_config::probe_library("sofia-sip-ua-glib")
        .expect("sofia-sip-ua-glib not found — install libsofia-sip-ua-glib-dev");

    cc::Build::new()
        .file("src/sip/glue.c")
        .includes(&sofia.include_paths)
        .includes(&sofia_glib.include_paths)
        .compile("sofia_glue");
}
