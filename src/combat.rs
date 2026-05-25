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
    character::{Character, SharedChars, Target},
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
    // ----- Phase 1: snapshot all fighters --------------------------------
    // Avoid holding two locks; collect intents then mutate.
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
                    room:          me.current_room,
                    target:        tgt,
                });
            }
        }
        v
    };

    // ----- Phase 2: resolve player attacks -------------------------------
    for intent in player_intents {
        resolve_player_attack(intent, world, chars).await;
    }

    // ----- Phase 3: snapshot mob attackers -------------------------------
    let mob_intents: Vec<MobIntent> = {
        let w = world.lock().await;
        w.mob_instances.iter()
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
}

struct PlayerIntent {
    attacker_id:   u32,
    attacker_name: String,
    level:         i32,
    room:          crate::world::RoomVnum,
    target:        Target,
}

struct MobIntent {
    attacker_id:   u32,
    attacker_vnum: i32,
    room:          crate::world::RoomVnum,
    target:        Target,
    level:         i32,
}

async fn resolve_player_attack(
    p: PlayerIntent,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    if p.target.is_player {
        return; // PvP not supported in this checkpoint
    }

    // Roll damage: 1..(level+2). Crude but functional.
    let dmg: i32 = rand::thread_rng().gen_range(1..=(p.level + 2).max(2));

    let (target_name, target_dead, target_room) = {
        let mut w = world.lock().await;

        // Read-only first: existence, room, proto name.
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
        let proto_name = w.mob_protos.get(&vnum)
            .map(|pr| pr.short_descr.clone())
            .unwrap_or_else(|| "the creature".into());

        // Now the mutation.
        let m = w.mob_instances.iter_mut().find(|m| m.id == p.target.id).unwrap();
        m.hp -= dmg;
        let dead = m.hp <= 0;
        if !dead && m.fighting.is_none() {
            m.fighting = Some(Target { id: p.attacker_id, is_player: true });
        }
        (proto_name, dead, in_room)
    };

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

    if target_dead {
        kill_mob(p.target.id, target_room, &target_name, &p.attacker_name, world, chars).await;
        // Clear the player's fighting state since the mob is gone.
        clear_player_fighting(p.attacker_id, chars).await;
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

    let dmg: i32 = rand::thread_rng().gen_range(1..=(m.level + 2).max(2));

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

    // Apply damage to the player; check death.
    let (player_dead, player_room, mob_short_name) = {
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
        (c.hp <= 0, c.current_room, short)
    };

    let to_victim = format!("\r\n{mob_short_name} hits you for {dmg} damage.\r\n");
    let to_room   = format!("\r\n{mob_short_name} hits {}.\r\n", ph.name);
    let _ = ph.send.send(to_victim);
    {
        let cl = chars.lock().await;
        cl.broadcast_room(m.room, Some(m.target.id), &to_room);
    }

    if player_dead {
        player_death(&ph, world, chars).await;
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

/// Remove a dead mob from the world. Drops the mob's inventory onto the
/// floor as ground items so the killer can collect them.
async fn kill_mob(
    mob_id: u32,
    room: crate::world::RoomVnum,
    mob_name: &str,
    killer_name: &str,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let dropped: Vec<u32> = {
        let mut w = world.lock().await;
        // Take the mob's inventory before extracting.
        let inv: Vec<u32> = w.mob_instances.iter()
            .find(|m| m.id == mob_id)
            .map(|m| m.inventory.clone())
            .unwrap_or_default();
        // Move objects to the room.
        if let Some(r) = w.rooms.get_mut(&room) {
            for &iid in &inv {
                r.objects.push(iid);
            }
        }
        for &iid in &inv {
            if let Some(o) = w.obj_instances.iter_mut().find(|o| o.id == iid) {
                o.in_room = room;
            }
        }
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
        inv
    };

    // Broadcast the death.
    {
        let cl = chars.lock().await;
        cl.broadcast_room(
            room, None,
            &format!("\r\n{killer_name} has slain {mob_name}!\r\n"),
        );
        if !dropped.is_empty() {
            cl.broadcast_room(
                room, None,
                &format!("{mob_name}'s belongings tumble to the floor.\r\n"),
            );
        }
    }
}

/// Player has dropped to 0 HP.  Heal to full and respawn at the mortal
/// start room (no XP loss yet).
async fn player_death(
    ph: &crate::character::PlayerHandle,
    world: &Arc<Mutex<World>>,
    chars: &SharedChars,
) {
    let (old_room, start_room, max_hp) = {
        let mut c = ph.character.lock().await;
        let immortal = c.level >= 34;
        let start = world.lock().await.start_room(immortal);
        let old = c.current_room;
        c.hp           = c.max_hp;
        c.current_room = start;
        c.fighting     = None;
        (old, start, c.max_hp)
    };

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

// Tiny no-op so unused imports don't warn during incremental builds.
#[allow(dead_code)]
fn _silence(_: &Character) {}
