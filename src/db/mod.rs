use std::{
    collections::HashMap,
    fmt::Write,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use sf_api::gamestate::{
    ServerTime,
    items::Equipment,
    social::{HallOfFamePlayer, OtherPlayer},
    unlockables::ScrapBook,
};
use sqlx::{Pool, Postgres, QueryBuilder, postgres::PgPoolOptions};
use tokio::sync::RwLock;
use zstd::stream::encode_all;

use crate::{
    common::{compress_ident, days, hours, ident_to_info, minutes},
    error::SFSError,
    types::*,
};

/// Gets the connection pool, that is going to be shared accross invocations
pub async fn get_db() -> Result<Pool<Postgres>, SFSError> {
    static DB: async_once_cell::OnceCell<sqlx::Pool<sqlx::Postgres>> =
        async_once_cell::OnceCell::new();

    let get_options = || {
        PgPoolOptions::new()
            .max_connections(500)
            .max_lifetime(Some(Duration::from_secs(60 * 3)))
            .min_connections(10)
            .acquire_timeout(Duration::from_secs(100))
    };

    Ok(DB
        .get_or_try_init(get_options().connect(env!("DATABASE_URL")))
        .await?
        .to_owned())
}

static LOOKUP_CACHE: LazyLock<RwLock<HashMap<String, i32>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub async fn get_server_id(
    db: &Pool<Postgres>,
    mut url: String,
) -> Result<i32, SFSError> {
    if !url.starts_with("http") {
        url = format!("https://{url}");
    }
    let Ok(mut server) = url::Url::parse(&url) else {
        log::error!("Could not parse url: {url}");
        return Err(SFSError::InvalidServer);
    };
    if server.set_scheme("https").is_err() {
        log::error!("Could not set scheme: {server}");
        return Err(SFSError::InvalidServer);
    }
    server.set_path("");
    let url = server.to_string();

    if let Some(id) = LOOKUP_CACHE.read().await.get(&url) {
        return Ok(*id);
    }

    let mut cache = LOOKUP_CACHE.write().await;

    if let Some(id) = cache.get(&url) {
        return Ok(*id);
    }
    let time = (Utc::now() - days(30)).naive_utc();
    let server_id = sqlx::query_scalar!(
        "INSERT INTO server (url, last_hof_crawl)
        VALUES ($1, $2)
        ON CONFLICT(url) DO UPDATE SET last_hof_crawl = server.last_hof_crawl
        RETURNING server_id",
        url,
        time
    )
    .fetch_one(db)
    .await?;

    log::info!("Fed server cache with {url}");
    cache.insert(url.to_string(), server_id);
    Ok(server_id)
}

/// Inserts a new bug report from the mfbot into the db
pub async fn insert_bug(args: BugReportArgs) -> Result<(), SFSError> {
    let current_time = Utc::now().naive_utc();
    sqlx::query!(
        "INSERT INTO error (stacktrace, version, additional_info, os, arch, \
         error_text, hwid, timestamp) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        args.stacktrace,
        args.version,
        args.additional_info,
        args.os,
        args.arch,
        args.error_text,
        args.hwid,
        current_time
    )
    .execute(&get_db().await?)
    .await?;

    Ok(())
}

struct CacheEntry<T> {
    result: T,
    insertion_time: DateTime<Utc>,
}

type CacheMap<K, V> = LazyLock<RwLock<HashMap<K, V>>>;

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
        if let Some(entry) = k.get(&args) {
            if entry.insertion_time + hours(1) > Utc::now() {
                log::info!("Skipped sb advice using cache");
                return Ok(entry.result.clone());
            }
        }
    }

    let sb = ScrapBook::parse(&args.raw_scrapbook)
        .ok_or(SFSError::InvalidScrapbook)?;
    let collected: Vec<i32> =
        sb.items.into_iter().map(compress_ident).collect();

    log::info!("New sb query: {}", collected.len());
    let db = get_db().await?;
    let server_id = get_server_id(&db, args.server.clone()).await?;

    let good_amount = match collected.len() {
        ..200 => 9,
        200..500 => 8,
        500..1000 => 7,
        1000..1300 => 6,
        1300..1400 => 5,
        1400..1600 => 4,
        1600..1682 => 3,
        _ => return Ok(Arc::default()),
    };

    let attempts = [10_000, 100_000, 1_000_000, 100_000_000];
    let now = Instant::now();
    for (attempt, limit) in attempts.into_iter().enumerate() {
        let result = sqlx::query!(
            "
            WITH filtered_equipment AS (
            SELECT player_id, attributes
            FROM equipment
            WHERE server_id = $1
                AND ident != ALL($2::integer[])
                AND attributes <= $3
            order by ATTRIBUTES
            LIMIT $4
            )
            SELECT
            player_id, count(*) AS new_count, attributes
            FROM filtered_equipment
            GROUP BY player_id, attributes
            ORDER BY new_count DESC, attributes
            LIMIT 20",
            server_id,
            collected.as_slice(),
            args.max_attrs as i64,
            limit
        )
        .fetch_all(&db)
        .await?;

        if attempt + 1 == attempts.len()
            || result
                .first()
                .is_some_and(|a| a.new_count.unwrap_or(0) >= good_amount)
        {
            if result.is_empty() {
                let now = Utc::now();
                let mut k = RESULT_CACHE.write().await;
                k.insert(
                    args,
                    CacheEntry {
                        result: Arc::default(),
                        insertion_time: now,
                    },
                );
                return Ok(Arc::default());
            }
            let ids: Vec<_> = result.iter().map(|a| a.player_id).collect();

            let mut names: HashMap<i32, String> = sqlx::query!(
                "SELECT name, player_id FROM player WHERE player_id = \
                 ANY($1::integer[])",
                &ids
            )
            .fetch_all(&db)
            .await?
            .into_iter()
            .map(|a| (a.player_id, a.name))
            .collect();

            log::info!(
                "Request for sb {} took {:?} and {attempt} attempts => {}",
                collected.len(),
                now.elapsed(),
                result.first().and_then(|a| a.new_count).unwrap_or(-1)
            );

            let res: Arc<_> = result
                .into_iter()
                .filter_map(|a| {
                    Some(ScrapBookAdvice {
                        player_name: names.remove(&a.player_id)?,
                        new_count: a.new_count? as u32,
                    })
                })
                .collect();

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

            return Ok(res);
        }
    }

    Err(SFSError::Internal("Did not fetch any scrapbook results"))
}

pub async fn mark_removed(
    db: &sqlx::Pool<sqlx::Postgres>,
    server_id: i32,
    player_name: String,
) -> Result<(), SFSError> {
    if player_name.is_empty() {
        return Ok(());
    }

    sqlx::query!(
        "UPDATE player
        SET is_removed = true
        WHERE server_id = $1 AND name = $2",
        server_id,
        &player_name
    )
    .execute(db)
    .await?;

    Ok(())
}

pub async fn insert_player(
    db: &sqlx::Pool<sqlx::Postgres>,
    server_id: i32,
    player_name: String,
    player: RawOtherPlayer,
) -> Result<(), SFSError> {
    let data: Result<Vec<i64>, _> = player
        .info
        .trim()
        .split('/')
        .map(|a| a.trim().parse())
        .collect();

    let data = match data {
        Ok(data) => data,
        Err(e) => {
            return Err(SFSError::InvalidPlayer(
                format!("Could not parse player data for {player_name}: {e}")
                    .into(),
            ));
        }
    };
    let mut other = match OtherPlayer::parse(&data, ServerTime::default()) {
        Ok(other) => other,
        Err(e) => {
            return Err(SFSError::InvalidPlayer(
                format!("Could not parse other player {player_name}: {e}")
                    .into(),
            ));
        }
    };
    let equip_data: Result<Vec<i64>, _> = player
        .info
        .trim()
        .split('/')
        .map(|a| a.trim().parse())
        .collect();

    let equip_data = match equip_data {
        Ok(data) => data,
        Err(e) => {
            return Err(SFSError::InvalidPlayer(
                format!("Could not parse player data for {player_name}: {e}")
                    .into(),
            ));
        }
    };
    let other_equipment =
        match Equipment::parse(&equip_data, ServerTime::default()) {
            Ok(other) => other,
            Err(e) => {
                return Err(SFSError::InvalidPlayer(
                    format!("Could not parse other player {player_name}: {e}")
                        .into(),
                ));
            }
        };
    other.equipment = other_equipment;

    let Ok(mut fetch_time) = DateTime::parse_from_rfc3339(&player.fetch_date)
        .map(|a| a.to_utc().naive_utc())
    else {
        return Err(SFSError::InvalidPlayer(
            format!("Could not parse fetch date: {}", player.fetch_date).into(),
        ));
    };
    let now = Utc::now().naive_utc();
    if fetch_time > now {
        fetch_time = now;
    }

    let experience = other.experience as i64;

    let mut equip_idents: Vec<_> = other
        .equipment
        .0
        .values()
        .flatten()
        .filter(|a| a.model_id < 100)
        .filter_map(|item| item.equipment_ident().map(compress_ident))
        .collect();

    // Assassins may have two swords, which can be identical
    equip_idents.sort_unstable();
    equip_idents.dedup();

    let equip_count = other.equipment.0.values().flatten().count() as i32;

    let attributes = other
        .base_attributes
        .values()
        .chain(other.bonus_attributes.values())
        .copied()
        .map(i64::from)
        .sum::<i64>();

    let mut tx = db.begin().await?;

    let existing = sqlx::query!(
        "SELECT player_id, level, attributes, last_reported, xp, last_changed
         FROM player
         WHERE server_id = $1 AND name = $2",
        server_id,
        player_name
    )
    .fetch_optional(&mut *tx)
    .await?;

    let mut guild_id = None;
    if let Some(guild) = &player.guild.filter(|a| !a.is_empty()) {
        let guild_name = guild;

        let mut id = sqlx::query_scalar!(
            "SELECT guild_id
            FROM guild
            WHERE server_id = $1 AND name = $2",
            server_id,
            guild_name,
        )
        .fetch_optional(&mut *tx)
        .await?;

        if id.is_none() {
            id = Some(
                sqlx::query_scalar!(
                    "INSERT INTO guild
                (server_id, name)
                VALUES ($1, $2)
                ON CONFLICT(server_id, name) DO UPDATE SET is_removed = FALSE
                RETURNING guild_id",
                    server_id,
                    guild_name,
                )
                .fetch_one(&mut *tx)
                .await?,
            );
        }

        guild_id = id;
    }

    let (pid, has_changed) = if let Some(existing) = existing {
        if existing.last_reported.is_some_and(|a| a >= fetch_time) {
            log::warn!("Discarded player update for {player_name}");
            return Ok(());
        }
        let has_changed = existing.attributes.is_none_or(|a| a != attributes)
            || existing.xp.is_none_or(|a| a != experience)
            || existing.level.is_none_or(|a| a != i32::from(other.level));

        let next_attempt = if has_changed {
            fetch_time
                + hours(fastrand::u64(11..14))
                + minutes(fastrand::u64(0..=59))
        } else {
            match existing.last_changed {
                Some(x) if x + days(3) > fetch_time => {
                    fetch_time
                        + days(1)
                        + hours(fastrand::u64(0..12))
                        + minutes(fastrand::u64(0..=59))
                }
                Some(x) if x + days(7) > fetch_time => {
                    fetch_time
                        + days(fastrand::u64(2..=4))
                        + hours(fastrand::u64(0..23))
                        + minutes(fastrand::u64(0..=59))
                }
                _ => {
                    fetch_time
                        + days(fastrand::u64(10..=14))
                        + hours(fastrand::u64(0..=23))
                        + minutes(fastrand::u64(0..=59))
                }
            }
        };

        let last_changed = existing
            .last_changed
            .filter(|_| !has_changed)
            .unwrap_or(fetch_time);

        // Update the player with new info
        sqlx::query!(
            "UPDATE player
            SET level = $1, attributes = $2, next_report_attempt = $3,
                last_reported = $4, last_changed = $5, equip_count = $6, xp = \
             $7, honor = $8, guild_id = $10, class = $11, server_player_id = \
             $12
            WHERE player_id = $9",
            i32::from(other.level),
            attributes,
            next_attempt,
            fetch_time,
            last_changed,
            equip_count as i32,
            experience,
            other.honor as i32,
            existing.player_id,
            guild_id,
            other.class as i16,
            other.player_id as i32
        )
        .execute(&mut *tx)
        .await?;
        (existing.player_id, has_changed)
    } else {
        let next_attempt = fetch_time + days(1);
        // Insert a new player and so far unseen player. This is very unlikely
        // since players should be created after HoF search
        let pid = sqlx::query_scalar!(
            "INSERT INTO player
            (server_id, name, level, attributes, next_report_attempt, \
             last_reported, last_changed, equip_count, xp, honor, guild_id, \
             class, server_player_id)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
            RETURNING player_id",
            server_id,
            player_name,
            i32::from(other.level),
            attributes,
            next_attempt,
            fetch_time,
            fetch_time,
            equip_count as i16,
            experience,
            other.honor as i32,
            guild_id,
            other.class as i16,
            other.player_id as i32
        )
        .fetch_one(&mut *tx)
        .await?;
        (pid, true)
    };

    let description = player.description.unwrap_or_default();

    let description_id = sqlx::query_scalar!(
        "SELECT description_id
        FROM description
        WHERE description = $1",
        description,
    )
    .fetch_optional(&mut *tx)
    .await?;

    let description_id = match description_id {
        Some(d) => d,
        None => {
            sqlx::query_scalar!(
                "INSERT INTO description (description) VALUES ($1)
            ON CONFLICT(description)
            DO UPDATE SET description_id = description.description_id
            RETURNING description_id",
                description,
            )
            .fetch_one(&mut *tx)
            .await?
        }
    };

    let new_raw_resp = reencode_response(&data, &player.raw_equipment)?;
    let resp = encode_all(new_raw_resp.as_bytes(), 3)
        .map_err(|_| SFSError::Internal("Could not zstd compress response"))?;

    let response_id = sqlx::query_scalar!(
        "SELECT otherplayer_resp_id FROM otherplayer_resp WHERE \
         otherplayer_resp = $1",
        &resp
    )
    .fetch_optional(&mut *tx)
    .await?;

    let response_id = match response_id {
        Some(id) => id,
        None => {
            sqlx::query_scalar!(
                "INSERT INTO otherplayer_resp (otherplayer_resp, version)
                VALUES ($1, 2)
                RETURNING otherplayer_resp_id",
                &resp,
            )
            .fetch_one(&mut *tx)
            .await?
        }
    };

    sqlx::query_scalar!(
        "INSERT INTO player_info (player_id, fetch_time, xp, level, \
         soldier_advice, description_id, guild_id, otherplayer_resp_id, \
         honor, rank)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9, $10)",
        pid,
        fetch_time,
        experience,
        i32::from(other.level),
        player.soldier_advice,
        description_id,
        guild_id,
        response_id,
        other.honor as i32,
        other.rank as i32
    )
    .execute(&mut *tx)
    .await?;

    if !has_changed {
        return Ok(tx.commit().await?);
    }

    sqlx::query!("DELETE FROM equipment WHERE player_id = $1", pid)
        .execute(&mut *tx)
        .await?;
    if equip_idents.is_empty() {
        return Ok(tx.commit().await?);
    }

    let mut builder = QueryBuilder::new(
        "INSERT INTO equipment (server_id, player_id, ident, attributes) ",
    );
    builder.push_values(equip_idents, |mut b, ident| {
        b.push_bind(server_id)
            .push_bind(pid)
            .push_bind(ident)
            .push_bind(attributes as i32);
    });
    builder.build().execute(&mut *tx).await?;
    return Ok(tx.commit().await?);
}

static STORED_SPLIT_CHAR: char = '🎆';

pub fn reencode_response(
    data: &[i64],
    equip_data: &str,
) -> Result<String, SFSError> {
    let mut new_raw_resp = String::new();
    for (pos, num) in data.iter().enumerate() {
        if !new_raw_resp.is_empty() {
            new_raw_resp.push('/');
        }
        if pos == 6 {
            // Remove rank, since that changes for
            new_raw_resp.push('0');
        } else {
            new_raw_resp
                .write_fmt(format_args!("{num}"))
                .map_err(|_| SFSError::Internal("Reencode"))?;
        }
    }
    new_raw_resp.push(STORED_SPLIT_CHAR);
    new_raw_resp.push_str(equip_data);
    Ok(new_raw_resp)
}

pub async fn get_characters_to_crawl(
    args: GetCharactersArgs,
) -> Result<Vec<String>, SFSError> {
    let db = get_db().await?;
    let server_id = get_server_id(&db, args.server).await?;

    let now = Utc::now().naive_utc();
    let next_retry = now + minutes(30);

    let limit = i64::from(args.limit.min(500));

    let todo = sqlx::query_scalar!(
        "WITH cte AS (
          SELECT player_id
          FROM player
          WHERE server_id = $1
            AND next_report_attempt < $2
            AND is_removed = false
          -- This forced an index to be used in my testing. Could also make
          -- things worse, will have to test
          ORDER BY next_report_attempt
          LIMIT $3
          FOR NO KEY UPDATE)
        UPDATE player
        SET next_report_attempt = $4
        WHERE player_id IN (SELECT player_id FROM cte)
        RETURNING name",
        server_id,
        now,
        limit,
        next_retry
    )
    .fetch_all(&db)
    .await?;

    Ok(todo)
}

pub async fn get_hof_pages_to_crawl(
    args: GetHofArgs,
) -> Result<Vec<i32>, SFSError> {
    let db = get_db().await?;
    let server_id = get_server_id(&db, args.server).await?;

    let mut tx = db.begin().await?;

    let now = Utc::now().naive_utc();
    let latest_accepted_crawling_start = now - days(3);

    let last_hof_crawl = sqlx::query_scalar!(
        "WITH cte AS (
          SELECT server_id
          FROM server
          WHERE server_id = $1 AND last_hof_crawl < $2
        )
        UPDATE server
        SET last_hof_crawl = $3
        WHERE server_id IN (SELECT server_id FROM cte)
        RETURNING server_id",
        server_id,
        latest_accepted_crawling_start,
        now
    )
    .fetch_optional(&mut *tx)
    .await?;

    if last_hof_crawl.is_some() {
        // We restart HoF crawling
        sqlx::query!(
            "DELETE FROM todo_hof_page WHERE server_id = $1",
            server_id
        )
        .execute(&mut *tx)
        .await?;

        let total_pages = (args.player_count as f32 / 51.0) as i32;

        sqlx::query!(
            "WITH RECURSIVE cnt(x) AS (
              SELECT 0
              UNION ALL
              SELECT x + 1 FROM cnt WHERE x < $1
            )
            INSERT INTO todo_hof_page (server_id, idx)
            SELECT $2, x FROM cnt;
        ",
            total_pages,
            server_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    let limit = i64::from(args.limit.min(100));
    let next_attempt_at = now + minutes(15);

    let pages_to_crawl = sqlx::query_scalar!(
        "WITH cte AS (
          SELECT idx
          FROM todo_hof_page
          WHERE server_id = $1 AND next_report_attempt < $2
          LIMIT $3
        )
        UPDATE todo_hof_page
        SET next_report_attempt = $4
        WHERE server_id = $1 AND idx IN (SELECT idx FROM cte)
        RETURNING idx",
        server_id,
        now,
        limit,
        next_attempt_at
    )
    .fetch_all(&db)
    .await?;

    Ok(pages_to_crawl)
}

pub async fn insert_hof_pages(args: ReportHofArgs) -> Result<(), SFSError> {
    let db = get_db().await?;
    let server_id = get_server_id(&db, args.server).await?;

    for (page, info) in args.pages {
        let mut tx = db.begin().await?;
        let mut players = vec![];
        for player in info.as_str().trim_matches(';').split(';') {
            // Stop parsing once we receive an empty player
            if player.ends_with(",,,0,0,0,") {
                break;
            }
            match HallOfFamePlayer::parse(player) {
                Ok(x) => {
                    if x.name.chars().all(|a| a.is_ascii_digit()) {
                        // Looking up these names will actually look up the
                        // player with that id, which can lead to issues
                        continue;
                    }
                    players.push(x);
                }
                Err(err) => log::warn!("{err}"),
            }
        }

        sqlx::query!(
            "DELETE FROM todo_hof_page
            WHERE server_id = $1 AND idx = $2",
            server_id,
            page as i32
        )
        .execute(&mut *tx)
        .await?;

        if players.is_empty() {
            tx.commit().await?;
            continue;
        }

        let mut b =
            QueryBuilder::new("INSERT INTO player (server_id, name, level) ");
        b.push_values(players, |mut b, player| {
            b.push_bind(server_id)
                .push_bind(player.name)
                .push_bind(player.level as i32);
        });
        b.push(" ON CONFLICT DO NOTHING");
        b.build().execute(&mut *tx).await?;
        tx.commit().await?;
    }
    Ok(())
}

// TODO: nude players
// SELECT name, level, ATTRIBUTES
// FROM player
// where equip_count < 3 AND is_removed = false and server_id = 1 and ATTRIBUTES
// < 9000 and attributes is not null ORDER BY LEVEL desc
// LIMIT 50;

pub async fn get_hof_player(
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

pub async fn handle_crawl_report(report: CrawlReport) -> Result<(), SFSError> {
    let db = get_db().await?;
    for (server, players) in report.0 {
        let Ok(server_id) = get_server_id(&db, server.clone()).await else {
            log::error!("Could not get server_id for '{server}'");
            continue;
        };
        for (name, info) in players.0 {
            if let Some(info) = info {
                let res =
                    insert_player(&db, server_id, name.clone(), info).await;
                if let Err(e) = res {
                    log::error!("Could not update {name}@{server}: {e}");
                }
            } else {
                let res = mark_removed(&db, server_id, name.clone()).await;
                if let Err(e) = res {
                    log::error!(
                        "Could not mark {name}@{server} as removed: {e}"
                    );
                }
            }
        }
    }
    Ok(())
}

pub async fn get_servers() -> Result<Arc<[ServerInfo]>, SFSError> {
    static RESULT_CACHE: LazyLock<
        tokio::sync::Mutex<Option<(Arc<[ServerInfo]>, Instant)>>,
    > = LazyLock::new(Default::default);

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

    let mut res: Vec<ServerInfo> = Default::default();

    for server_info in info {
        let Some(url) = server_ids.remove(&server_info.server_id) else {
            continue;
        };

        let Some((id, tld)) = url
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .split_once(".sfgame.")
        else {
            continue;
        };

        let shorthand = match tld {
            "eu" => format!("eu{}", id.trim_start_matches('s')),
            _ => id.to_string(),
        };

        let category = match tld {
            "eu" => ServerCategory::Europe,
            "net" if id.starts_with("am") => ServerCategory::America,
            "net" if id.starts_with('w') => ServerCategory::International,
            _ => ServerCategory::Fused,
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

// WITH bad_guilds AS (
//     SELECT guild_id FROM guild where name = ''
// )
// UPDATE player_info
// SET guild_id = NULL WHERE guild_id in (SELECT guild_id FROM bad_guilds);

// UPDATE player
// SET guild_id = (
//     SELECT pi.guild_id
//     FROM player_info pi
//     WHERE pi.player_id = player.player_id
//     ORDER BY pi.fetch_time DESC
//     LIMIT 1
// );

pub async fn get_server_info(
    ident: String,
) -> Result<Arc<DetailedServerInfo>, SFSError> {
    let (url, cat) = ident_to_info(&ident);

    static RESULT_CACHE: LazyLock<
        tokio::sync::Mutex<Option<(Arc<DetailedServerInfo>, Instant)>>,
    > = LazyLock::new(Default::default);

    let mut cache_res = RESULT_CACHE.lock().await;

    if let Some(res) = &*cache_res
        && res.1.elapsed() < hours(6)
    {
        return Ok(res.0.clone());
    }

    let db = get_db().await?;

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

    todo!()
}
