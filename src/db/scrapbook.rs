use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock},
    time::Instant,
};

use chrono::{Local, Utc};
use log::warn;
use nohash_hasher::IntMap;
use sf_api::session::Response;
use sqlx::{Pool, Postgres};
use tokio::sync::RwLock;

use crate::{
    common::{compress_ident, hours},
    db::{
        CacheEntry, CacheMap, get_db, get_gamestate, get_server_id,
        update::UPDATE_SENDER,
    },
    error::SFSError,
    types::{ScrapBookAdvice, ScrapBookAdviceArgs},
};

// ── Delta+Varint compressed sorted integer array ──

/// Stores a sorted array of `i32` values using delta encoding + varint
/// compression.
///
/// Memory layout:
/// - `first`: the first (minimum) value
/// - `data`: varint-encoded deltas between consecutive values
/// - `len`: total number of elements
///
/// Typical compression: ~2 bytes per entry (vs 4 for raw `i32`).
#[derive(Debug, Clone)]
pub struct DeltaVarintArray {
    data: Box<[u8]>,
    first: i32,
    len: u32,
}

impl DeltaVarintArray {
    /// Build a compressed array from a **sorted** slice of values.
    fn from_sorted(values: &[i32]) -> Self {
        if values.is_empty() {
            return Self {
                data: Box::new([]),
                first: 0,
                len: 0,
            };
        }
        let mut encoded = Vec::with_capacity(values.len());
        let first = values.first().copied().unwrap_or(0);
        let mut prev = first as u64;
        for v in values.get(1..).unwrap_or_default().iter().copied() {
            let delta = (v as u64).wrapping_sub(prev);
            encode_varint(delta, &mut encoded);
            prev = v as u64;
        }
        Self {
            data: encoded.into_boxed_slice(),
            first,
            len: values.len() as u32,
        }
    }

    /// Decompress to a `Vec<i32>`.
    fn to_vec(&self) -> Vec<i32> {
        let mut result = Vec::with_capacity(self.len as usize);
        if self.len == 0 {
            return result;
        }
        result.push(self.first);
        let mut current = self.first as u64;
        let mut pos = 0;
        while pos < self.data.len() {
            let (delta, bytes) =
                decode_varint(self.data.get(pos..).unwrap_or_default());
            current = current.wrapping_add(delta);
            result.push(current as i32);
            pos += bytes;
        }
        result
    }

    /// Returns `true` if the array is empty.
    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the number of elements.
    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.len as usize
    }

    /// Iterate over the decompressed values (lazy, no allocation).
    #[allow(clippy::iter_without_into_iter)]
    pub(crate) fn iter(&self) -> DeltaVarintIter<'_> {
        DeltaVarintIter {
            data: &self.data,
            pos: 0,
            current: self.first as u64,
            remaining: self.len as usize,
        }
    }
}

/// Lazy iterator over a `DeltaVarintArray`.
pub(crate) struct DeltaVarintIter<'a> {
    data: &'a [u8],
    pos: usize,
    current: u64,
    remaining: usize,
}

impl Iterator for DeltaVarintIter<'_> {
    type Item = i32;

    fn next(&mut self) -> Option<i32> {
        if self.remaining == 0 {
            return None;
        }
        let result = self.current as i32;
        self.remaining -= 1;
        if self.remaining > 0 {
            let (delta, bytes_read) =
                decode_varint(self.data.get(self.pos..).unwrap_or_default());
            self.current = self.current.wrapping_add(delta);
            self.pos += bytes_read;
        }
        Some(result)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

fn encode_varint(mut value: u64, buf: &mut Vec<u8>) {
    loop {
        if value < 128 {
            buf.push(value as u8);
            break;
        }
        buf.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
}

fn decode_varint(buf: &[u8]) -> (u64, usize) {
    let mut result = 0u64;
    let mut shift = 0;
    let mut bytes_read = 0;
    for &byte in buf {
        bytes_read += 1;
        result |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        assert!(shift <= 63, "varint too long");
    }
    (result, bytes_read)
}

// ── Scrapbook data structures ──

#[derive(Debug, Default)]
pub(crate) struct ServerScrapbookInfo {
    // playerid => stats
    pub(crate) player_info: IntMap<i32, u64>,
    // Equipment id => player ids (compressed, sorted)
    pub(crate) equipment: IntMap<i32, DeltaVarintArray>,
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

    // Decompress equipment arrays to Vec<i32> for mutation.
    // This uses ~14 MB temporary memory per server instead of ~130 MB
    // (HashSet).
    let mut equipment: HashMap<i32, Vec<i32>> = cache_entry
        .equipment
        .iter()
        .map(|(eq_id, arr)| (*eq_id, arr.to_vec()))
        .collect();

    drop(cache_entry);

    for (pid, (player_equip, info)) in player_updates {
        player_info.insert(pid, info.stats);

        if is_initial_fetch {
            // No need to clear old entries, we just insert all players
            for equip_ident in player_equip {
                equipment.entry(equip_ident).or_default().push(pid);
            }
            continue;
        }

        for (ident, players) in &mut equipment {
            // player_equip is <= 10 elements, so a linear scan is cheaper
            // than building a hashset for each update.
            let currently_owns = player_equip.contains(ident);

            if currently_owns {
                // Insert only if not already present (binary search).
                let Err(idx) = players.binary_search(&pid) else {
                    continue;
                };
                players.insert(idx, pid);
            } else {
                // Remove if present (binary search).
                if let Ok(idx) = players.binary_search(&pid) {
                    players.remove(idx);
                }
            }
        }
    }

    // Re-sort and dedup equipment arrays that were modified during initial
    // fetch. (Incremental updates maintain sorted order via binary-search
    // inserts.)
    if is_initial_fetch {
        for players in equipment.values_mut() {
            players.sort_unstable();
            players.dedup();
        }
    }

    let new_data = ServerScrapbookInfo {
        player_info,
        equipment: equipment
            .into_iter()
            .map(|(eq_id, vec)| (eq_id, DeltaVarintArray::from_sorted(&vec)))
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
    equipment: &IntMap<i32, DeltaVarintArray>,
    db: &Pool<Postgres>,
) -> Result<Vec<ScrapBookAdvice>, SFSError> {
    let result_limit = 10;
    let mut gs = get_gamestate().await;
    let Ok(response) = Response::parse(
        format!("scrapbook:{}", args.raw_scrapbook),
        Local::now().naive_utc(),
    ) else {
        warn!("Invalid scrapbook: {}", args.raw_scrapbook);
        return Ok(vec![]);
    };
    gs.character.scrapbook = None;
    if let Err(err) = gs.update(&response) {
        warn!("Invalid scrapbook: {err}");
        return Ok(vec![]);
    }
    let Some(scrapbook) = gs.character.scrapbook.take() else {
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

#[must_use]
pub fn calc_per_player_count(
    // player => Detailed info
    player_info: &IntMap<i32, u64>,
    // equipment => players
    equipment: &IntMap<i32, DeltaVarintArray>,
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
        for player in players.iter() {
            *per_player_counts.entry(player).or_insert(0) += 1;
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
