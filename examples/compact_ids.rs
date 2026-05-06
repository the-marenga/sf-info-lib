#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::cast_lossless,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]

use std::collections::HashMap;

use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use sqlx::{PgPool, Pool, Postgres};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("DATABASE_URL").unwrap();
    let db = PgPool::connect(&database_url).await?;

    let total_players: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM player")
        .fetch_one(&db)
        .await?
        .unwrap_or(0);

    if total_players == 0 {
        println!("No players to compact.");
        return Ok(());
    }

    println!("Compacting {total_players} player IDs.");

    let old_ids: Vec<i32> =
        sqlx::query_scalar!("SELECT player_id FROM player ORDER BY player_id")
            .fetch_all(&db)
            .await?;

    let mut id_map: HashMap<i32, i32> = HashMap::with_capacity(old_ids.len());
    let mut max_new_id: i32 = 0;
    for old_id in &old_ids {
        max_new_id += 1;
        if *old_id != max_new_id {
            id_map.insert(*old_id, max_new_id);
        }
    }

    let to_update = id_map.len();
    let skipped = total_players as usize - to_update;
    println!(
        "{skipped} players already at the correct ID (skipped). {to_update} \
         need reassignment."
    );

    if to_update == 0 {
        println!("Nothing to do — IDs are already compact!");
        let max_id: Option<i32> =
            sqlx::query_scalar!("SELECT MAX(player_id) FROM player")
                .fetch_one(&db)
                .await?;
        if let Some(max) = max_id {
            let next_val = max + 1;
            sqlx::query(&format!(
                "ALTER SEQUENCE player_player_id_seq RESTART WITH {next_val}"
            ))
            .execute(&db)
            .await?;
            println!("Sequence reset to {next_val}.");
        }
        return Ok(());
    }

    println!("Dropping FK constraint player_info -> player ...");
    sqlx::query!(
        "ALTER TABLE player_info DROP CONSTRAINT IF EXISTS \
         player_info_player_id_fkey",
    )
    .execute(&db)
    .await?;

    println!("Starting ID updates (CTRL+C to abort at any point)...");

    let pb = ProgressBar::new(total_players as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] \
                 {pos}/{len} ({eta})",
            )
            .unwrap()
            .progress_chars("#>-"),
    );

    // Update IDs that may be overwritten later — sequential, in order.
    for old_id in old_ids.iter().take_while(|&&a| a <= max_new_id) {
        if let Some(&new_id) = id_map.get(old_id) {
            move_id(*old_id, new_id, &db).await?;
        }
        pb.inc(1);
    }

    // Remaining IDs are safe (old_id > max_new_id, no collisions possible).
    // Process them in batches with limited concurrency.
    let safe_pairs: Vec<(i32, i32)> = old_ids
        .iter()
        .copied()
        .filter(|&a| a > max_new_id)
        .filter_map(|old_id| {
            id_map.get(&old_id).map(|&new_id| (old_id, new_id))
        })
        .collect();

    const BATCH_SIZE: usize = 1000;
    const MAX_CONCURRENT: usize = 8;

    futures::stream::iter(safe_pairs.chunks(BATCH_SIZE).map(|chunk| {
        let db = db.clone();
        let pb = pb.clone();
        async move {
            move_ids_batch(chunk, &db).await.unwrap();
            pb.inc(chunk.len() as u64);
        }
    }))
    .buffer_unordered(MAX_CONCURRENT)
    .for_each(|_| async {})
    .await;
    pb.finish_with_message("All player IDs updated!");

    println!("Re-adding FK constraint...");
    sqlx::query!(
        "ALTER TABLE player_info
         ADD CONSTRAINT player_info_player_id_fkey
         FOREIGN KEY (player_id) REFERENCES player (player_id)",
    )
    .execute(&db)
    .await?;
    println!("  Re-added player_info_player_id_fkey.");

    let max_id: Option<i32> =
        sqlx::query_scalar!("SELECT MAX(player_id) FROM player")
            .fetch_one(&db)
            .await?;

    if let Some(max) = max_id {
        let next_val = max + 1;
        sqlx::query(&format!(
            "ALTER SEQUENCE player_player_id_seq RESTART WITH {next_val}"
        ))
        .execute(&db)
        .await?;
        println!(
            "Sequence reset to {next_val} (next insert gets \
             player_id={next_val})."
        );
    }

    let count: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM player")
        .fetch_one(&db)
        .await?
        .unwrap_or(0);

    let max_id2: Option<i32> =
        sqlx::query_scalar!("SELECT MAX(player_id) FROM player")
            .fetch_one(&db)
            .await?;

    if let Some(max) = max_id2 {
        if max as i64 == count {
            println!(
                "✓ Verification passed: {count} players, max(player_id) = \
                 {max}. IDs are perfectly compact!"
            );
        } else {
            println!(
                "⚠ Verification: {count} players, max(player_id) = {max}. {} \
                 IDs are reusable.",
                max - count as i32
            );
        }
    }

    Ok(())
}

async fn move_ids_batch(
    batch: &[(i32, i32)],
    db: &Pool<Postgres>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut tx = db.begin().await?;

    // Build CASE expressions from the batch pairs.
    // i32 values are safe to format directly (no SQL injection risk).
    let old_ids_str: Vec<String> =
        batch.iter().map(|(o, _)| o.to_string()).collect();
    let case_whens: Vec<String> = batch
        .iter()
        .map(|(o, n)| format!("WHEN player_id = {o} THEN {n}"))
        .collect();
    let case_clause = case_whens.join(" ");
    let id_list = old_ids_str.join(", ");

    let pi_sql = format!(
        "UPDATE player_info SET player_id = CASE {case_clause} END WHERE \
         player_id IN ({id_list})"
    );
    sqlx::query(&pi_sql).execute(&mut *tx).await?;

    let p_sql = format!(
        "UPDATE player SET player_id = CASE {case_clause} END WHERE player_id \
         IN ({id_list})"
    );
    sqlx::query(&p_sql).execute(&mut *tx).await?;

    tx.commit().await?;
    Ok(())
}

async fn move_id(
    old_id: i32,
    new_id: i32,
    db: &Pool<Postgres>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut tx = db.begin().await?;

    sqlx::query!(
        "UPDATE player_info SET player_id = $1 WHERE player_id = $2",
        new_id,
        old_id,
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        "UPDATE player SET player_id = $1 WHERE player_id = $2",
        new_id,
        old_id,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(())
}
