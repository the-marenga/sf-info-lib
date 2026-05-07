use std::{collections::HashMap, sync::LazyLock};

use chrono::Utc;
use nohash_hasher::IntMap;
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    task::JoinHandle,
};

use crate::{
    common::minutes,
    db::{
        CacheEntry, CharacterInfo, Mutex, get_db,
        underworld::update_underworld_enemies, update_scrapbook_cache,
    },
    error::SFSError,
};

pub(crate) static UPDATE_SENDER: LazyLock<UnboundedSender<PlayerUpdate>> =
    LazyLock::new(|| {
        let (send, recv) = unbounded_channel();
        tokio::spawn(async move { fill_all_server_caches(recv).await });
        send
    });

pub(crate) struct PlayerUpdate {
    // Internal id, not server player id
    pub(crate) player_id: i32,
    pub(crate) server_id: i32,
    pub(crate) items: Box<[i32]>,
    pub(crate) info: CharacterInfo,
}

/// Initially fills the scrapbook cache with player data from the database.
/// Once that is done, it will start reading and applying updates from the
/// given receiver
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
    for server_id in servers {
        let db = db.clone();
        let task: JoinHandle<Result<(), SFSError>> = tokio::spawn(async move {
            let player_records = sqlx::query!(
                "SELECT player_id, attributes, equipment, level
                    FROM player
                    WHERE server_id = $1 AND is_removed = FALSE",
                server_id
            )
            .fetch_all(&db)
            .await?;

            let updates: IntMap<_, _> = player_records
                .into_iter()
                .filter_map(|a| {
                    Some((
                        a.player_id,
                        (
                            a.equipment?.into_boxed_slice(),
                            CharacterInfo {
                                stats: a.attributes? as u64,
                                level: a.level? as u16,
                            },
                        ),
                    ))
                })
                .collect();

            update_underworld_enemies(server_id, &updates).await;
            update_scrapbook_cache(server_id, updates).await;
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

/// Reads player data updates from the given receiver and stores them grouped
/// by the server. Once a sufficiently large amount of player updates have been
/// found, or enough time has passed, server batches will be written to the
/// actual cache.
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
/// Updates the server cache with the given updates. Each update requires
/// cloning the player data, converting it to more update friendly datatypes,
/// updating the existing data and interacting with the cache (read & lock),
/// which is why we have to do this work in per server batches, not immediately
async fn apply_server_cache_updates(
    server_id: i32,
    player_updates: IntMap<i32, (Box<[i32]>, CharacterInfo)>,
) {
    // Only update one server at a time in order to not exhaust memory
    static SEM: Mutex<()> = Mutex::const_new(());
    let permit = SEM.lock().await;

    log::info!(
        "Updating cache for {} players on {server_id}",
        player_updates.len()
    );
    update_underworld_enemies(server_id, &player_updates).await;
    update_scrapbook_cache(server_id, player_updates).await;

    drop(permit);
}
