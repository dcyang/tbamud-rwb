fn main() {
    // Link against libcrypt for DES crypt(3) password hashing, which is
    // compatible with existing tbaMUD player files (Pass: field uses crypt output).
    // On modern Debian/Ubuntu this is libxcrypt; on older glibc it is part of libc.
    // `cargo::` is the current build-script directive syntax (Rust >= 1.77);
    // the legacy single-colon `cargo:` form is deprecated.
    #[cfg(target_os = "linux")]
    println!("cargo::rustc-link-lib=crypt");
}
