fn main() {
    // Link libarchive by its stable soname so only the runtime package (libarchive13) is
    // needed, not libarchive-dev with its unversioned libarchive.so symlink.
    println!("cargo:rustc-link-lib=dylib:+verbatim=libarchive.so.13");
    println!("cargo:rerun-if-changed=build.rs");
}
