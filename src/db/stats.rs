#![allow(unused)]

use std::sync::{Arc, LazyLock};

use tokio::sync::RwLock;

use crate::{common::*, db::*, error::SFSError, types::*};

pub async fn get_servers() -> Result<Arc<[ServerInfo]>, SFSError> {
    static RESULT_CACHE: CacheSlot<Arc<[ServerInfo]>> =
        LazyLock::new(Default::default);

    let mut cache_res = RESULT_CACHE.lock().await;

    if let Some(res) = &*cache_res
        && res.1.elapsed() < hours(6)
    {
        return Ok(res.0.clone());
    }

    let db = get_db().await?;

    let server_ids = sqlx::query!("SELECT url, server_id FROM server")
        .fetch_all(&db)
        .await?;

    let mut server_ids: HashMap<_, _> = server_ids
        .into_iter()
        .map(|a| (a.server_id, a.url))
        .collect();

    let info = sqlx::query!(
        "
        SELECT
            server_id,
            count(*) AS total,
            count(*) FILTER (WHERE last_changed + interval '3 days' > now() AT \
         TIME ZONE 'UTC' ) AS active,
         count(*) FILTER (WHERE class = 0) AS warriors,
         count(*) FILTER (WHERE class = 1) AS mages,
         count(*) FILTER (WHERE class = 2) AS scouts,
         count(*) FILTER (WHERE class = 3) AS assassins,
         count(*) FILTER (WHERE class = 4) AS battle_mages,
         count(*) FILTER (WHERE class = 5) AS berserkers,
         count(*) FILTER (WHERE class = 6) AS demon_hunters,
         count(*) FILTER (WHERE class = 7) AS druids,
         count(*) FILTER (WHERE class = 8) AS bards,
         count(*) FILTER (WHERE class = 9) AS necromancer,
         count(*) FILTER (WHERE class = 10) AS paladins,
         avg(level)::int as avg_level
        FROM
       	player
            GROUP BY
           	server_id
            ORDER BY
    	count(*) DESC;
	"
    )
    .fetch_all(&db)
    .await?;

    let mut res: Vec<ServerInfo> = Vec::new();

    for server_info in info {
        let Some(url) = server_ids.remove(&server_info.server_id) else {
            continue;
        };

        let Some((shorthand, category)) = url_to_info(&url) else {
            continue;
        };

        let info = ServerInfo {
            url,
            shorthand,
            category,
            player_count: server_info.total.unwrap_or(0),
            active_players: server_info.active.unwrap_or(0),
            classes: [
                ("Warrior".to_string(), server_info.warriors.unwrap_or(0)),
                ("Mage".to_string(), server_info.mages.unwrap_or(0)),
                ("Scout".to_string(), server_info.scouts.unwrap_or(0)),
                ("Assassin".to_string(), server_info.assassins.unwrap_or(0)),
                (
                    "BattleMage".to_string(),
                    server_info.battle_mages.unwrap_or(0),
                ),
                ("Berserker".to_string(), server_info.berserkers.unwrap_or(0)),
                (
                    "DemonHunter".to_string(),
                    server_info.demon_hunters.unwrap_or(0),
                ),
                ("Druid".to_string(), server_info.druids.unwrap_or(0)),
                ("Bard".to_string(), server_info.bards.unwrap_or(0)),
                (
                    "Necromancer".to_string(),
                    server_info.necromancer.unwrap_or(0),
                ),
                ("Paladin".to_string(), server_info.paladins.unwrap_or(0)),
            ]
            .into_iter()
            .collect(),
            lvl_avg: server_info.avg_level.unwrap_or(0),
            server_id: server_info.server_id,
        };
        res.push(info);
    }

    let c: Arc<[_]> = res.into();
    *cache_res = Some((c.clone(), Instant::now()));
    drop(cache_res);
    Ok(c)
}

pub async fn get_hof_players(
    args: GetHofPlayersArgs,
) -> Result<Vec<HofPlayerInfo>, SFSError> {
    let db = get_db().await?;

    let players = sqlx::query!(
        "WITH paginated_players AS (
            SELECT player.name, guild_id, player.level, player.honor
            FROM player
            WHERE server_id = $1
              AND honor IS NOT NULL
              AND is_removed = FALSE
              ORDER BY honor DESC, player_id
            OFFSET $2
            LIMIT $3
        )
        SELECT p.name as player_name, g.name as guild_name, p.honor, p.level
        FROM paginated_players p
        LEFT JOIN guild g ON g.guild_id = p.guild_id
        ORDER BY honor DESC
    ",
        args.server_id,
        args.offset as i32,
        args.limit.clamp(1, 100) as i32
    )
    .fetch_all(&db)
    .await?;

    Ok(players
        .into_iter()
        .enumerate()
        .map(|(idx, player)| HofPlayerInfo {
            name: player.player_name,
            rank: (idx as u32 + args.offset + 1),
            honor: player.honor.map_or(0, |a| a as u32),
            level: player.level.map_or(0, |a| a as u32),
            guild: player.guild_name,
        })
        .collect())
}

pub async fn get_server_info(
    ident: String,
) -> Result<Arc<DetailedServerInfo>, SFSError> {
    let (url, cat) = ident_to_info(&ident);

    static RESULT_CACHE: CacheMap<
        String,
        Arc<RwLock<CacheEntry<DetailedServerInfo>>>,
    > = LazyLock::new(Default::default);

    let mut cache_res = RESULT_CACHE.read().await;

    todo!()
}

pub async fn get_server_levels(
    ident: String,
) -> Result<Arc<ServerLevels>, SFSError> {
    let (url, cat) = ident_to_info(&ident);

    static RESULT_CACHE: CacheMap<
        String,
        Arc<RwLock<CacheEntry<ServerLevels>>>,
    > = LazyLock::new(Default::default);

    let mut cache_res = RESULT_CACHE.read().await;

    todo!()
}

pub async fn get_class_distribution(
    ident: String,
) -> Result<Arc<ServerClassDistributions>, SFSError> {
    let (url, cat) = ident_to_info(&ident);

    static RESULT_CACHE: CacheMap<
        String,
        Arc<RwLock<CacheEntry<ServerClassDistributions>>>,
    > = LazyLock::new(Default::default);

    let mut cache_res = RESULT_CACHE.read().await;

    todo!()
}
