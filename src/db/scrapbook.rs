use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, atomic::AtomicBool},
    time::{Duration, Instant},
};

use chrono::Utc;
use nohash_hasher::IntMap;
use sf_api::gamestate::unlockables::ScrapBook;
use sqlx::{Pool, Postgres};
use tokio::{sync::RwLock, task::JoinHandle, time::sleep};

use crate::{common::*, db::*, error::SFSError, types::*};

async fn read_full_player_db(
    db: &Pool<Postgres>,
    server_id: i32,
) -> Result<PCacheValue, SFSError> {
    let now = Instant::now();
    let res = sqlx::query!(
        "SELECT player_id, attributes, equipment
        FROM player
        WHERE server_id = $1 AND is_removed = FALSE",
        server_id
    )
    .fetch_all(db)
    .await?;

    log::info!(
        "Read {server_id}'s {} players in {:?}",
        res.len(),
        now.elapsed()
    );

    let mut vals = PCacheValue::default();

    let mut equip: HashMap<i32, HashSet<u32>> = HashMap::new();

    for r in res {
        let idents = r.equipment.unwrap_or_default();
        if idents.is_empty() {
            continue;
        }
        for equip_ident in idents {
            equip
                .entry(equip_ident)
                .or_default()
                .insert(r.player_id as u32);
        }
        let info = CharacterInfo {
            stats: r.attributes.unwrap_or(i64::MAX) as u64,
            player_id: r.player_id,
        };
        vals.player_info.insert(r.player_id as u32, info);
    }
    let mut saved = 0;
    let mut total = 1;
    vals.equipment = equip
        .into_iter()
        .map(|(e_ident, players)| {
            saved += players.capacity() - players.len();
            total += players.capacity();
            (e_ident, players.into_iter().collect())
        })
        .collect();
    log::info!("Saved {}%", (saved as f32 / total as f32) * 100.0);
    Ok(vals)
}

#[derive(Debug, Default)]
struct PCacheValue {
    player_info: IntMap<u32, CharacterInfo>,
    // Equipment id => player ids
    equipment: IntMap<i32, Box<[u32]>>,
}

// Maps the server_id to a list of all servers on the server
static SERVER_PLAYER_CACHE: CacheMap<i32, Arc<PCacheValue>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

async fn fill_all_server_caches(db: Pool<Postgres>) -> Result<(), SFSError> {
    let mut first_iter = true;
    loop {
        if !first_iter {
            sleep(Duration::from_secs(60 * 60)).await;
        }

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
            let task: JoinHandle<Result<(), SFSError>> =
                tokio::spawn(async move {
                    let db = db.clone();
                    let res = Arc::new(read_full_player_db(&db, id).await?);
                    let mut spc = SERVER_PLAYER_CACHE.write().await;
                    spc.insert(id, res.clone());
                    drop(spc);
                    Ok(())
                });
            if first_iter {
                tasks.push(task);
            } else {
                _ = task.await;
            }
        }
        first_iter = false;
        for task in tasks {
            if let Err(e) = task.await {
                log::error!("Could not fetch server data: {e}");
            }
        }
    }
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
    static HAS_STARTED_UPDATES: AtomicBool = AtomicBool::new(false);

    {
        let k = RESULT_CACHE.read().await;
        if let Some(entry) = k.get(&args)
            && entry.insertion_time + hours(1) > Utc::now()
        {
            log::info!("Skipped sb advice using cache");
            return Ok(entry.result.clone());
        }
    }

    let db = get_db().await?;

    if HAS_STARTED_UPDATES
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
    {
        let db = db.clone();
        tokio::spawn(async move { fill_all_server_caches(db).await });
    }

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
    .await
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
) -> Vec<ScrapBookAdvice> {
    let result_limit = 10;
    let Some(scrapbook) = ScrapBook::parse(&args.raw_scrapbook) else {
        return vec![];
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
        find_best(&per_player_counts, player_info, result_limit, db).await;
    best.sort_by(|a, b| {
        b.new_count.cmp(&a.new_count).then(a.stats.cmp(&b.stats))
    });
    best
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct CharacterInfo {
    stats: u64,
    player_id: i32,
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
) -> Vec<ScrapBookAdvice> {
    // Prune the counts to make computation faster
    let mut max = 1;
    let mut counts = [(); 10].map(|_| vec![]);
    for (player, count) in per_player_counts.iter().map(|a| (*a.0, *a.1)) {
        if max_out == 1 && count < max || count == 0 {
            continue;
        }
        max = max.max(count);
        counts[(count - 1).clamp(0, 9)].push(player);
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

    let winner_ids: Vec<_> =
        best_players.iter().map(|a| a.1.player_id).collect();
    let names = sqlx::query!(
        "SELECT player_id, name FROM player WHERE player_id = ANY($1)",
        winner_ids.as_slice()
    )
    .fetch_all(db)
    .await
    .unwrap();

    let names: HashMap<_, _> =
        names.into_iter().map(|a| (a.player_id, a.name)).collect();

    best_players
        .into_iter()
        .map(|a| ScrapBookAdvice {
            player_name: names
                .get(&a.1.player_id)
                .cloned()
                .unwrap_or_else(|| a.0.to_string()),
            new_count: a.0,
            stats: a.1.stats,
        })
        .collect()
}
