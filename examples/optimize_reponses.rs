use std::{fmt::Write, io::Cursor};

use bytemuck::cast_vec;
use futures::StreamExt;
use sf_info_lib::db::get_db;
use zstd::{decode_all, encode_all};

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = get_db().await?;

    let ids = sqlx::query!(
        "SELECT otherplayer_resp_id, player_info_id, player_id
        FROM player_info
        NATURAL JOIN otherplayer_resp
        LIMIT 10000"
    )
    .fetch_all(&db)
    .await?;

    tokio::fs::create_dir("strings").await;
    tokio::fs::create_dir("strings_zst").await;
    tokio::fs::create_dir("numbers").await;
    tokio::fs::create_dir("numbers_zst").await;

    let tasks = ids.into_iter().map(|id| {
        let db = db.clone();

        async move {
            let data = sqlx::query!(
                "SELECT hash, otherplayer_resp FROM otherplayer_resp WHERE \
                 otherplayer_resp_id = $1",
                id.otherplayer_resp_id
            )
            .fetch_one(&db)
            .await
            .unwrap();

            let decoded_resp =
                decode_all(Cursor::new(data.otherplayer_resp)).unwrap();
            let raw_response = String::from_utf8(decoded_resp).unwrap();
            let data2: Result<Vec<i64>, _> =
                raw_response.split('/').map(|a| a.trim().parse()).collect();
            let mut int_data = data2.unwrap();
            // Set rank to 0, since the rank can change even if the rest stays
            // the same, removing most of the dedup benefits. We
            // could also reset playerid to 0, but
            int_data[6] = 0;

            let mut new_raw_resp = String::new();
            for num in &int_data {
                if !new_raw_resp.is_empty() {
                    new_raw_resp.push('/');
                }
                new_raw_resp.write_fmt(format_args!("{num}")).unwrap();
            }
            tokio::fs::write(
                format!("strings/{}.txt", data.hash),
                &new_raw_resp,
            )
            .await
            .unwrap();

            let encoded = encode_all(new_raw_resp.as_bytes(), 3).unwrap();
            tokio::fs::write(
                format!("strings_zst/{}.txt.zst", data.hash),
                &encoded,
            )
            .await
            .unwrap();

            let vec_u8: Vec<u8> =
                int_data.into_iter().flat_map(|a| a.to_le_bytes()).collect();

            tokio::fs::write(format!("numbers/{}", data.hash), &encoded)
                .await
                .unwrap();

            let cmp = encode_all(vec_u8.as_slice(), 3).unwrap();
            tokio::fs::write(format!("numbers_zst/{}", data.hash), &cmp)
                .await
                .unwrap();
        }
    });

    futures::stream::iter(tasks)
        .buffer_unordered(10)
        .collect::<Vec<_>>()
        .await;

    Ok(())
}
