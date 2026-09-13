fn main() {
    // The APFS adapter is Rust. Only link the public Apple frameworks needed by
    // its FFI declarations; no project C source or archive is compiled here.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        for framework in ["CoreServices", "CoreFoundation", "DiskArbitration"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }
}
