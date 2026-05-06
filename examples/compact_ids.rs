#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::cast_lossless,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]

use std::collections::HashMap;

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

    let n = total_players as i32;
    let existing: std::collections::HashSet<i32> = old_ids.iter().copied().collect();

    // Available slots in [1, n] that are not occupied
    let available: Vec<i32> = (1..=n).filter(|id| !existing.contains(id)).collect();

    // IDs > n that need to be moved into available slots (descending order)
    let mut to_move: Vec<i32> = old_ids.iter().copied().filter(|id| *id > n).collect();
    to_move.sort_unstable_by(|a, b| b.cmp(a));

    let mut id_map: HashMap<i32, i32> = HashMap::with_capacity(to_move.len());
    for (i, old_id) in to_move.iter().enumerate() {
        id_map.insert(*old_id, available[i]);
    }

    let to_update = id_map.len();
    let skipped = total_players as usize - to_update;
    println!(
        "{skipped} players already within range (skipped). {to_update} \
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

    // Collect all pairs into a single flat list and process them sequentially.
    // No parallelism — avoids concurrency safety issues entirely.
    let all_pairs: Vec<(i32, i32)> = id_map.iter().map(|(&o, &n)| (o, n)).collect();

    pb.inc(skipped as u64);

    for (old_id, new_id) in &all_pairs {
        move_id(*old_id, *new_id, &db).await?;
        pb.inc(1);
    }

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

async fn move_id(
    old_id: i32,
    new_id: i32,
    db: &Pool<Postgres>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut tx = db.begin().await?;

    sqlx::query("UPDATE player_info SET player_id = $1 WHERE player_id = $2")
        .bind(new_id)
        .bind(old_id)
        .execute(&mut *tx)
        .await?;

    sqlx::query("UPDATE player SET player_id = $1 WHERE player_id = $2")
        .bind(new_id)
        .bind(old_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}

