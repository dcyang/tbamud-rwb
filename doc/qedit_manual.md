# qedit Manual — Not Applicable to tbamud-rwb

The stock TbaMUD `doc/` ships `qedit_manual.pdf`, a manual for the in-game quest
OLC editor `qedit`.

The Rust rewrite has **no OLC** — there is no `qedit` (or `medit`/`oedit`/
`zedit`/`redit`/`sedit`/`trigedit`/`aedit`) command. That binary PDF is
therefore not carried over as an adapted document.

Quests in the rewrite are still supported at runtime: the loader reads the stock
quest files (`lib/world/qst/*.qst`) directly, and the player-facing `quest`
command works against them. To create or change a quest, edit the `.qst` file by
hand (use an existing quest in the same file as a template) and restart, exactly
as for the other world data — see [building.md](building.md).
