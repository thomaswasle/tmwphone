fn main() {
    // ── GSettings schema ──────────────────────────────────────────────────────
    println!("cargo:rerun-if-changed=data/io.github.thomaswasle.TMWPhone.gschema.xml");
    let ok = std::process::Command::new("glib-compile-schemas")
        .arg("data/")
        .status()
        .expect("glib-compile-schemas not found — install libglib2.0-bin");
    assert!(ok.success(), "glib-compile-schemas failed");

    // ── C glue layer ──────────────────────────────────────────────────────────
    println!("cargo:rerun-if-changed=src/sip/glue.c");
    println!("cargo:rerun-if-changed=src/sip/glue.h");

    // Probe for include paths only — suppress automatic cargo:rustc-link-lib
    // output so we can emit link directives AFTER cc::Build::compile().
    // This ensures libsofia_glue.a appears before the dynamic sofia libs in
    // the linker command line, which is required when ld uses --as-needed
    // (the linker skips a dynamic library if no unresolved references exist
    // at the point it is encountered, so the static archive must come first).
    let sofia = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("sofia-sip-ua")
        .expect("sofia-sip-ua not found — install libsofia-sip-ua-dev");
    let sofia_glib = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("sofia-sip-ua-glib")
        .expect("sofia-sip-ua-glib not found — install libsofia-sip-ua-glib-dev");

    cc::Build::new()
        .file("src/sip/glue.c")
        .includes(&sofia.include_paths)
        .includes(&sofia_glib.include_paths)
        .compile("sofia_glue");

    // Emit link search paths and library names after the static glue archive.
    for path in &sofia.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for lib in &sofia.libs {
        println!("cargo:rustc-link-lib={lib}");
    }
    for path in &sofia_glib.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for lib in &sofia_glib.libs {
        println!("cargo:rustc-link-lib={lib}");
    }
}
