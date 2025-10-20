use futures::StreamExt;
use sf_info_lib::db::get_db;

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = get_db().await?;

    let player_ids: Vec<i32> =
        sqlx::query_scalar!("SELECT player_id FROM player")
            .fetch_all(&db)
            .await?;
    let bar = indicatif::ProgressBar::new(player_ids.len() as u64);

    futures::stream::iter(player_ids)
        .for_each_concurrent(20, |player_id| {
            let db = db.clone();
            let bar = bar.clone();
            async move {
                bar.inc(1);
                let equipment: Vec<i32> = sqlx::query_scalar!(
                    "SELECT ident FROM equipment WHERE player_id = $1",
                    player_id
                )
                .fetch_all(&db)
                .await
                .unwrap();

                sqlx::query!(
                    "UPDATE player SET equipment = $1 WHERE player_id = $2",
                    &equipment,
                    player_id,
                )
                .execute(&db)
                .await
                .unwrap();
            }
        })
        .await;

    Ok(())
}
