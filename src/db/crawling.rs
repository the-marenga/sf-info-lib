use std::collections::HashSet;

use chrono::Utc;
use log::warn;
use sf_api::{misc::to_sf_string, session::Response};

use crate::{
    common::{compress_ident, days, hours, minutes},
    db::{
        CharacterInfo, get_db, get_gamestate, get_server_id,
        update::{PlayerUpdate, UPDATE_SENDER},
    },
    error::SFSError,
    types::{CrawlReport, GetCharactersArgs, GetHofArgs, ReportHofArgs},
};

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

pub async fn mark_removed(
    db: &sqlx::Pool<sqlx::Postgres>,
    server_id: i32,
    player_name: String,
) -> Result<(), SFSError> {
    if player_name.is_empty() {
        return Ok(());
    }

    let pid = sqlx::query_scalar!(
        "UPDATE player
        SET is_removed = true
        WHERE server_id = $1 AND name = $2
        RETURNING player_id",
        server_id,
        &player_name
    )
    .fetch_optional(db)
    .await?;

    if let Some(pid) = pid {
        let res = UPDATE_SENDER.send(PlayerUpdate {
            player_id: pid,
            server_id,
            items: Box::new([]),
            info: CharacterInfo { stats: 0, level: 0 },
        });
        if res.is_err() {
            log::error!("Could not remove player data");
        }
    }

    Ok(())
}

pub async fn insert_player(
    db: &sqlx::Pool<sqlx::Postgres>,
    server_id: i32,
    player_name: String,
    player_response: Response,
) -> Result<(), SFSError> {
    let Some(resp_name) = player_response.values().get("otherplayername")
    else {
        return Err(SFSError::InvalidPlayer(
            format!("Invalid response for '{player_name}': missing name key")
                .into(),
        ));
    };

    let mut gs = get_gamestate().await;
    gs.lookup.reset_lookups();
    gs.update(&player_response)
        .map_err(|e| SFSError::InvalidPlayer(e.to_string().into()))?;

    let Some(other) = gs.lookup.lookup_name(resp_name.as_str()) else {
        return Err(SFSError::InvalidPlayer(
            format!(
                "Invalid response for '{player_name}': not found in lookup"
            )
            .into(),
        ));
    };
    let other = other.clone();
    drop(gs);

    // TODO: Turn this into a UTC?
    let mut fetch_time = player_response.received_at();

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
        .attribute_basis
        .values()
        .chain(other.attribute_additions.values())
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
    if let Some(guild) = other.guild.as_ref().filter(|a| !a.is_empty()) {
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

    let pid = if let Some(existing) = existing {
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
             $12, equipment = $13
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
            other.player_id as i32,
            &equip_idents
        )
        .execute(&mut *tx)
        .await?;
        existing.player_id
    } else {
        let next_attempt = fetch_time + days(1);
        // Insert a new player and so far unseen player. This is very unlikely
        // since players should be created after HoF search
        sqlx::query_scalar!(
            "INSERT INTO player
            (server_id, name, level, attributes, next_report_attempt, \
             last_reported, last_changed, equip_count, xp, honor, guild_id, \
             class, server_player_id, equipment)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
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
            other.player_id as i32,
            &equip_idents
        )
        .fetch_one(&mut *tx)
        .await?
    };

    let res = UPDATE_SENDER.send(PlayerUpdate {
        player_id: pid,
        server_id,
        items: equip_idents.into_boxed_slice(),
        info: CharacterInfo {
            stats: attributes as u64,
            level: other.level,
        },
    });
    if res.is_err() {
        log::error!("Could not send update");
    }

    let description = to_sf_string(&other.description);

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

    let resp = reencode_response(&player_response)?;

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
                VALUES ($1, 4)
                RETURNING otherplayer_resp_id",
                &resp,
            )
            .fetch_one(&mut *tx)
            .await?
        }
    };

    sqlx::query_scalar!(
        "INSERT INTO player_info (player_id, fetch_time, level, \
         soldier_advice, description_id, guild_id, otherplayer_resp_id, \
         honor, rank)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        pid,
        fetch_time,
        other.level as i16,
        other
            .fortress
            .as_ref()
            .map_or(0, |a| a.soldier_advice as i16),
        description_id,
        guild_id,
        response_id,
        other.honor as i32,
        other.rank as i32
    )
    .execute(&mut *tx)
    .await?;

    return Ok(tx.commit().await?);
}
pub fn reencode_response(response: &Response) -> Result<Vec<u8>, SFSError> {
    use std::io::Write;
    let mut compressor = zstd::stream::Encoder::new(Vec::new(), 3)?;

    for (idx, (&key, value)) in response.values().iter().enumerate() {
        if idx > 0 {
            write!(compressor, "&")?;
        }

        write!(compressor, "{key}")?;

        if !value.sub_key().is_empty() {
            write!(compressor, ".{}", value.sub_key())?;
        }

        write!(compressor, ":")?;

        if key == "otherplayersavecharacter" {
            for (i, val) in value.as_str().split('/').enumerate() {
                if i > 0 {
                    write!(compressor, "/")?;
                }
                if (6..=7).contains(&i) {
                    write!(compressor, "0")?;
                } else {
                    write!(compressor, "{val}")?;
                }
            }
        } else {
            write!(compressor, "{}", value.as_str())?;
        }
    }

    Ok(compressor.finish()?)
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
        let mut gs = get_gamestate().await;
        gs.hall_of_fames.players.clear();

        if let Err(err) = gs.update(&info) {
            warn!("Invalid hall of fame response: {err} - {info:?}");
            continue;
        }

        let mut players = vec![];
        std::mem::swap(&mut players, &mut gs.hall_of_fames.players);
        drop(gs);

        players.retain(|player| {
            // Looking up these names will actually look up the
            // player with that id, which can lead to issues
            !player.name.chars().all(|a| a.is_ascii_digit())
        });

        let mut tx = db.begin().await?;
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

        // Filter out existing players to avoid incrementing the SERIAL sequence
        let player_names: Vec<String> =
            players.iter().map(|p| p.name.clone()).collect();
        let existing_names: HashSet<String> = sqlx::query_scalar!(
            "SELECT name FROM player WHERE server_id = $1 AND name = ANY($2)",
            server_id,
            &player_names
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();

        players.retain(|p| !existing_names.contains(&p.name));

        if !players.is_empty() {
            let mut b = sqlx::QueryBuilder::new(
                "INSERT INTO player (server_id, name, level) ",
            );
            b.push_values(players, |mut b, player| {
                b.push_bind(server_id)
                    .push_bind(player.name)
                    .push_bind(player.level as i32);
            });
            b.push(" ON CONFLICT DO NOTHING");
            b.build().execute(&mut *tx).await?;
        }
        tx.commit().await?;
    }
    Ok(())
}
