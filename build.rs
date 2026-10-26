fn main() {
    // Link system libraries by their stable sonames so only the runtime packages
    // (libarchive13, libsodium23) are needed, not the -dev packages with unversioned symlinks.
    // The soname is looked up in the library directory reported by pkg-config, so distributions
    // with another soname (Arch: libsodium.so.26) work too; without the .pc file the default is used.
    link_soname("libarchive", "archive", "libarchive.so.13");
    // SALAMANDER_SODIUM_STATIC=<directory with libsodium.a> links libsodium statically, for
    // distributions where it has another soname or is not in the base repositories (the RPM
    // for AlmaLinux: EPEL 10 ships libsodium.so.26).
    match std::env::var_os("SALAMANDER_SODIUM_STATIC") {
        Some(dir) => {
            println!("cargo:rustc-link-search=native={}", std::path::Path::new(&dir).display());
            println!("cargo:rustc-link-lib=static=sodium");
        }
        None => link_soname("libsodium", "sodium", "libsodium.so.23"),
    }
    println!("cargo:rerun-if-env-changed=SALAMANDER_SODIUM_STATIC");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-changed=build.rs");
}

/// Links `lib<name>.so.<major>` found in the pkg-config libdir of `package`, or `default`.
fn link_soname(package: &str, name: &str, default: &str) {
    let soname = pkg_config::get_variable(package, "libdir")
        .ok()
        .and_then(|libdir| {
            let found = find_soname(std::path::Path::new(&libdir), name)?;
            println!("cargo:rustc-link-search=native={libdir}");
            Some(found)
        })
        .unwrap_or_else(|| default.to_owned());
    println!("cargo:rustc-link-lib=dylib:+verbatim={soname}");
}

/// The file `lib<name>.so.<major>` in `dir` (the soname link, not `lib<name>.so.<major>.<minor>…`).
fn find_soname(dir: &std::path::Path, name: &str) -> Option<String> {
    let prefix = format!("lib{name}.so.");
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let file = entry.file_name().into_string().ok()?;
        let major = file.strip_prefix(&prefix)?;
        (!major.is_empty() && major.bytes().all(|b| b.is_ascii_digit())).then_some(file)
    })
}
