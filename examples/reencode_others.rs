use futures::StreamExt;
use sf_info_lib::db::{crawling::*, get_db};
use tokio::task;
use zstd::{decode_all, encode_all};

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = get_db().await?;

    sqlx::query!(
        "CREATE INDEX IF NOT EXISTS player_info_otherplayer_resp_id_idx
        ON player_info (otherplayer_resp_id)"
    )
    .execute(&db)
    .await?;

    let resp_ids: Vec<i32> = sqlx::query_scalar!(
        "SELECT otherplayer_resp_id FROM otherplayer_resp WHERE version < 3"
    )
    .fetch_all(&db)
    .await?;

    let bar = indicatif::ProgressBar::new(resp_ids.len() as u64);

    futures::stream::iter(resp_ids)
        .for_each_concurrent(8, |old_id| {
            let db = db.clone();
            let bar = bar.clone();
            async move {
                bar.inc(1);
                let result: Result<(), Box<dyn std::error::Error>> = async {
                    let response = sqlx::query_scalar!(
                        "SELECT otherplayer_resp
                        FROM otherplayer_resp
                        WHERE otherplayer_resp_id = $1 AND version < 3",
                        old_id
                    )
                    .fetch_optional(&db)
                    .await?;

                    let Some(response) = response else {
                        return Ok(());
                    };

                    let decoded = task::spawn_blocking(move || {
                        decode_all(response.as_slice())
                    })
                    .await??;
                    let decoded = String::from_utf8(decoded)?;
                    let (otherplayer, equipment) =
                        match decoded.split_once(STORED_SPLIT_CHAR) {
                            Some(r) => r,
                            None => (decoded.as_str(), ""),
                        };
                    let data: Result<Vec<i64>, _> = otherplayer
                        .trim()
                        .split('/')
                        .map(|a| a.trim().parse())
                        .collect();
                    let data = data?;

                    let new_raw_resp = reencode_response(&data, equipment)?;
                    let resp = task::spawn_blocking(move || {
                        encode_all(new_raw_resp.as_bytes(), 3)
                    })
                    .await??;

                    let mut tx = db.begin().await?;
                    let response_id = sqlx::query_scalar!(
                        "SELECT otherplayer_resp_id FROM otherplayer_resp \
                         WHERE otherplayer_resp = $1",
                        &resp
                    )
                    .fetch_optional(&mut *tx)
                    .await?;

                    let new_id = match response_id {
                        Some(id) => id,
                        None => {
                            sqlx::query_scalar!(
                                "INSERT INTO otherplayer_resp
                                (otherplayer_resp, version)
                            VALUES ($1, 3)
                            RETURNING otherplayer_resp_id",
                                &resp,
                            )
                            .fetch_one(&mut *tx)
                            .await?
                        }
                    };

                    if new_id == old_id {
                        return Ok(());
                    }

                    sqlx::query!(
                        "UPDATE player_info
                        SET otherplayer_resp_id = $1
                        WHERE otherplayer_resp_id = $2",
                        new_id,
                        old_id
                    )
                    .execute(&mut *tx)
                    .await?;

                    sqlx::query!(
                        "DELETE FROM otherplayer_resp
                        WHERE otherplayer_resp_id = $1",
                        old_id
                    )
                    .execute(&mut *tx)
                    .await?;

                    tx.commit().await?;
                    Ok(())
                }
                .await;

                if let Err(e) = result {
                    bar.println(format!(
                        "Error processing {}: {:?}",
                        old_id, e
                    ));
                }
            }
        })
        .await;

    Ok(())
}
