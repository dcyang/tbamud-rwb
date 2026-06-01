tbamud-rwb build notes: Acorn RISC OS (obsolete)
==================================================

This file existed in stock TbaMUD to describe compiling the C server with Acorn RISC OS.
The Rust rewrite builds only with Cargo and targets modern Rust toolchains, so
Acorn RISC OS is not supported.  Build on a current operating system with:

    cargo build --release

See ../README.md and doc/porting.txt.
