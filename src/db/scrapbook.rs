use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock},
    time::Instant,
};

use chrono::Utc;
use nohash_hasher::IntMap;
use sf_api::gamestate::unlockables::ScrapBook;
use sqlx::{Pool, Postgres};
use tokio::sync::RwLock;

use crate::{
    common::*,
    db::{update::UPDATE_SENDER, *},
    error::SFSError,
    types::*,
};

#[derive(Debug, Default)]
pub(crate) struct ServerScrapbookInfo {
    // playerid => stats
    pub(crate) player_info: IntMap<i32, u64>,
    // Equipment id => player ids
    pub(crate) equipment: IntMap<i32, Box<[i32]>>,
}

// Maps the server_id to a info about items on that server
static SCRAPBOOK_PLAYER_CACHE: CacheMap<i32, Arc<ServerScrapbookInfo>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub(crate) async fn update_scrapbook_cache(
    server_id: i32,
    player_updates: IntMap<i32, (Box<[i32]>, CharacterInfo)>,
) {
    let cache_entry = {
        let entry = SCRAPBOOK_PLAYER_CACHE.read().await;
        entry.get(&server_id).cloned().unwrap_or_default()
    };
    let is_initial_fetch = cache_entry.equipment.is_empty();

    let mut player_info = cache_entry.player_info.clone();
    let mut equipment: HashMap<i32, HashSet<i32>> = cache_entry
        .equipment
        .iter()
        .map(|a| (*a.0, a.1.iter().copied().collect()))
        .collect();

    drop(cache_entry);

    for (pid, (player_equip, info)) in player_updates {
        player_info.insert(pid, info.stats);

        if is_initial_fetch {
            // No need to clear old entries, we just insert all players
            for equip_ident in player_equip {
                equipment.entry(equip_ident).or_default().insert(pid);
            }
            continue;
        }

        for (ident, equipment) in &mut equipment {
            // player_equip is <= 10 elements, so constructing and querying a
            // hashset would likely be slower here
            if player_equip.contains(ident) {
                equipment.insert(pid);
            } else {
                // Remove any old equipment. This will mostly remove nothing,
                // but we have to do this. The alternative would be to track
                // the old equipment (which is what the helper does), but that
                // roughly doubles the memory usage, so no...
                equipment.remove(&pid);
            }
        }
    }

    let new_data = ServerScrapbookInfo {
        player_info,
        equipment: equipment
            .into_iter()
            .map(|(a, b)| (a, b.into_iter().collect()))
            .collect(),
    };

    let mut entry = SCRAPBOOK_PLAYER_CACHE.write().await;
    entry.insert(server_id, Arc::new(new_data));
    drop(entry);
}

/// Returns players, that have a lot of items, that are not yet in the
/// scrapbook
pub async fn get_scrapbook_advice(
    args: ScrapBookAdviceArgs,
) -> Result<Arc<[ScrapBookAdvice]>, SFSError> {
    static RESULT_CACHE: CacheMap<
        ScrapBookAdviceArgs,
        CacheEntry<Arc<[ScrapBookAdvice]>>,
    > = LazyLock::new(|| RwLock::new(HashMap::new()));

    {
        let k = RESULT_CACHE.read().await;
        if let Some(entry) = k.get(&args)
            && entry.insertion_time + hours(1) > Utc::now()
        {
            log::info!("Skipped sb advice using cache");
            return Ok(entry.result.clone());
        }
    }

    let _init_caches = UPDATE_SENDER.weak_count();

    let db = get_db().await?;
    let id = get_server_id(&db, args.server.clone()).await?;

    let server_data = {
        let scp = SCRAPBOOK_PLAYER_CACHE.read().await;
        let Some(entry) = scp.get(&id) else {
            return Ok(Arc::default());
        };
        entry.clone()
    };

    let now = Instant::now();
    let res: Arc<[ScrapBookAdvice]> = calc_best_targets(
        &args,
        &server_data.player_info,
        &server_data.equipment,
        &db,
    )
    .await?
    .into();
    log::info!("Calculated: {:?}", now.elapsed());

    let now = Utc::now();
    let mut k = RESULT_CACHE.write().await;
    k.insert(
        args,
        CacheEntry {
            result: res.clone(),
            insertion_time: now,
        },
    );

    if fastrand::u64(..10_000) == 0 {
        k.retain(|_, v| v.insertion_time + hours(1) > now);
    }
    Ok(res)
}

async fn calc_best_targets(
    args: &ScrapBookAdviceArgs,
    player_info: &IntMap<i32, u64>,
    equipment: &IntMap<i32, Box<[i32]>>,
    db: &Pool<Postgres>,
) -> Result<Vec<ScrapBookAdvice>, SFSError> {
    let result_limit = 10;
    let Some(scrapbook) = ScrapBook::parse(&args.raw_scrapbook) else {
        return Ok(vec![]);
    };

    let scrapbook: HashSet<i32> =
        scrapbook.items.into_iter().map(compress_ident).collect();

    let per_player_counts = calc_per_player_count(
        player_info,
        equipment,
        &scrapbook,
        args.max_attrs,
    );

    let mut best =
        find_best(&per_player_counts, player_info, result_limit, db).await?;
    best.sort_by(|a, b| {
        b.new_count.cmp(&a.new_count).then(a.stats.cmp(&b.stats))
    });
    Ok(best)
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct CharacterInfo {
    pub(crate) stats: u64,
    // *Sad memory alignment noises*
    pub(crate) level: u16,
}

pub fn calc_per_player_count(
    // player => Detailed info
    player_info: &IntMap<i32, u64>,
    // equipment => players
    equipment: &IntMap<i32, Box<[i32]>>,
    // the items the player already has
    scrapbook: &HashSet<i32>,
    max_attrs: u64,
) -> HashMap<i32, usize> {
    let mut per_player_counts = HashMap::default();
    per_player_counts.reserve(player_info.len());

    for (eq, players) in equipment {
        if scrapbook.contains(eq) {
            continue;
        }
        for player in players {
            *per_player_counts.entry(*player).or_insert(0) += 1;
        }
    }

    per_player_counts.retain(|a, _| {
        let Some(stats) = player_info.get(a) else {
            return false;
        };

        if *stats > max_attrs {
            return false;
        }
        true
    });
    per_player_counts
}

async fn find_best(
    per_player_counts: &HashMap<i32, usize>,
    player_info: &IntMap<i32, u64>,
    max_out: usize,
    db: &Pool<Postgres>,
) -> Result<Vec<ScrapBookAdvice>, SFSError> {
    // Prune the counts to make computation faster
    const MAX_EQUIP_COUNT: usize = 10;
    let mut max = 1;

    // Map amount of new items => player ids
    let mut counts = [(); MAX_EQUIP_COUNT].map(|()| vec![]);
    for (player_id, count) in per_player_counts.iter().map(|a| (*a.0, *a.1)) {
        if max_out == 1 && count < max || count == 0 {
            continue;
        }
        max = max.max(count);
        let new_itm_counts = (count - 1).clamp(0, 9);
        let Some(slot) = counts.get_mut(new_itm_counts) else {
            // Should never happen
            continue;
        };
        slot.push(player_id);
    }

    struct RawScrapBookAdvice {
        player_id: i32,
        count: u32,
        stats: u64,
    }

    let mut best_players = Vec::new();
    for (count, players) in counts.into_iter().enumerate().rev() {
        for player_id in players {
            if let Some(stats) = player_info.get(&player_id) {
                best_players.push(RawScrapBookAdvice {
                    player_id,
                    count: (count + 1) as u32,
                    stats: *stats,
                });
            }
        }
        if best_players.len() >= max_out {
            break;
        }
    }
    best_players
        .sort_by(|a, b| b.count.cmp(&a.count).then(a.stats.cmp(&b.stats)));
    best_players.truncate(max_out);

    let winner_ids: Vec<_> = best_players.iter().map(|a| a.player_id).collect();
    let names = sqlx::query!(
        "SELECT player_id, name FROM player WHERE player_id = ANY($1)",
        winner_ids.as_slice()
    )
    .fetch_all(db)
    .await?;

    let names: HashMap<_, _> =
        names.into_iter().map(|a| (a.player_id, a.name)).collect();

    Ok(best_players
        .into_iter()
        .filter_map(|a| {
            Some(ScrapBookAdvice {
                player_name: names.get(&a.player_id).cloned()?,
                new_count: a.count,
                stats: a.stats,
            })
        })
        .collect())
}
