use std::io::Cursor;

use futures::StreamExt;
use sf_info_lib::db::{get_db, reencode_response};
use zstd::{decode_all, encode_all};

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = get_db().await?;

    let ids = sqlx::query!(
        "SELECT otherplayer_resp_id, player_info_id, player_id
        FROM player_info
        NATURAL JOIN otherplayer_resp"
    )
    .fetch_all(&db)
    .await?;
    let bar = indicatif::ProgressBar::new(ids.len() as u64);

    let tasks = ids.into_iter().map(|id| {
        let db = db.clone();
        let bar = bar.clone();
        async move {
            bar.inc(1);
            let mut tx = db.begin().await.unwrap();
            let data = sqlx::query!(
                "SELECT otherplayer_resp FROM otherplayer_resp WHERE \
                 otherplayer_resp_id = $1",
                id.otherplayer_resp_id
            )
            .fetch_one(&mut *tx)
            .await
            .unwrap();

            let decoded_resp =
                decode_all(Cursor::new(data.otherplayer_resp)).unwrap();
            let raw_response = String::from_utf8(decoded_resp).unwrap();
            let data2: Result<Vec<i64>, _> =
                raw_response.split('/').map(|a| a.trim().parse()).collect();
            let int_data = data2.unwrap();
            // Set rank to 0, since the rank can change even if the rest stays
            // the same, removing most of the dedup benefits. We
            // could also reset playerid to 0, but the chance that two chars
            // are identical is basically zero
            let pid = int_data[0];
            let rank = int_data[6];
            let class = int_data[20] - 1;

            if rank == 0 {
                return;
            }

            let new_raw_resp = reencode_response(&int_data).unwrap();
            let encoded = encode_all(new_raw_resp.as_bytes(), 3).unwrap();

            let new_response_id = sqlx::query_scalar!(
                "SELECT otherplayer_resp_id FROM otherplayer_resp WHERE \
                 otherplayer_resp = $1",
                &encoded
            )
            .fetch_optional(&mut *tx)
            .await
            .unwrap();

            let new_response_id = match new_response_id {
                Some(id) => id,
                None => sqlx::query_scalar!(
                    "INSERT INTO otherplayer_resp (otherplayer_resp)
                    VALUES ($1)
                    RETURNING otherplayer_resp_id",
                    &encoded,
                )
                .fetch_one(&mut *tx)
                .await
                .unwrap(),
            };

            sqlx::query!(
                "UPDATE player_info SET
                    otherplayer_resp_id = $1,
                    rank = $2
                WHERE player_info_id = $3",
                new_response_id,
                rank as i32,
                id.player_info_id
            )
            .execute(&mut *tx)
            .await
            .unwrap();

            sqlx::query!(
                "UPDATE player SET
                    class = $1,
                    server_player_id = $2
                WHERE player_id = $3",
                class as i32,
                pid as i32,
                id.player_id
            )
            .execute(&mut *tx)
            .await
            .unwrap();

            tx.commit().await.unwrap();
        }
    });

    futures::stream::iter(tasks)
        .buffer_unordered(10)
        .collect::<Vec<_>>()
        .await;

    Ok(())
}
