# tbamud-rwb Utility Programs

Adapted from the stock TbaMUD "Utility Programs" document (originally by Alex
Fletcher).

Stock TbaMUD ships a `src/util/` directory of standalone C programs, compiled
into `bin/`, for one-off conversions and maintenance. The Rust rewrite does
**not** build any of these separate utilities — it is a single `circle` binary.
This file lists each stock utility and its status / equivalent in the rewrite.

## Conversion utilities (one-time, mostly obsolete)

- **shopconv** — Converted pre-v3 shop files to the v3 format.
  *Status: N/A.* The rewrite reads the current TbaMUD shop format directly
  (`db.rs::parse_shop_file`); no conversion needed.
- **split** — Split a large area file into separate files.
  *Status: not provided.* Edit world files by hand ([building.md](building.md)).
- **wld2html** — Generated HTML maps from `.wld` files.
  *Status: not provided.* (In-game `map`/`scan` cover orientation.)
- **webster** — Dictionary lookup helper.
  *Status: not provided / obsolete.*

## Maintenance utilities

- **asciipasswd** — Set/reset a player's password by editing the ASCII player
  file. *Status: not needed as a separate tool.* Player files are plain ASCII
  (`players.rs`); an immortal can also use in-game commands. Passwords use the
  same DES `crypt(3)` as stock (`build.rs` links libcrypt), so stock and rewrite
  player files are compatible.
- **sign** — Posted a "sign" / slow-printed banner to a terminal.
  *Status: not provided.*

## Informational utilities

- **listrent** — Dumped the contents of a player's rent/crash object file.
  *Status: superseded.* Player objects are stored as plain-text `.objs` files
  under `lib/plrobjs/<bucket>/<name>.objs` (`players.rs`); read them directly, or
  use the immortal `stat <player>` command on an offline player. See
  [admin.md](admin.md) for the rent system and the boot-time stale-object
  cleanup (the `-q` flag).

## Internal utilities

- **autowiz** — Regenerated the wizlist/immlist text from the player file when an
  immortal's level changed. *Status: not automated.* The `wizlist` / `immlist`
  commands show `lib/text/wizlist` and `lib/text/immlist`; edit those text files
  (or regenerate them by hand) to update the lists.

## Summary

Everything the stock utilities did is either (a) no longer necessary because the
rewrite reads current-format, plain-text data directly, or (b) handled in-game by
an immortal command. If you need a batch operation over world or player data,
write a small throwaway script against the plain-text files rather than a
compiled utility.
