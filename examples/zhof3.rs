use std::io::Write;

use sf_info_lib::db::get_db;
use sqlx::{Pool, Postgres};

/// ZHOF3_01
///     -1
///     LEVEL
///         pid
///         ident_count
///             ident

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = get_db().await?;
    let server_ids = sqlx::query_scalar!(
        "SELECT server_id FROM server ORDER BY server_id ASC"
    )
    .fetch_all(&db)
    .await?;

    for server_id in server_ids.into_iter().take(1) {
        let mut data: Vec<u8> = b"ZHOF3_01".into();
        let players = sqlx::query!(
            "SELECT
                level, array_agg(player_id) as player_ids
            FROM player
            WHERE server_id = $1 AND is_removed = FALSE AND level is not null
            GROUP BY level
            ORDER BY level asc",
            server_id
        )
        .fetch_all(&db)
        .await?;
        let bar = indicatif::ProgressBar::new(
            players
                .iter()
                .filter_map(|a| a.player_ids.as_deref())
                .map(|a| a.len() as u64)
                .sum::<u64>(),
        );

        for rec in players {
            let Some(level) = rec.level else {
                continue;
            };
            let Some(ids) = rec.player_ids else {
                continue;
            };
            data.write_all((-1i8).to_le_bytes().as_ref())?;
            data.write_all((level as u16).to_le_bytes().as_ref())?;

            for pid in ids {
                bar.inc(1);
                serialize_player(pid, &mut data, &db).await?;
            }
        }

        tokio::fs::write(format!("{server_id}.zhof3"), &data).await?;
    }

    Ok(())
}

pub async fn serialize_player(
    player_id: i32,
    writer: &mut impl std::io::Write,
    db: &Pool<Postgres>,
) -> Result<(), Box<dyn std::error::Error>> {
    let pid = sqlx::query_scalar!(
        "SELECT server_player_id
        FROM player WHERE player_id = $1",
        player_id
    )
    .fetch_one(db)
    .await?;

    let Some(pid) = pid else {
        return Ok(());
    };

    let idents = sqlx::query_scalar!(
        "SELECT ident FROM equipment WHERE player_id = $1",
        player_id
    )
    .fetch_all(db)
    .await?;

    if idents.is_empty() {
        return Ok(());
    }

    writer.write_all(pid.to_le_bytes().as_ref())?;
    for ident in idents {
        writer.write_all(ident.to_le_bytes().as_ref())?;
    }

    Ok(())
}
