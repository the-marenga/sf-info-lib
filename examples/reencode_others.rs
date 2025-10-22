use futures::StreamExt;
use sf_info_lib::db::{STORED_SPLIT_CHAR, get_db, reencode_response};
use zstd::{decode_all, encode_all};

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = get_db().await?;

    let mut player_ids: Vec<i32> =
        sqlx::query_scalar!("SELECT player_id FROM player")
            .fetch_all(&db)
            .await?;

    // DEBUG
    player_ids = vec![741427];

    let bar = indicatif::ProgressBar::new(player_ids.len() as u64);

    futures::stream::iter(player_ids)
        .for_each_concurrent(8, |player_id| {
            let db = db.clone();
            let bar = bar.clone();
            async move {
                bar.inc(1);
                let result: Result<(), Box<dyn std::error::Error>> = async {
                    let response_ids: Vec<i32> = sqlx::query_scalar!(
                        "SELECT DISTINCT otherplayer_resp_id FROM player_info \
                         WHERE player_id = $1",
                        player_id
                    )
                    .fetch_all(&db)
                    .await?;

                    for id in response_ids {
                        let response = sqlx::query_scalar!(
                            "SELECT otherplayer_resp
                            FROM otherplayer_resp
                            WHERE otherplayer_resp_id = $1 AND version < 3",
                            id
                        )
                        .fetch_optional(&db)
                        .await?;

                        let Some(response) = response else {
                            continue;
                        };

                        let decoded = decode_all(response.as_slice())?;
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
                        let resp = encode_all(new_raw_resp.as_bytes(), 3)?;

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
                                    "INSERT INTO otherplayer_resp \
                                     (otherplayer_resp, version)
                                VALUES ($1, 3)
                                RETURNING otherplayer_resp_id",
                                    &resp,
                                )
                                .fetch_one(&mut *tx)
                                .await?
                            }
                        };
                        if new_id == id {
                            continue;
                        }

                        sqlx::query!(
                            "UPDATE player_info
                            SET otherplayer_resp_id = $1
                            WHERE player_id = $2 AND otherplayer_resp_id = $3",
                            new_id,
                            player_id,
                            id
                        )
                        .execute(&mut *tx)
                        .await?;
                        tx.commit().await?;
                    }
                    Ok(())
                }
                .await;

                if let Err(e) = result {
                    bar.println(format!(
                        "Error processing player_id {}: {:?}",
                        player_id, e
                    ));
                }
            }
        })
        .await;

    Ok(())
}
