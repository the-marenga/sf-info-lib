#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::cast_lossless,
    clippy::doc_markdown,
    clippy::uninlined_format_args
)]

use std::collections::HashMap;

use clap::Parser;
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use sqlx::PgPool;

#[derive(Parser)]
#[command(name = "compact_ids")]
struct Args {
    /// Number of IDs to update in each batch
    #[arg(long = "chunk-size", default_value = "100")]
    chunk_size: usize,

    /// Maximum number of batches to process concurrently
    #[arg(long = "buffer-size", default_value = "10")]
    buffer_size: usize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

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
    let existing: std::collections::HashSet<i32> =
        old_ids.iter().copied().collect();

    // Available slots in [1, n] that are not occupied
    let available: Vec<i32> =
        (1..=n).filter(|id| !existing.contains(id)).collect();

    // IDs > n that need to be moved into available slots (descending order)
    let mut to_move: Vec<i32> =
        old_ids.iter().copied().filter(|id| *id > n).collect();
    to_move.sort_unstable_by(|a, b| b.cmp(a));

    let mut id_map: HashMap<i32, i32> = HashMap::with_capacity(to_move.len());
    for (i, old_id) in to_move.iter().enumerate() {
        id_map.insert(*old_id, available[i]);
    }

    let to_update = id_map.len();
    let skipped = total_players as usize - to_update;
    println!(
        "{skipped} players already within range (skipped). {to_update} need \
         reassignment."
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

    println!(
        "Starting ID updates (chunk-size={}, buffer-size={}, CTRL+C to abort \
         at any point)...",
        args.chunk_size, args.buffer_size
    );

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

    // Collect all pairs and process in concurrent batches
    let all_pairs: Vec<(i32, i32)> =
        id_map.iter().map(|(&o, &n)| (o, n)).collect();

    pb.inc(skipped as u64);

    let chunks: Vec<&[(i32, i32)]> =
        all_pairs.chunks(args.chunk_size).collect();

    let results = futures::stream::iter(chunks)
        .map(|chunk| async {
            let mut tx = db.begin().await?;

            let values: Vec<String> = chunk
                .iter()
                .map(|(old_id, new_id)| format!("({old_id}, {new_id})"))
                .collect();
            let values_str = values.join(", ");

            // Update player_info table first (FK is dropped so order doesn't
            // matter)
            let info_sql = format!(
                "WITH v(old_id, new_id) AS (VALUES {values_str}) UPDATE \
                 player_info SET player_id = v.new_id FROM v WHERE \
                 player_info.player_id = v.old_id"
            );
            sqlx::query(&info_sql).execute(&mut *tx).await?;

            // Update player table
            let player_sql = format!(
                "WITH v(old_id, new_id) AS (VALUES {values_str}) UPDATE \
                 player SET player_id = v.new_id FROM v WHERE \
                 player.player_id = v.old_id"
            );
            sqlx::query(&player_sql).execute(&mut *tx).await?;

            tx.commit().await?;
            pb.inc(chunk.len() as u64);
            Ok::<_, Box<dyn std::error::Error>>(())
        })
        .buffer_unordered(args.buffer_size)
        .collect::<Vec<Result<(), Box<dyn std::error::Error>>>>()
        .await;

    for result in results {
        result?;
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
