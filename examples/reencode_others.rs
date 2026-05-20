use std::sync::atomic::AtomicUsize;

use chrono::Local;
use futures::StreamExt;
use sf_api::session::Response;
use sf_info_lib::db::{crawling::*, get_db};
use tokio::task;
use zstd::decode_all;

const OLD_VERSION: i16 = 4;
const NEW_VERSION: i16 = 5;

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
        "SELECT otherplayer_resp_id FROM otherplayer_resp WHERE version = $1",
        OLD_VERSION
    )
    .fetch_all(&db)
    .await?;

    let bar = indicatif::ProgressBar::new(resp_ids.len() as u64);

    static TOTAL: AtomicUsize = AtomicUsize::new(0);
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
                        WHERE otherplayer_resp_id = $1 AND version = $2",
                        old_id,
                        OLD_VERSION
                    )
                    .fetch_optional(&db)
                    .await?;

                    let Some(response) = response else {
                        return Ok(());
                    };
                    let old_size = response.len();

                    let decoded = task::spawn_blocking(move || {
                        decode_all(response.as_slice())
                    })
                    .await??;
                    let decoded = String::from_utf8(decoded)?;
                    let response =
                        Response::parse(decoded, Local::now().naive_local())?;

                    let resp = reencode_response(&response)?;

                    let nv = TOTAL.fetch_add(
                        old_size.saturating_sub(resp.len()),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    bar.println(format!("{old_size} => {}, total: {}", resp.len(), nv));


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
                                VALUES ($1, $2)
                                RETURNING otherplayer_resp_id",
                                &resp,
                                NEW_VERSION
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
                    bar.println(format!("Error processing {old_id}: {e:?}"));
                }
            }
        })
        .await;

    Ok(())
}
