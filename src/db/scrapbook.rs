use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock},
    time::Instant,
};

use chrono::Utc;
use nohash_hasher::IntMap;
use sf_api::gamestate::unlockables::ScrapBook;
use sqlx::{Pool, Postgres};
use tokio::{
    sync::{RwLock, mpsc::*},
    task::JoinHandle,
};

use crate::{common::*, db::*, error::SFSError, types::*};

async fn read_full_player_db(
    db: &Pool<Postgres>,
    server_id: i32,
) -> Result<ServerScrapbookInfo, SFSError> {
    let now = Instant::now();
    let player_records = sqlx::query!(
        "SELECT player_id, attributes, equipment
        FROM player
        WHERE server_id = $1 AND is_removed = FALSE",
        server_id
    )
    .fetch_all(db)
    .await?;

    log::info!(
        "Read {server_id}'s {} players in {:?}",
        player_records.len(),
        now.elapsed()
    );

    let mut vals = ServerScrapbookInfo::default();
    let mut equip: HashMap<i32, HashSet<u32>> = HashMap::new();

    for player_record in player_records {
        let idents = player_record.equipment.unwrap_or_default();
        if idents.is_empty() {
            continue;
        }
        for equip_ident in idents {
            equip
                .entry(equip_ident)
                .or_default()
                .insert(player_record.player_id as u32);
        }
        let info = CharacterInfo {
            stats: player_record.attributes.unwrap_or(i64::MAX) as u64,
        };
        vals.player_info.insert(player_record.player_id as u32, info);
    }

    // Box<[T]> has less overhead and we are not expected to resize this ever
    // again, so we just convert them
    vals.equipment = equip
        .into_iter()
        .map(|(e_ident, players)| (e_ident, players.into_iter().collect()))
        .collect();
    Ok(vals)
}

#[derive(Debug, Default)]
struct ServerScrapbookInfo {
    // playerid => Character info
    player_info: IntMap<u32, CharacterInfo>,
    // Equipment id => player ids
    equipment: IntMap<i32, Box<[u32]>>,
}

pub(crate) static UPDATE_SENDER: LazyLock<UnboundedSender<PlayerUpdate>> =
    LazyLock::new(|| {
        let (send, recv) = unbounded_channel();
        tokio::spawn(async move { fill_all_server_caches(recv).await });
        send
    });

// Maps the server_id to a info about items on that server
static SERVER_PLAYER_CACHE: CacheMap<i32, Arc<ServerScrapbookInfo>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub(crate) struct PlayerUpdate {
    pub(crate) player_id: u32,
    pub(crate) server_id: i32,
    pub(crate) items: Box<[i32]>,
    pub(crate) info: CharacterInfo,
}

pub(crate) async fn fill_all_server_caches(
    recv: UnboundedReceiver<PlayerUpdate>,
) -> Result<(), SFSError> {
    let db = get_db().await?;

    let servers = sqlx::query_scalar!(
        "SELECT server_id
        FROM server
        WHERE last_hof_crawl >= NOW() - interval '7 days'"
    )
    .fetch_all(&db)
    .await?;

    let mut tasks = vec![];
    for id in servers {
        let db = db.clone();
        let task: JoinHandle<Result<(), SFSError>> = tokio::spawn(async move {
            let db = db.clone();
            let res = Arc::new(read_full_player_db(&db, id).await?);
            let mut spc = SERVER_PLAYER_CACHE.write().await;
            spc.insert(id, res.clone());
            drop(spc);
            Ok(())
        });
        tasks.push(task);
    }
    for task in tasks {
        if let Err(e) = task.await {
            log::error!("Could not fetch server data: {e}");
        }
    }
    continuously_update_server_caches(recv).await
}

async fn continuously_update_server_caches(
    mut recv: UnboundedReceiver<PlayerUpdate>,
) -> Result<(), SFSError> {
    let mut server_updates = HashMap::new();
    while let Some(update) = recv.recv().await {
        let server_data = server_updates
            .entry(update.server_id)
            .or_insert_with(|| CacheEntry {
                result: IntMap::default(),
                insertion_time: Utc::now(),
            });
        server_data
            .result
            .insert(update.player_id, (update.items, update.info));

        if server_data.insertion_time + minutes(30) < Utc::now()
            || server_data.result.len() >= 3000
        {
            let mut updates = IntMap::default();
            server_data.insertion_time =
                Utc::now() + minutes(fastrand::u64(1..5));

            std::mem::swap(&mut server_data.result, &mut updates);
            tokio::spawn(apply_server_cache_updates(update.server_id, updates));
        }
    }

    Ok(())
}

async fn apply_server_cache_updates(
    server_id: i32,
    player_updates: IntMap<u32, (Box<[i32]>, CharacterInfo)>,
) {
    // Only update one server at a time in order to not exhaust memory
    static SEM: Mutex<()> = Mutex::const_new(());
    let permit = SEM.lock().await;

    let cache_entry = {
        let mut entry = SERVER_PLAYER_CACHE.write().await;
        entry.entry(server_id).or_default().clone()
    };

    let mut player_info = cache_entry.player_info.clone();
    let mut equipment: HashMap<i32, HashSet<u32>> = cache_entry
        .equipment
        .iter()
        .map(|a| (*a.0, a.1.iter().copied().collect()))
        .collect();
    drop(cache_entry);

    for (pid, (player_equip, info)) in player_updates {
        player_info.insert(pid, info);

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

    let mut entry = SERVER_PLAYER_CACHE.write().await;
    entry.insert(server_id, Arc::new(new_data));
    drop(entry);
    drop(permit);
}

/// Returns players, that have a lot of items, that are not yet in the
/// scrapbook. Evaluating ALL players can be prohibitively slow (> 20 secs),
/// so we look at progressively larger chunks of the playerbase until we find
/// a player with "enough" new items. This reduce the time to 15ms-2s in most
/// cases, especially for new chars
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

    let _init_the_lazy_lock = UPDATE_SENDER.weak_count();

    let db = get_db().await?;
    let id = get_server_id(&db, args.server.clone()).await?;

    let server_data = {
        let scp = SERVER_PLAYER_CACHE.read().await;
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
    player_info: &IntMap<u32, CharacterInfo>,
    equipment: &IntMap<i32, Box<[u32]>>,
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
}

pub fn calc_per_player_count(
    // player => Detailed info
    player_info: &IntMap<u32, CharacterInfo>,
    // equipment => players
    equipment: &IntMap<i32, Box<[u32]>>,
    // the items the player already has
    scrapbook: &HashSet<i32>,
    max_attrs: u64,
) -> HashMap<u32, usize> {
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
        let Some(info) = player_info.get(a) else {
            return false;
        };

        if info.stats > max_attrs {
            return false;
        }
        true
    });
    per_player_counts
}

async fn find_best(
    per_player_counts: &HashMap<u32, usize>,
    player_info: &IntMap<u32, CharacterInfo>,
    max_out: usize,
    db: &Pool<Postgres>,
) -> Result<Vec<ScrapBookAdvice>, SFSError> {
    // Prune the counts to make computation faster
    const MAX_EQUIP_COUNT: usize = 10;
    let mut max = 1;
    let mut counts = [(); MAX_EQUIP_COUNT].map(|()| vec![]);
    for (player, count) in per_player_counts.iter().map(|a| (*a.0, *a.1)) {
        if max_out == 1 && count < max || count == 0 {
            continue;
        }
        max = max.max(count);
        let idx = (count - 1).clamp(0, 9);
        let Some(slot) = counts.get_mut(idx) else {
            // Should never happen
            continue;
        };
        slot.push(player);
    }

    let mut best_players = Vec::new();
    for (count, players) in counts.iter().enumerate().rev() {
        best_players.extend(
            players
                .iter()
                .filter_map(|a| player_info.get(a))
                .map(|a| ((count + 1) as u32, a)),
        );
        if best_players.len() >= max_out {
            break;
        }
    }
    best_players.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.stats.cmp(&b.1.stats)));
    best_players.truncate(max_out);

    let winner_ids: Vec<_> = best_players.iter().map(|a| a.0 as i32).collect();
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
        .map(|a| ScrapBookAdvice {
            player_name: names
                .get(&(a.0 as i32))
                .cloned()
                .unwrap_or_else(|| a.0.to_string()),
            new_count: a.0,
            stats: a.1.stats,
        })
        .collect())
}
