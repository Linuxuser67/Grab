fn main() {
    println!("cargo:rerun-if-changed=data/io.github.linuxuser67.Grab.gschema.xml");
    println!("cargo:rerun-if-changed=data/grab.gresource.xml");
    println!("cargo:rerun-if-env-changed=GRAB_METAINFO_FILE");
    println!("cargo:rustc-env=GRAB_VERSION={}", env!("CARGO_PKG_VERSION"));
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let schema_dir = out_dir.join("grab-schemas/glib-2.0/schemas");
    let _ = std::fs::create_dir_all(&schema_dir);

    let src = std::path::Path::new("data/io.github.linuxuser67.Grab.gschema.xml");
    if src.exists() {
        let dst = schema_dir.join("io.github.linuxuser67.Grab.gschema.xml");
        if std::fs::copy(src, &dst).is_ok() {
            let status = std::process::Command::new("glib-compile-schemas")
                .arg(&schema_dir)
                .status();
            if !matches!(status, Ok(s) if s.success()) {
                eprintln!(
                    "cargo:warning=glib-compile-schemas failed; run with GSETTINGS_SCHEMA_DIR pointing at compiled schemas"
                );
            }
        }
    }
    println!("cargo:rustc-env=GRAB_SCHEMA_DIR={}", schema_dir.display());
    // About dialog reads name/version/notes from the metainfo via
    // from_appdata, which needs a GResource path. The bundle is embedded
    // into the binary with include_bytes!, so a failure here must fail the
    // build loudly instead of shipping an About dialog that aborts on open.
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let metainfo_file = std::env::var_os("GRAB_METAINFO_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("data/io.github.linuxuser67.Grab.metainfo.xml.in"));
    println!("cargo:rerun-if-changed={}", metainfo_file.display());
    let metainfo_parent = metainfo_file
        .parent()
        .expect("AppStream metainfo path must have a parent directory");
    let metainfo_name = metainfo_file
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .expect("AppStream metainfo filename must be UTF-8");
    let resource_template = std::fs::read_to_string(manifest_dir.join("data/grab.gresource.xml"))
        .expect("could not read the GResource manifest");
    let source_entry = "data/io.github.linuxuser67.Grab.metainfo.xml.in";
    assert_eq!(
        resource_template.matches(source_entry).count(),
        1,
        "GResource manifest must contain exactly one AppStream metainfo entry"
    );
    let resource_manifest = out_dir.join("grab.gresource.xml");
    std::fs::write(
        &resource_manifest,
        resource_template.replace(source_entry, metainfo_name),
    )
    .expect("could not write the generated GResource manifest");
    let gresource = out_dir.join("grab.gresource");
    let res_status = std::process::Command::new("glib-compile-resources")
        // The translated AppStream file is generated outside the source tree,
        // so its directory must be searched first. Keep the manifest root as
        // a second source directory so future GResource entries using
        // source-relative paths continue to resolve.
        .arg("--sourcedir")
        .arg(metainfo_parent)
        .arg("--sourcedir")
        .arg(&manifest_dir)
        .arg("--target")
        .arg(&gresource)
        .arg(&resource_manifest)
        .current_dir(&manifest_dir)
        .status();
    if !matches!(res_status, Ok(s) if s.success()) {
        panic!("glib-compile-resources failed; cannot embed About dialog metainfo");
    }
    // Installed message catalogs live under $prefix/share/locale; the
    // meson build passes it as GRAB_PREFIX, dev/test builds fall back to
    // the source po/ dir at runtime (no .mo there, so gettext is a no-op).
    let prefix = std::env::var("GRAB_PREFIX").unwrap_or_else(|_| "/usr".to_string());
    println!(
        "cargo:rustc-env=GRAB_LOCALEDIR={}/share/locale",
        prefix.trim_end_matches('/')
    );
}
