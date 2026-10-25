fn main() {
    // Link system libraries by their stable sonames so only the runtime packages
    // (libarchive13, libsodium23) are needed, not the -dev packages with unversioned symlinks.
    println!("cargo:rustc-link-lib=dylib:+verbatim=libarchive.so.13");
    println!("cargo:rustc-link-lib=dylib:+verbatim=libsodium.so.23");
    println!("cargo:rerun-if-changed=build.rs");
}
