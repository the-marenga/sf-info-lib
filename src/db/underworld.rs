use std::{
    collections::{HashMap, hash_map::Entry},
    ops::Neg,
    sync::{Arc, LazyLock},
};

use nohash_hasher::IntMap;
use tokio::sync::RwLock;

use crate::{
    db::{update::UPDATE_SENDER, *},
    error::SFSError,
    types::*,
};

static NUDE_PLAYER_CACHE: CacheMap<i32, Arc<[UnderworldCacheEntry]>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub(crate) async fn update_underworld_enemies(
    server_id: i32,
    player_updates: &IntMap<i32, (Box<[i32]>, CharacterInfo)>,
) {
    let cache_entry = {
        let entry = NUDE_PLAYER_CACHE.read().await;
        entry.get(&server_id).cloned().unwrap_or_default()
    };
    let mut current: HashMap<_, _> = cache_entry
        .iter()
        .map(|a| (a.player_id, (a.level, a.stats)))
        .collect();

    for (player_id, (items, info)) in player_updates {
        if info.level < 25 {
            if info.stats == 0 {
                // Removed player
                current.remove(player_id);
            }
            // Do not care about super low level player, we would have too many
            // of them
            continue;
        }
        match current.entry(*player_id) {
            Entry::Occupied(mut occ) => {
                if items.len() > 3 {
                    // player put on items. Remove them from the list
                    occ.remove();
                    continue;
                }
                // Update old entry
                let occ = occ.get_mut();
                occ.0 = info.level;
                occ.1 = info.stats as i64;
            }
            Entry::Vacant(vac) => {
                if items.len() > 3 {
                    continue;
                }
                // Found a new good candidate
                vac.insert((info.level, info.stats as i64));
            }
        }
    }
    let mut result: Vec<UnderworldCacheEntry> = current
        .into_iter()
        .map(|a| UnderworldCacheEntry {
            stats: a.1.1,
            player_id: a.0,
            level: a.1.0,
        })
        .collect();

    result.sort_unstable_by_key(|a| a.stats.neg());

    let mut entry = NUDE_PLAYER_CACHE.write().await;
    entry.insert(server_id, result.into());
    drop(entry);
}

struct UnderworldCacheEntry {
    stats: i64,
    player_id: i32,
    level: u16,
}

pub async fn get_best_nude_players(
    args: UnderworldAdviceArgs,
) -> Result<Arc<[UnderworldAdvice]>, SFSError> {
    // NOTE: This is so fast, that we do not really need to cache this. We
    // still return Arc here, in case we ever want to do that

    let _init_caches = UPDATE_SENDER.weak_count();

    let db = get_db().await?;
    let server_id = get_server_id(&db, args.server.clone()).await?;

    let players = {
        let cache = NUDE_PLAYER_CACHE.read().await;
        let Some(players) = cache.get(&server_id) else {
            return Ok(Arc::default());
        };
        players.clone()
    };

    let max_attrs = args.max_attrs;

    let first_pos =
        match players.binary_search_by_key(&(-max_attrs), |a| -a.stats) {
            Ok(x) | Err(x) => x,
        };
    // Contains players with at most the given stats. These must now be sorted
    // by level basically.
    let filtered_player = players.get(first_pos..).unwrap_or(&[]);

    const OUTPUT_SIZE: usize = 20;
    // highest level => lowest level
    let mut best: Vec<&UnderworldCacheEntry> = Vec::with_capacity(OUTPUT_SIZE);
    for player in filtered_player {
        // TODO: Might want to sub sort by attributes here
        let insert_pos = best
            .binary_search_by_key(&(-i32::from(player.level)), |a| {
                -i32::from(a.level)
            });
        let insert_pos = match insert_pos {
            Ok(x) | Err(x) => x,
        };

        if best.len() == OUTPUT_SIZE {
            if insert_pos >= OUTPUT_SIZE {
                // As bad, or worse than currently worst
                continue;
            }
            best.pop();
        }
        best.insert(insert_pos, player);
    }

    let best_ids: Vec<_> = best.iter().map(|a| a.player_id).collect();
    let names = sqlx::query!(
        "SELECT player_id, name FROM player WHERE player_id = ANY($1)",
        best_ids.as_slice()
    )
    .fetch_all(&db)
    .await?;

    let names: HashMap<_, _> =
        names.into_iter().map(|a| (a.player_id, a.name)).collect();

    Ok(best
        .into_iter()
        .filter_map(|a| {
            Some(UnderworldAdvice {
                player_name: names.get(&a.player_id).cloned()?,
                stats: a.stats,
                level: a.level,
            })
        })
        .collect())
}
