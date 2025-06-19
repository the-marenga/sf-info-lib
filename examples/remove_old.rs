use futures::StreamExt;
use sf_info_lib::db::get_db;

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = get_db().await?;
    let ids = sqlx::query_scalar!(
        "SELECT otherplayer_resp_id
        FROM otherplayer_resp
        LEFT JOIN player_info USING (otherplayer_resp_id)
        WHERE player_info.player_id IS NULL"
    )
    .fetch_all(&db)
    .await?;

    let tasks = ids.into_iter().map(|id| {
        let db = db.clone();
        async move {
            sqlx::query_scalar!(
                "DELETE FROM otherplayer_resp
                WHERE otherplayer_resp_id = $1",
                id
            )
            .fetch_all(&db)
            .await
            .unwrap()
        }
    });

    futures::stream::iter(tasks)
        .buffer_unordered(50)
        .collect::<Vec<_>>()
        .await;

    Ok(())
}
