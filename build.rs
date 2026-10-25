fn main() {
    // Link system libraries by their stable sonames so only the runtime packages
    // (libarchive13, libsodium23) are needed, not the -dev packages with unversioned symlinks.
    println!("cargo:rustc-link-lib=dylib:+verbatim=libarchive.so.13");
    // SALAMANDER_SODIUM_STATIC=<directory with libsodium.a> links libsodium statically, for
    // distributions where it has another soname or is not in the base repositories (the RPM
    // for AlmaLinux: EPEL 10 ships libsodium.so.26).
    match std::env::var_os("SALAMANDER_SODIUM_STATIC") {
        Some(dir) => {
            println!("cargo:rustc-link-search=native={}", std::path::Path::new(&dir).display());
            println!("cargo:rustc-link-lib=static=sodium");
        }
        None => println!("cargo:rustc-link-lib=dylib:+verbatim=libsodium.so.23"),
    }
    println!("cargo:rerun-if-env-changed=SALAMANDER_SODIUM_STATIC");
    println!("cargo:rerun-if-changed=build.rs");
}
