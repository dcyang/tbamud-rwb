# qedit Manual — see building.md

The stock TbaMUD `doc/` ships `qedit_manual.pdf`, a manual for the in-game quest
OLC editor `qedit`.

The Rust rewrite **implements `qedit`** (along with the rest of the OLC editor
set). That binary PDF isn't carried over verbatim; the quest editor is
documented in [building.md](building.md) (the "Editing in-game with OLC"
section). In brief: `qedit <vnum>` opens a menu to edit a quest's name, the
description/info/completion/quit texts, type, quest-master, target, flags,
prev/next/prereq links, the seven values, and the gold/exp/object rewards; `Q`
commits to the live world and rewrites the zone's `.qst` file.

Quests in the rewrite are also supported at runtime regardless: the loader reads the stock
quest files (`lib/world/qst/*.qst`) directly, and the player-facing `quest`
command works against them. To create or change a quest, edit the `.qst` file by
hand (use an existing quest in the same file as a template) and restart, exactly
as for the other world data — see [building.md](building.md).
