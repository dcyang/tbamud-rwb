/// Combat — turn-based round resolution driven by a background tick task.
///
/// Mirrors the violence loop in fight.c:
///   - heartbeat runs perform_violence() every PULSE_VIOLENCE pulses
///   - each combatant whose `fighting` is set attacks their opponent
///   - dam_message() / damage() resolve hits, deaths, and removal
///
/// Scope for Checkpoint 6: player-vs-mob only. PvP, weapons (THAC0/dam from
/// equipment), skills, spells, and corpse generation are deferred. Damage
/// is a simple `1..=level+1` roll per round.

use std::sync::Arc;

use rand::Rng;
use tokio::{sync::Mutex, time::{Duration, MissedTickBehavior}};

use crate::{
    character::{str_damage_bonus, dex_hit_bonus, Character, SharedChars, Target, WEAR_WIELD},
    players::Class,
    db::dice,
    world::World,
};

/// How often the combat tick fires.  Matches PULSE_VIOLENCE in CircleMUD
/// (2 seconds — long enough to read, short enough to feel responsive).
const TICK_SECONDS: u64 = 2;

/// Spawn the long-running combat task.  Returns immediately; the task lives
/// for the lifetime of the server.
pub fn spawn(world: Arc<Mutex<World>>, chars: SharedChars) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(TICK_SECONDS));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            tick_once(&world, &chars).await;
        }
    });
}

/// One combat round.  Resolves all player→mob attacks first, then all
/// mob→player counter-attacks.
async fn tick_once(world: &Arc<Mutex<World>>, chars: &SharedChars) {
    // ----- Phase 0: tick mob affects (Poison etc) -------------------------
    // Returns the list of (mob_id, room, mob_name, dead) for any mob that
    // either had an expired effect (broadcast "looks better") or was
    // killed outright by DoT damage.
    let mob_effect_outcomes: Vec<(u32, crate::world::RoomVnum, String, bool, Vec<crate::character::Skill>)> = {
        let mut w = world.lock().await;
        let mut out = Vec::new();
        // Snapshot ids so we can mutate w.mob_instances entries one by one.
        let mids: Vec<u32> = w.mob_instances.iter()
            .filter(|m| !m.affects.is_empty())
            .map(|m| m.id).collect();
        for mid in mids {
            let expired = if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mid) {
                m.tick_affects()
            } else { continue; };
            // Pacify charmed mobs: clear their fighting state every tick
            // so they refuse to swing.  Player attacks still wake them
            // (Sleep is stripped on damage); we deliberately keep this
            // *separate* from Sleep so being attacked doesn't break the
            // charm.
            if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mid) {
                if m.affects.iter().any(|a| a.skill == crate::character::Skill::CharmPerson) {
                    m.fighting = None;
                }
            }
            if let Some(m) = w.mob_instances.iter().find(|m| m.id == mid) {
                let room = m.in_room;
                let name = w.mob_protos.get(&m.vnum)
                    .map(|p| p.short_descr.clone())
                    .unwrap_or_else(|| "the creature".to_string());
                let dead = m.hp <= 0;
                out.push((mid, room, name, dead, expired));
            }
        }
        out
    };
    for (mid, room, name, dead, expired) in mob_effect_outcomes {
        let cl = chars.lock().await;
        if !expired.is_empty() {
            cl.broadcast_room(room, None, &format!("{name} looks better.\r\n"));
        }
        drop(cl);
        if dead {
            // DoT-only kill — no XP attribution (no clear killer).
            kill_mob(mid, room, &name, "the venom", world, chars).await;
        }
    }

    // ----- Phase 0.5: autoassist sweep -----------------------------------
    // For each follower with autoassist set, if their leader is fighting
    // a mob in the same room and the follower isn't fighting, engage.
    {
        let handles: Vec<crate::character::PlayerHandle> = {
            let cl = chars.lock().await;
            cl.iter().cloned().collect()
        };
        // Snapshot (follower_id, leader_id, want_assist, room) for those
        // who'd potentially join in.
        let mut candidates: Vec<(u32, u32, crate::world::RoomVnum)> = Vec::new();
        for ph in &handles {
            let c = ph.character.lock().await;
            if !c.autoassist || c.fighting.is_some() { continue; }
            if let Some(lid) = c.following {
                candidates.push((ph.id, lid, c.current_room));
            }
        }
        for (fid, lid, room) in candidates {
            let leader_target = if let Some(lh) = handles.iter().find(|p| p.id == lid) {
                let c = lh.character.lock().await;
                if c.current_room == room { c.fighting } else { None }
            } else { None };
            let Some(tgt) = leader_target else { continue; };
            if tgt.is_player { continue; } // don't autoassist into PvP
            // Engage: set follower.fighting = tgt; back-fill mob.fighting if free.
            if let Some(fh) = handles.iter().find(|p| p.id == fid) {
                fh.character.lock().await.fighting = Some(tgt);
                let _ = fh.send.send("\r\nYou rush to your leader's aid!\r\n".to_string());
                let mut w = world.lock().await;
                if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == tgt.id) {
                    if m.fighting.is_none() {
                        m.fighting = Some(crate::character::Target {
                            id: fid, is_player: true,
                        });
                    }
                }
            }
        }
    }

    // ----- Phase 1: snapshot all fighters --------------------------------
    // Avoid replacing two locks; collect intents then mutate.
    let player_intents: Vec<PlayerIntent> = {
        let cl = chars.lock().await;
        let mut v = Vec::new();
        for p in cl.iter() {
            let me = p.character.lock().await;
            if let Some(tgt) = me.fighting {
                v.push(PlayerIntent {
                    attacker_id:   p.id,
                    attacker_name: me.name.clone(),
                    level:         me.level,
                    class:         me.class,
                    room:          me.current_room,
                    target:        tgt,
                    weapon_iid:    me.equipment[WEAR_WIELD],
                    str_score:     me.str_,
                    dex_score:     me.dex,
                    hit_bonus:     me.affect_hit_bonus() + me.bonus_hitroll,
                    dam_bonus:     me.affect_dam_bonus() + me.bonus_damroll,
                    has_haste:     me.affects.iter().any(|a| a.skill == crate::character::Skill::Haste),
                });
            }
        }
        v
    };

    // ----- Phase 2: resolve player attacks (multi-attack by level/class) --
    for intent in player_intents {
        let n = num_attacks(intent.level, intent.class, intent.has_haste);
        for _ in 0..n {
            // Stop early if the attacker stopped fighting between swings
            // (target died on a prior iteration → fighting cleared by
            // resolve_player_attack).
            let still_fighting = {
                let ph = {
                    let cl = chars.lock().await;
                    let h = cl.iter().find(|p| p.id == intent.attacker_id).cloned();
                    h
                };
                match ph {
                    Some(ph) => ph.character.lock().await.fighting.is_some(),
                    None     => false,
                }
            };
            if !still_fighting { break; }
            resolve_player_attack(intent.clone(), world, chars).await;
        }
    }

    // ----- Phase 3: snapshot mob attackers -------------------------------
    // Mobs with an active Sleep affect skip their swing this round; their
    // intent never makes it into the snapshot. Slow gives a 50% chance to
    // skip per tick.  Blindness is honored later inside resolve_mob_attack
    // via the affect lookup.
    let mob_intents: Vec<MobIntent> = {
        use rand::Rng;
        let w = world.lock().await;
        w.mob_instances.iter()
            .filter(|m| !m.affects.iter().any(|a| a.skill == crate::character::Skill::Sleep))
            .filter(|m| {
                if m.affects.iter().any(|a| a.skill == crate::character::Skill::Slow) {
                    rand::thread_rng().gen_range(0..100) >= 50
                } else { true }
            })
            .filter_map(|m| {
                m.fighting.map(|tgt| MobIntent {
                    attacker_id:   m.id,
                    attacker_vnum: m.vnum,
                    room:          m.in_room,
                    target:        tgt,
                    level:         w.mob_protos.get(&m.vnum).map(|p| p.level).unwrap_or(1),
                })
            })
            .collect()
    };

    // ----- Phase 4: resolve mob counter-attacks --------------------------
    for intent in mob_intents {
        resolve_mob_attack(intent, world, chars).await;
    }

    // ----- Phase 4.5: FIGHT (`k`) triggers fire each round on combat-ants
    fight_trigger_tick(world, chars).await;

    // ----- Phase 5: HP/mana regen for non-fighting players ---------------
    // Regen is gentle (small per-tick) and only applies when out of combat,
    // so that combat losses feel meaningful.
    regen_tick(chars).await;

    // ----- Phase 6: aggressive mobs engage players in their room --------
    aggro_tick(world, chars).await;
}

/// Each tick, walk all aggressive (MOB_AGGRESSIVE) mobs that aren't
/// already fighting and look for a player in the same room.  If found,
/// engage combat.  Mobs with see_invisible/level concerns are not yet
/// modeled — for now hidden players are skipped (gives `hide` its main
/// utility against mundane aggro).
async fn aggro_tick(world: &Arc<Mutex<World>>, chars: &SharedChars) {
    // Snapshot all online player rooms (and hidden state) for cheap lookup.
    let live_players: Vec<(u32, crate::world::RoomVnum, bool)> = {
        let cl = chars.lock().await;
        let mut v = Vec::new();
        for p in cl.iter() {
            let c = p.character.lock().await;
            v.push((p.id, c.current_room, c.hidden));
        }
        v
    };
    if live_players.is_empty() { return; }

    // Identify all aggressive (or memory-grudged) mob instances that
    // have no current target and have an appropriate player in-room.
    let intents: Vec<(u32, u32, crate::world::RoomVnum, String)> = {
        let w = world.lock().await;
        let mut v = Vec::new();
        for m in &w.mob_instances {
            if m.fighting.is_some() { continue; }
            let proto = match w.mob_protos.get(&m.vnum) {
                Some(p) => p,
                None    => continue,
            };
            let is_aggro  = proto.mob_flags[0] & crate::world::MOB_AGGRESSIVE != 0;
            let has_memory = proto.mob_flags[0] & crate::world::MOB_MEMORY     != 0;

            // First check memory: any remembered player in this room is a
            // priority target, even if not normally aggressive.
            let mem_target = if has_memory && !m.remembers.is_empty() {
                live_players.iter().find(|(pid, room, _)| {
                    *room == m.in_room && m.remembers.contains(pid)
                }).map(|&(pid, _, _)| pid)
            } else { None };

            if let Some(pid) = mem_target {
                v.push((m.id, pid, m.in_room, proto.short_descr.clone()));
                continue;
            }
            if !is_aggro { continue; }
            // Non-hidden player in this mob's room.
            let target = live_players.iter()
                .find(|(_, room, hidden)| *room == m.in_room && !hidden);
            if let Some(&(pid, _, _)) = target {
                v.push((m.id, pid, m.in_room, proto.short_descr.clone()));
            }
        }
        v
    };

    // Engage each (mob -> player) pairing.
    for (mob_id, player_id, room, mob_name) in intents {
        let handle = {
            let cl = chars.lock().await;
            let h = cl.iter().find(|p| p.id == player_id).cloned();
            h
        };
        let Some(ph) = handle else { continue; };
        {
            let mut c = ph.character.lock().await;
            if c.fighting.is_some() { continue; }
            c.fighting = Some(crate::character::Target {
                id: mob_id, is_player: false,
            });
        }
        {
            let mut w = world.lock().await;
            if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
                m.fighting = Some(crate::character::Target {
                    id: player_id, is_player: true,
                });
            }
        }
        let _ = ph.send.send(format!(
            "\r\n{mob_name} attacks you!\r\n",
        ));
        let cl = chars.lock().await;
        cl.broadcast_room(
            room, Some(player_id),
            &format!("{mob_name} attacks {}!\r\n", ph.name),
        );
    }
}

/// Regen HP and mana for players not currently in combat, and decrement
/// active affect durations.  Mirrors a slice of point_update() in limits.c.
async fn regen_tick(chars: &SharedChars) {
    let handles: Vec<crate::character::PlayerHandle> = {
        let cl = chars.lock().await;
        cl.iter().cloned().collect()
    };
    for ph in handles {
        // Tick affects + collect expiry messages.
        let (expired, in_combat) = {
            let mut c = ph.character.lock().await;
            (c.tick_affects(), c.fighting.is_some())
        };
        for skill in expired {
            let _ = ph.send.send(format!(
                "\r\nThe glow of {} fades from you.\r\n", skill.name(),
            ));
        }
        // Regen — only when out of combat.
        if in_combat { continue; }
        let mut c = ph.character.lock().await;
        let mult = c.position.regen_factor();
        if mult == 0 { continue; }
        if c.hp < c.max_hp {
            c.hp = (c.hp + (1 + c.level / 5) * mult).min(c.max_hp);
        }
        if c.mana < c.max_mana {
            c.mana = (c.mana + (1 + c.level / 4) * mult).min(c.max_mana);
        }
        if c.movement < c.max_movement {
            c.movement = (c.movement + (1 + c.level / 3) * mult).min(c.max_movement);
        }
    }
}

#[derive(Clone)]
struct PlayerIntent {
    attacker_id:   u32,
    attacker_name: String,
    level:         i32,
    class:         Class,
    room:          crate::world::RoomVnum,
    target:        Target,
    weapon_iid:    Option<u32>,
    str_score:     i32,
    dex_score:     i32,
    hit_bonus:     i32,
    dam_bonus:     i32,
    has_haste:     bool,
}

/// Defender-side passive-skill learn bump: roll `chance_pct`%, on hit
/// add 1 to the named skill (capped at 100) and notify the defender.
/// Used by Dodge/Parry success branches.
async fn bump_defensive_skill(
    ph: &crate::character::PlayerHandle,
    skill: crate::character::Skill,
    chance_pct: i32,
) {
    use rand::Rng;
    if rand::thread_rng().gen_range(0..100) >= chance_pct { return; }
    let bumped = {
        let mut c = ph.character.lock().await;
        let cur = *c.skills.get(&skill).unwrap_or(&0);
        if cur >= 100 { return; }
        let next = (cur + 1).min(100);
        c.skills.insert(skill, next);
        true
    };
    if bumped {
        let _ = ph.send.send(format!(
            "\r\nYou feel more skilled at {}.\r\n", skill.name(),
        ));
    }
}

/// Number of attacks per combat round.  Warriors gain extras at lvl 8 /
/// 16 / 24; thieves at lvl 16.  Other classes are stuck at 1.  Haste
/// (any class) grants +1 attack per round on top.
fn num_attacks(level: i32, class: Class, has_haste: bool) -> i32 {
    let base = match class {
        Class::Warrior => {
            if      level >= 24 { 4 }
            else if level >= 16 { 3 }
            else if level >= 8  { 2 }
            else                { 1 }
        }
        Class::Thief => if level >= 16 { 2 } else { 1 },
        _ => 1,
    };
    base + if has_haste { 1 } else { 0 }
}

struct MobIntent {
    attacker_id:   u32,
    attacker_vnum: i32,
    room:          crate::world::RoomVnum,
    target:        Target,
    level:         i32,
}

/// Player-vs-player swing.  Mirrors resolve_player_attack's structure
/// but the target is another Character behind a mutex.  Uses
/// `interpreter::total_ac` for the defender's AC and `player_death`
/// for kill resolution.
async fn resolve_pvp_attack(
    p: PlayerIntent,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let target_ph = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|h| h.id == p.target.id).cloned();
        h
    };
    let Some(ph) = target_ph else {
        clear_player_fighting(p.attacker_id, chars).await;
        return;
    };
    // Damage roll first.
    let base_dmg = {
        let w = world.lock().await;
        let weapon = p.weapon_iid.and_then(|iid|
            w.obj_instances.iter().find(|o| o.id == iid)
                .and_then(|o| w.obj_protos.get(&o.vnum)));
        let base = if let Some(wp) = weapon {
            dice(wp.value[1], wp.value[2])
        } else {
            dice(1, 4)
        };
        (base.max(1) + p.level / 4 + str_damage_bonus(p.str_score) + p.dam_bonus).max(1)
    };
    // Snapshot defender AC + check room/peace.
    let (target_ac, target_room, target_name) = {
        let c = ph.character.lock().await;
        let ac = crate::interpreter::total_ac(&c, world).await;
        (ac, c.current_room, c.name.clone())
    };
    if target_room != p.room {
        clear_player_fighting(p.attacker_id, chars).await;
        return;
    }
    let hit_chance = (60 + p.level + dex_hit_bonus(p.dex_score) + p.hit_bonus - target_ac / 10)
        .clamp(5, 95);
    let landed = rand::thread_rng().gen_range(0..100) < hit_chance;
    if !landed {
        let _ = ph.send.send(format!("\r\n{} swings at you and misses.\r\n", p.attacker_name));
        let cl = chars.lock().await;
        if let Some(att) = cl.iter().find(|h| h.id == p.attacker_id) {
            let _ = att.send.send(format!("\r\nYou swing at {target_name} and miss.\r\n"));
        }
        cl.broadcast_room(p.room, Some(p.attacker_id),
            &format!("{} swings at {target_name} and misses.\r\n", p.attacker_name));
        return;
    }
    // Apply damage; check for death.
    let dead = {
        let mut c = ph.character.lock().await;
        c.hp -= base_dmg;
        c.hp <= 0
    };
    let _ = ph.send.send(format!(
        "\r\n{} hits you for {base_dmg} damage!\r\n", p.attacker_name,
    ));
    {
        let cl = chars.lock().await;
        if let Some(att) = cl.iter().find(|h| h.id == p.attacker_id) {
            let _ = att.send.send(format!(
                "\r\nYou hit {target_name} for {base_dmg} damage.\r\n",
            ));
        }
        cl.broadcast_room(p.room, Some(p.attacker_id),
            &format!("{} hits {target_name}.\r\n", p.attacker_name));
    }
    if dead {
        player_death(&ph, world, chars).await;
        clear_player_fighting(p.attacker_id, chars).await;
    }
}

async fn resolve_player_attack(
    p: PlayerIntent,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    if p.target.is_player {
        resolve_pvp_attack(p, world, chars).await;
        return;
    }

    // To-hit roll first.  Hit% = base + level + dex_hit + hit_bonus -
    // mob.ac/10, clamped 5..=95. Mob AC is looked up alongside the
    // damage roll (same world lock).
    let (mob_ac, dmg): (i32, i32) = {
        let w = world.lock().await;
        let mob_ac = w.mob_instances.iter().find(|m| m.id == p.target.id)
            .and_then(|m| w.mob_protos.get(&m.vnum))
            .map(|pr| pr.ac).unwrap_or(0);
        let weapon = p.weapon_iid.and_then(|iid|
            w.obj_instances.iter().find(|o| o.id == iid)
                .and_then(|o| w.obj_protos.get(&o.vnum)));
        let base = if let Some(wp) = weapon {
            dice(wp.value[1], wp.value[2])
        } else {
            dice(1, 4)
        };
        let dmg = (base.max(1) + p.level / 4 + str_damage_bonus(p.str_score) + p.dam_bonus).max(1);
        (mob_ac, dmg)
    };
    let hit_chance = (60 + p.level + dex_hit_bonus(p.dex_score) + p.hit_bonus - mob_ac / 10)
        .clamp(5, 95);
    let landed = rand::thread_rng().gen_range(0..100) < hit_chance;
    if !landed {
        // Miss: broadcast and bail out for this swing.
        let target_name = {
            let w = world.lock().await;
            w.mob_instances.iter().find(|m| m.id == p.target.id)
                .and_then(|m| w.mob_protos.get(&m.vnum))
                .map(|pr| pr.short_descr.clone())
                .unwrap_or_else(|| "the creature".to_string())
        };
        let cl = chars.lock().await;
        if let Some(ph) = cl.iter().find(|h| h.id == p.attacker_id) {
            let _ = ph.send.send(format!("\r\nYou swing at {target_name} and miss.\r\n"));
        }
        cl.broadcast_room(p.room, Some(p.attacker_id),
            &format!("\r\n{} swings at {target_name} and misses.\r\n", p.attacker_name));
        return;
    }

    let (target_name, target_dead, target_room, has_memory, is_wimpy, low_hp) = {
        let mut w = world.lock().await;

        // Read-only first: existence, room, proto name + flags.
        let (vnum, in_room) = match w.mob_instances.iter().find(|m| m.id == p.target.id) {
            Some(m) => (m.vnum, m.in_room),
            None => {
                drop(w);
                clear_player_fighting(p.attacker_id, chars).await;
                return;
            }
        };
        if in_room != p.room {
            drop(w);
            clear_player_fighting(p.attacker_id, chars).await;
            return;
        }
        let (proto_name, has_memory, is_wimpy) = match w.mob_protos.get(&vnum) {
            Some(pr) => (
                pr.short_descr.clone(),
                pr.mob_flags[0] & crate::world::MOB_MEMORY != 0,
                pr.mob_flags[0] & crate::world::MOB_WIMPY  != 0,
            ),
            None => ("the creature".into(), false, false),
        };

        // Mutation: damage + remember attacker.  Sleep is broken by any
        // damage, so the affect is stripped here.
        let m = w.mob_instances.iter_mut().find(|m| m.id == p.target.id).unwrap();
        m.hp -= dmg;
        m.affects.retain(|a| a.skill != crate::character::Skill::Sleep);
        let dead = m.hp <= 0;
        if !dead && m.fighting.is_none() {
            m.fighting = Some(Target { id: p.attacker_id, is_player: true });
        }
        if has_memory && !m.remembers.contains(&p.attacker_id) {
            m.remembers.push(p.attacker_id);
        }
        let low_hp = !dead && m.hp <= m.max_hp / 6;
        (proto_name, dead, in_room, has_memory, is_wimpy, low_hp)
    };
    let _ = has_memory;

    // Build per-recipient messages.
    let to_attacker = format!(
        "\r\nYou hit {target_name} for {dmg} damage.\r\n",
    );
    let to_room = format!(
        "\r\n{} hits {target_name}.\r\n", p.attacker_name,
    );
    {
        let cl = chars.lock().await;
        if let Some(ph) = cl.iter().find(|h| h.id == p.attacker_id) {
            let _ = ph.send.send(to_attacker);
        }
        cl.broadcast_room(p.room, Some(p.attacker_id), &to_room);
    }

    // MOB_HELPER assist: any non-fighting mob in the room with the
    // helper flag joins in on the side of the original victim — i.e.
    // it engages the attacker (the player).  Skip the original target
    // and any mob already fighting; broadcast each join.
    if !target_dead {
        let helpers: Vec<(u32, String)> = {
            let mut w = world.lock().await;
            let mut joined = Vec::new();
            let mids: Vec<u32> = w.rooms.get(&p.room).map(|r| r.mobs.clone()).unwrap_or_default();
            for mid in mids {
                if mid == p.target.id { continue; }
                let Some(m) = w.mob_instances.iter().find(|m| m.id == mid) else { continue; };
                if m.fighting.is_some() { continue; }
                let Some(pr) = w.mob_protos.get(&m.vnum) else { continue; };
                if pr.mob_flags[0] & crate::world::MOB_HELPER == 0 { continue; }
                let short = pr.short_descr.clone();
                let mut_m = w.mob_instances.iter_mut().find(|m| m.id == mid).unwrap();
                mut_m.fighting = Some(Target { id: p.attacker_id, is_player: true });
                joined.push((mid, short));
            }
            joined
        };
        if !helpers.is_empty() {
            let cl = chars.lock().await;
            for (_id, name) in &helpers {
                cl.broadcast_room(p.room, None,
                    &format!("{name} leaps to the defense!\r\n"));
            }
        }
    }

    if target_dead {
        let (xp, gold, killed_vnum) = {
            let w = world.lock().await;
            let m = w.mob_instances.iter().find(|m| m.id == p.target.id);
            let vnum = m.map(|m| m.vnum).unwrap_or(-1);
            let proto = m.and_then(|m| w.mob_protos.get(&m.vnum));
            let xp = proto.map(|mp| mp.exp as i64).unwrap_or(0);
            let gold = proto.map(|mp| mp.gold as i64).unwrap_or(0);
            (xp, gold, vnum)
        };
        kill_mob(p.target.id, target_room, &target_name, &p.attacker_name, world, chars).await;
        // Clear the player's fighting state since the mob is gone.
        clear_player_fighting(p.attacker_id, chars).await;
        // Award XP, split among grouped members in the kill room.
        award_xp_split(p.attacker_id, target_room, xp, chars).await;
        // Gold drop, same split rule as XP.
        award_gold_split(p.attacker_id, target_room, gold, chars).await;
        // Quest progress.
        notify_quest_kill(p.attacker_id, killed_vnum, world, chars).await;
        // Autoloot: if the attacker has autoloot set, drain the freshly
        // spawned corpse into their inventory.
        autoloot_after_kill(p.attacker_id, &target_name, target_room, world, chars).await;
        return;
    }

    // Wimpy mob check: low HP + MOB_WIMPY → mob flees.
    if is_wimpy && low_hp {
        mob_flee(p.target.id, &target_name, world, chars).await;
        clear_player_fighting(p.attacker_id, chars).await;
    }
}

/// A mob attempts to flee in a random valid direction.  Clears its
/// fighting state, broadcasts "X flees!" to both rooms, and physically
/// moves the mob between room.mobs lists.  No-op if no valid exit.
async fn mob_flee(
    mob_id: u32,
    mob_name: &str,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    use rand::seq::SliceRandom;
    let (from_room, target_room) = {
        let mut w = world.lock().await;
        let Some(m) = w.mob_instances.iter().find(|m| m.id == mob_id) else { return; };
        let from = m.in_room;
        // Pick a random exit, skipping NOMOB destinations.
        let candidates: Vec<crate::world::RoomVnum> = w.rooms.get(&from)
            .map(|r| r.exits.iter()
                .filter_map(|e| e.as_ref())
                .filter(|e| e.to_room != crate::world::NOWHERE
                          && w.rooms.contains_key(&e.to_room))
                .filter(|e| {
                    w.rooms.get(&e.to_room)
                        .map(|t| t.room_flags[0] & crate::world::ROOM_NOMOB == 0)
                        .unwrap_or(false)
                })
                .map(|e| e.to_room)
                .collect())
            .unwrap_or_default();
        let Some(&to) = candidates.choose(&mut rand::thread_rng()) else {
            return;
        };
        // Move the mob.
        if let Some(r) = w.rooms.get_mut(&from) {
            r.mobs.retain(|&id| id != mob_id);
        }
        if let Some(r) = w.rooms.get_mut(&to) {
            r.mobs.push(mob_id);
        }
        if let Some(m) = w.mob_instances.iter_mut().find(|m| m.id == mob_id) {
            m.in_room  = to;
            m.fighting = None;
        }
        (from, to)
    };
    {
        let cl = chars.lock().await;
        cl.broadcast_room(from_room, None, &format!("{mob_name} flees, severely wounded!\r\n"));
        cl.broadcast_room(target_room, None, &format!("{mob_name} arrives, fleeing in panic.\r\n"));
    }
    // Fire ENTRY triggers on the fleeing mob.
    crate::interpreter::fire_mob_entry_triggers(mob_id, world, chars).await;
}

/// Push a "Quest objective complete" message to a player if their active
/// quest matches the kill.  Mirrors interpreter::quest_check_kill but takes
/// the player by id (we don't have direct &mut Character in combat).
async fn notify_quest_kill(
    player_id: u32,
    killed_vnum: i32,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let ph = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.id == player_id).cloned();
        h
    };
    let Some(ph) = ph else { return };
    let mut c = ph.character.lock().await;
    if let Some(qmsg) = crate::interpreter::quest_check_kill(&mut c, killed_vnum, world).await {
        let _ = ph.send.send(qmsg);
    }
    if let Some(qmsg) = crate::interpreter::quest_check_save(&mut c, world).await {
        let _ = ph.send.send(qmsg);
    }
}

/// Split `xp` among the killer and any grouped allies in `killer_room`.
/// The killer always gets at least their share — a solo kill (no group,
/// or no grouped ally co-located) hands them the full amount.  Sharers
/// must be `grouped` AND in the same group as the killer (sharing a
/// leader id).
async fn award_xp_split(
    killer_id: u32,
    killer_room: crate::world::RoomVnum,
    xp: i64,
    chars: &SharedChars,
) {
    if xp <= 0 { return; }
    let recipients: Vec<u32> = {
        let cl = chars.lock().await;
        let killer = match cl.iter().find(|p| p.id == killer_id) {
            Some(k) => k.clone(),
            None    => { award_xp(killer_id, xp, chars).await; return; }
        };
        let (killer_grouped, killer_leader) = {
            let c = killer.character.lock().await;
            (c.grouped, c.following.unwrap_or(killer_id))
        };
        if !killer_grouped {
            vec![killer_id]
        } else {
            let mut ids = vec![killer_id];
            let handles: Vec<_> = cl.iter().cloned().collect();
            drop(cl);
            for ph in handles {
                if ph.id == killer_id { continue; }
                if ph.current_room != killer_room { continue; }
                let c = ph.character.lock().await;
                let in_group = c.grouped && (
                    c.id == killer_leader
                    || c.following == Some(killer_leader)
                );
                if in_group { ids.push(ph.id); }
            }
            ids
        }
    };
    let n = recipients.len() as i64;
    let share = (xp / n).max(1);
    for id in recipients {
        award_xp(id, share, chars).await;
    }
}

/// Split a mob's `gold` reward across the killer's grouped same-room
/// allies (using the same membership rule as XP).  Floors to min 1 per
/// recipient.  Each receives the line "You receive N gold from the
/// corpse." through their mpsc.
async fn award_gold_split(
    killer_id: u32,
    killer_room: crate::world::RoomVnum,
    gold: i64,
    chars: &SharedChars,
) {
    if gold <= 0 { return; }
    let recipients: Vec<u32> = {
        let cl = chars.lock().await;
        let killer = match cl.iter().find(|p| p.id == killer_id) {
            Some(k) => k.clone(),
            None    => return,
        };
        let (killer_grouped, killer_leader) = {
            let c = killer.character.lock().await;
            (c.grouped, c.following.unwrap_or(killer_id))
        };
        if !killer_grouped {
            vec![killer_id]
        } else {
            let mut ids = vec![killer_id];
            let handles: Vec<_> = cl.iter().cloned().collect();
            drop(cl);
            for ph in handles {
                if ph.id == killer_id { continue; }
                if ph.current_room != killer_room { continue; }
                let c = ph.character.lock().await;
                let in_group = c.grouped && (
                    c.id == killer_leader
                    || c.following == Some(killer_leader)
                );
                if in_group { ids.push(ph.id); }
            }
            ids
        }
    };
    let n = recipients.len() as i64;
    let share = (gold / n).max(1);
    for id in recipients {
        let handle = {
            let cl = chars.lock().await;
            let h = cl.iter().find(|p| p.id == id).cloned();
            h
        };
        let Some(h) = handle else { continue };
        h.character.lock().await.gold += share;
        let _ = h.send.send(format!(
            "\r\nYou receive {share} gold from the corpse.\r\n"
        ));
    }
}

/// Award `xp` experience points to the player handle with `id`. Sends the
/// gain message + any level-up notification through the player's mpsc.
async fn award_xp(player_id: u32, xp: i64, chars: &SharedChars) {
    if xp <= 0 { return; }
    let handle = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.id == player_id).cloned();
        h
    };
    let Some(ph) = handle else { return; };

    let (msg, levels) = {
        let mut c = ph.character.lock().await;
        c.exp += xp;
        let levels = c.check_level_up();
        let msg = format!("\r\nYou gain {xp} experience.\r\n");
        (msg, levels)
    };
    let _ = ph.send.send(msg);
    if levels > 0 {
        // Snapshot the new level/maxhp for the message.
        let (level, max_hp) = {
            let c = ph.character.lock().await;
            (c.level, c.max_hp)
        };
        let _ = ph.send.send(format!(
            "\r\n*** You feel more powerful!  You are now level {level}.  Max HP: {max_hp} ***\r\n"
        ));
    }
}

async fn resolve_mob_attack(
    m: MobIntent,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    if !m.target.is_player {
        return; // mob vs mob not supported
    }

    let target_handle = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.id == m.target.id).cloned();
        h
    };
    let Some(ph) = target_handle else {
        // Target left the world; clear the mob's fighting state.
        let mut w = world.lock().await;
        if let Some(mob) = w.mob_instances.iter_mut().find(|x| x.id == m.attacker_id) {
            mob.fighting = None;
        }
        return;
    };

    // Blindness: 50% miss chance.  Skip the swing on the missed roll.
    let blinded = {
        let w = world.lock().await;
        w.mob_instances.iter().find(|x| x.id == m.attacker_id)
            .map(|x| x.affects.iter().any(|a| a.skill == crate::character::Skill::Blindness))
            .unwrap_or(false)
    };
    if blinded && rand::thread_rng().gen_range(0..100) < 50 {
        let short = {
            let w = world.lock().await;
            w.mob_protos.get(&m.attacker_vnum)
                .map(|p| p.short_descr.clone())
                .unwrap_or_else(|| "Something".into())
        };
        let _ = ph.send.send(format!("\r\n{short} swings at you blindly and misses!\r\n"));
        let cl = chars.lock().await;
        cl.broadcast_room(m.room, Some(m.target.id),
            &format!("\r\n{short} swings blindly at {} and misses.\r\n", ph.name));
        return;
    }

    // Defender dodge/parry rolls — pure miss chances rolled before the
    // damage calc.  Chance = learned/2, capped at 40 so neither skill
    // becomes auto-dodge at 100%.  Parry also requires a wielded weapon.
    let (dodge_pct, parry_pct, has_weapon, short, defender_name) = {
        let c = ph.character.lock().await;
        let dodge = *c.skills.get(&crate::character::Skill::Dodge).unwrap_or(&0) as i32;
        let parry = *c.skills.get(&crate::character::Skill::Parry).unwrap_or(&0) as i32;
        let has_weapon = c.equipment[WEAR_WIELD].is_some();
        let w = world.lock().await;
        let short = w.mob_protos.get(&m.attacker_vnum)
            .map(|p| p.short_descr.clone()).unwrap_or_else(|| "Something".into());
        ((dodge / 2).min(40), (parry / 2).min(40), has_weapon, short, c.name.clone())
    };
    if dodge_pct > 0 && rand::thread_rng().gen_range(0..100) < dodge_pct {
        bump_defensive_skill(&ph, crate::character::Skill::Dodge, 5).await;
        let _ = ph.send.send(format!("\r\nYou dodge {short}'s swing!\r\n"));
        let cl = chars.lock().await;
        cl.broadcast_room(m.room, Some(m.target.id),
            &format!("\r\n{defender_name} dodges {short}'s swing.\r\n"));
        return;
    }
    if has_weapon && parry_pct > 0 && rand::thread_rng().gen_range(0..100) < parry_pct {
        bump_defensive_skill(&ph, crate::character::Skill::Parry, 5).await;
        let _ = ph.send.send(format!("\r\nYou parry {short}'s attack!\r\n"));
        let cl = chars.lock().await;
        cl.broadcast_room(m.room, Some(m.target.id),
            &format!("\r\n{defender_name} parries {short}'s attack.\r\n"));
        return;
    }

    // Decide whether this mob is a spellcaster and rolls a cast this round.
    let cast_attempt = {
        let w = world.lock().await;
        let casts = w.mob_protos.get(&m.attacker_vnum)
            .map(|p| should_cast(p))
            .unwrap_or(false);
        casts
    } && rand::thread_rng().gen_range(0..100) < 30;

    // Roll mob damage. Spell attacks (when cast_attempt) use a separate
    // dice pool that ignores mundane AC but is still reduced by
    // affect-based dmg_reduction (sanctuary). Mundane attacks go through AC.
    let (dmg, is_spell): (i32, bool) = {
        let raw_mundane = {
            let w = world.lock().await;
            match w.mob_protos.get(&m.attacker_vnum) {
                Some(p) => (dice(p.dam_dice, p.dam_size) + p.damroll).max(1),
                None    => 1,
            }
        };
        let (ac, reduction) = {
            let me = ph.character.lock().await;
            let ac = crate::interpreter::total_ac(&me, world).await;
            let r  = me.affect_dmg_reduction();
            (ac, r)
        };
        if cast_attempt {
            let raw = dice(2, 4) + m.level / 2;
            let after = (raw * (100 - reduction)) / 100;
            (after.max(1), true)
        } else {
            let after_ac = (raw_mundane - ac / 3).max(1);
            let after = (after_ac * (100 - reduction)) / 100;
            (after.max(1), false)
        }
    };

    // Apply damage to the player; check death.
    let (player_dead, player_room, mob_short_name, trigger_wimpy) = {
        let mut c = ph.character.lock().await;
        if c.current_room != m.room {
            // Player moved/fled; mob loses its target.
            let mut w = world.lock().await;
            if let Some(mob) = w.mob_instances.iter_mut().find(|x| x.id == m.attacker_id) {
                mob.fighting = None;
            }
            return;
        }
        c.hp -= dmg;
        let w = world.lock().await;
        let short = w.mob_protos.get(&m.attacker_vnum)
            .map(|p| p.short_descr.clone())
            .unwrap_or_else(|| "Something".into());
        // Wimpy auto-flee: HP > 0 but below threshold.
        let wimpy_trigger = c.hp > 0 && c.wimpy > 0 && c.hp <= c.wimpy;
        (c.hp <= 0, c.current_room, short, wimpy_trigger)
    };

    let (to_victim, to_room) = if is_spell {
        (
            format!("\r\n{mob_short_name} conjures a glowing dart of force that strikes you for {dmg} damage!\r\n"),
            format!("\r\n{mob_short_name} hurls a glowing dart of force at {}.\r\n", ph.name),
        )
    } else {
        (
            format!("\r\n{mob_short_name} hits you for {dmg} damage.\r\n"),
            format!("\r\n{mob_short_name} hits {}.\r\n", ph.name),
        )
    };
    let _ = ph.send.send(to_victim);
    {
        let cl = chars.lock().await;
        cl.broadcast_room(m.room, Some(m.target.id), &to_room);
    }

    // Snake spec_proc: 15% chance on a melee hit to apply Poison.
    if !is_spell && !player_dead {
        let attacker_spec = {
            let w = world.lock().await;
            w.mob_instances.iter().find(|x| x.id == m.attacker_id)
                .and_then(|x| x.spec)
        };
        if attacker_spec == Some(crate::world::MobSpec::Snake) {
            use rand::Rng;
            if rand::thread_rng().gen_range(0..100) < 15 {
                let mut c = ph.character.lock().await;
                let already = c.affects.iter().any(|a|
                    a.skill == crate::character::Skill::Poison);
                if !already {
                    c.apply_affect(crate::character::Affect {
                        skill:          crate::character::Skill::Poison,
                        duration:       5,
                        to_hit:         0,
                        to_dam:         0,
                        dmg_reduction:  0,
                        dot_damage:     3,
                        to_ac:          0,
                    });
                    let _ = ph.send.send(
                        "\r\nVenom courses through your veins!\r\n".to_string()
                    );
                }
            }
        }
    }

    if player_dead {
        player_death(&ph, world, chars).await;
    } else if trigger_wimpy {
        if let Some(tx) = crate::interpreter::FORCE_CMD_TX.get() {
            let _ = ph.send.send(format!(
                "\r\nWimp mode: you flee head over heels.\r\n"
            ));
            let _ = tx.send(crate::interpreter::ForceCmdMsg {
                player:  ph.name.clone(),
                command: "flee".to_string(),
                world:   Arc::clone(world),
                chars:   Arc::clone(chars),
            });
        }
    }
    let _ = player_room;
}

async fn clear_player_fighting(player_id: u32, chars: &SharedChars) {
    let handle = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.id == player_id).cloned();
        h
    };
    if let Some(h) = handle {
        let mut c = h.character.lock().await;
        c.fighting = None;
    }
}

/// Autoloot: if the killer has `autoloot` set, drain the corpse just
/// created in `room` (matching `mob_short`) into their inventory.
/// The emptied corpse is left in the room to decay normally.
async fn autoloot_after_kill(
    killer_id: u32,
    mob_short: &str,
    room: crate::world::RoomVnum,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let killer = {
        let cl = chars.lock().await;
        let h = cl.iter().find(|p| p.id == killer_id).cloned();
        h
    };
    let Some(killer) = killer else { return };
    let want = killer.character.lock().await.autoloot;
    if !want { return; }
    // Find the most recently spawned corpse in the room matching this mob.
    let (corpse_id, drained) = {
        let mut w = world.lock().await;
        let cid = w.obj_instances.iter().rev()
            .find(|o| o.in_room == room
                && o.corpse_of.as_deref() == Some(mob_short))
            .map(|o| o.id);
        let Some(cid) = cid else { return; };
        let contents = w.obj_instances.iter_mut()
            .find(|o| o.id == cid)
            .map(|o| std::mem::take(&mut o.contents))
            .unwrap_or_default();
        (cid, contents)
    };
    if drained.is_empty() { return; }
    // Push the drained items into the killer's inventory.
    let mut moved: Vec<String> = Vec::new();
    {
        let mut c = killer.character.lock().await;
        let w = world.lock().await;
        for iid in &drained {
            if let Some(o) = w.obj_instances.iter().find(|o| o.id == *iid) {
                if let Some(p) = w.obj_protos.get(&o.vnum) {
                    moved.push(p.short_description.clone());
                }
            }
            c.inventory.push(*iid);
        }
    }
    let line = if moved.len() == 1 {
        format!("\r\nYou take {} from the corpse.\r\n", moved[0])
    } else {
        format!("\r\nYou loot {} items from the corpse.\r\n", moved.len())
    };
    let _ = killer.send.send(line);
    let _ = corpse_id;
}

/// External entry to the `kill_mob` path — used by the immortal
/// `slay` command.  Drops the standard kill flow including DEATH
/// triggers, corpse spawn, and fighting-state cleanup.
pub async fn kill_mob_immediate(
    mob_id: u32,
    room: crate::world::RoomVnum,
    mob_name: &str,
    killer_name: &str,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    kill_mob(mob_id, room, mob_name, killer_name, world, chars).await;
}

/// Remove a dead mob from the world. Spawns a corpse container in the room
/// holding the mob's former inventory.
async fn kill_mob(
    mob_id: u32,
    room: crate::world::RoomVnum,
    mob_name: &str,
    killer_name: &str,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    // Fire any DEATH triggers BEFORE the mob is extracted.
    crate::interpreter::fire_mob_death_triggers(mob_id, killer_name, world, chars).await;
    {
        let mut w = world.lock().await;
        let inv: Vec<u32> = w.mob_instances.iter()
            .find(|m| m.id == mob_id)
            .map(|m| m.inventory.clone())
            .unwrap_or_default();

        // Clear any other mob/player fighting state targeting this mob.
        for other in w.mob_instances.iter_mut() {
            if other.fighting.map(|t| !t.is_player && t.id == mob_id).unwrap_or(false) {
                other.fighting = None;
            }
        }
        // Remove the mob from its room and the instance vec.
        if let Some(r) = w.rooms.get_mut(&room) {
            r.mobs.retain(|&id| id != mob_id);
        }
        w.mob_instances.retain(|m| m.id != mob_id);

        // Spawn the corpse holding the mob's inventory.
        w.create_corpse(mob_name, inv, room);
    }

    let cl = chars.lock().await;
    cl.broadcast_room(
        room, None,
        &format!("\r\n{killer_name} has slain {mob_name}!\r\n"),
    );
    cl.broadcast_room(
        room, None,
        &format!("{mob_name} collapses to the floor, dead.\r\n"),
    );
}

/// Player has dropped to 0 HP.  Heal to full and respawn at the mortal
/// start room (no XP loss yet).
async fn player_death(
    ph: &crate::character::PlayerHandle,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    // Snapshot inventory before reset so we can drop it as a corpse.
    let (old_room, start_room, max_hp, dropped, corpse_label) = {
        let mut c = ph.character.lock().await;
        let immortal = c.level >= 34;
        let start = world.lock().await.start_room(immortal);
        let old = c.current_room;
        let dropped: Vec<u32> = std::mem::take(&mut c.inventory);
        let label = format!("corpse of {}", c.name);
        c.hp           = c.max_hp;
        c.current_room = start;
        c.fighting     = None;
        (old, start, c.max_hp, dropped, label)
    };

    // Spawn the corpse in the death room (if it's a real room) holding
    // the dropped inventory.  Equipped items stay on the body.
    if old_room != crate::world::NOWHERE && !dropped.is_empty() {
        let mut w = world.lock().await;
        w.create_corpse(&corpse_label, dropped, old_room);
    }

    // Update the registry's cached current_room.
    {
        let mut cl = chars.lock().await;
        cl.update_room(ph.id, start_room);
        // Any mob still pointing at us as a target loses interest.
        {
            let mut w = world.lock().await;
            for m in w.mob_instances.iter_mut() {
                if m.fighting.map(|t| t.is_player && t.id == ph.id).unwrap_or(false) {
                    m.fighting = None;
                }
            }
        }
        cl.broadcast_room(old_room, None,
            &format!("\r\n{} dies, their body fading away into the ether.\r\n", ph.name));
        cl.broadcast_room(start_room, Some(ph.id),
            &format!("\r\n{} materializes here, looking dazed.\r\n", ph.name));
    }

    let _ = ph.send.send(format!(
        "\r\n\r\n*** You have died. ***\r\n\r\nYou awaken at the temple, restored to {} hp.\r\n",
        max_hp,
    ));
    // Show the new room.
    let view = crate::interpreter::render_room(start_room, Some(ph.id), world, chars).await;
    let _ = ph.send.send(view);
    let _ = ph.send.send("\r\n> ".to_string());
}

/// Fire FIGHT (`k`) triggers each combat round for every mob currently
/// in combat against a player.  The trigger's actor is the player they're
/// fighting; `narg` controls the per-round fire chance.
async fn fight_trigger_tick(world: &Arc<Mutex<World>>, chars: &SharedChars) {
    // Snapshot fighting mob ids + their opponent player ids.
    let pairs: Vec<(u32, u32)> = {
        let w = world.lock().await;
        w.mob_instances.iter()
            .filter_map(|m| m.fighting.and_then(|t|
                if t.is_player { Some((m.id, t.id)) } else { None }))
            .collect()
    };
    for (mob_id, player_id) in pairs {
        // Player name lookup.
        let actor_name = {
            let cl = chars.lock().await;
            let name = cl.iter().find(|p| p.id == player_id).map(|p| p.name.clone());
            name
        };
        let Some(actor_name) = actor_name else { continue; };
        // Run any 'k' triggers via the generic fire path.
        crate::interpreter::fire_mob_fight_triggers(mob_id, &actor_name, world, chars).await;
    }
}

/// Heuristic: should this mob attempt to cast a spell in combat?
/// We don't have a class-on-mob field yet, so we infer from the mob's
/// keywords / short_descr. Anything level 10+ whose name contains a
/// caster archetype keyword qualifies.
fn should_cast(p: &crate::world::MobProto) -> bool {
    if p.level < 10 { return false; }
    const KW: &[&str] = &[
        "mage", "wizard", "sorcer", "witch", "warlock",
        "priest", "cleric", "shaman", "druid", "necromancer",
    ];
    let bag = format!("{} {}", p.name.to_ascii_lowercase(), p.short_descr.to_ascii_lowercase());
    KW.iter().any(|k| bag.contains(k))
}

// Tiny no-op so unused imports don't warn during incremental builds.
#[allow(dead_code)]
fn _silence(_: &Character) {}
