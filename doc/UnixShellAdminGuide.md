# Unix Shell Admin Guide — See admin.md

The stock TbaMUD `doc/` ships `UnixShellAdminGuide.pdf`, a general primer on
administering a MUD from a UNIX shell account (shell basics, running the C
server under autorun, managing processes and files).

That binary PDF is not carried over as an adapted document. Its content is
either generic UNIX shell knowledge (covered far better by your system's own
documentation and `man` pages) or specific to the stock C server's `autorun` /
`bin/` layout, which the Rust rewrite does not use.

For administering the rewrite, see:

- [admin.md](admin.md) — running, configuring, and maintaining a tbamud-rwb
  server (build, flags, the implementor, immortal commands, player data, bans,
  boards, rent cleanup, backups, shutdown).
- [`../README.md`](../README.md) — build & run, command-line flags.
- [debugging.md](debugging.md) — logging (`RUST_LOG`), backtraces, validating
  world data.

In brief: build with `cargo build --release`, run
`target/release/circle -p <port> -d <libdir>` under whatever process supervisor
you prefer (systemd, a shell `while` loop, tmux), back up the plain-text `lib/`
tree regularly, and read the log (stderr or the `-o` file).
