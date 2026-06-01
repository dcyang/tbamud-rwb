# tbamud-rwb and CMake

Stock TbaMUD offers an optional CMake build alongside autoconf.  The Rust
rewrite uses neither CMake nor autoconf — the build system is Cargo:

    cargo build --release

There is nothing to configure or generate.  See ../README.md.
