use std::{collections::BTreeMap, io::Write, time::Duration};

use chrono::{DateTime, NaiveDate, Utc};
use clap::Parser;
use flate2::{Compression, write::ZlibEncoder};
use futures::TryStreamExt;
use indicatif::ProgressStyle;
use serde::{Deserialize, Serialize};
use sf_api::gamestate::{character::Class, unlockables::EquipmentIdent};
use sf_info_lib::{
    common::{days, decompress_ident},
    db::get_db,
};

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct ZHofBackup {
    #[serde(default)]
    pub todo_pages: Vec<usize>,
    #[serde(default)]
    pub invalid_pages: Vec<usize>,
    #[serde(default)]
    pub todo_accounts: Vec<String>,
    #[serde(default)]
    pub invalid_accounts: Vec<String>,
    #[serde(default)]
    pub order: CrawlingOrder,
    pub export_time: Option<DateTime<Utc>>,
    pub characters: Vec<CharacterInfo>,
    #[serde(default)]
    pub lvl_skipped_accounts: BTreeMap<u32, Vec<String>>,
    #[serde(default)]
    pub min_level: u32,
    #[serde(default = "default_max_lvl")]
    pub max_level: u32,
}

fn default_max_lvl() -> u32 {
    9999
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct CharacterInfo {
    equipment: Vec<EquipmentIdent>,
    name: String,
    uid: u32,
    level: u16,
    #[serde(skip)]
    stats: Option<u32>,
    #[serde(skip)]
    fetch_date: Option<NaiveDate>,
    #[serde(skip)]
    class: Option<Class>,
}

#[derive(
    Debug, Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq,
)]
pub enum CrawlingOrder {
    #[default]
    Random,
    TopDown,
    BottomUp,
}

#[derive(Debug, clap::Parser)]
pub struct Args {
    /// Only fetch this url
    #[clap(short, long)]
    pub url: Option<String>,
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let db = get_db().await?;

    let now = Utc::now().naive_utc();
    let three_days_ago = now - days(3);

    let server_ids = sqlx::query!(
        "SELECT server_id, url FROM server WHERE last_hof_crawl > $1 ORDER BY \
         server_id ASC",
        three_days_ago
    )
    .fetch_all(&db)
    .await?;

    for server in server_ids
        .into_iter()
        .filter(|a| args.url.as_ref().is_none_or(|b| a.url.contains(b)))
    {
        let bar = indicatif::ProgressBar::new_spinner();
        let style = ProgressStyle::default_spinner()
            .template(
                "{spinner} {prefix:17.red} - {msg:25.blue} {wide_bar:.green} \
                 [{elapsed_precise}/{duration_precise}] [{pos:6}/{len:6}]",
            )
            .unwrap_or_else(|_| ProgressStyle::default_spinner());
        bar.set_style(style);
        bar.enable_steady_tick(Duration::from_millis(100));
        bar.set_prefix(server.url.clone());
        bar.set_message("Loading player data");

        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM player WHERE server_id = $1 AND is_removed \
             = FALSE AND level IS NOT NULL",
            server.server_id
        )
        .fetch_one(&db)
        .await?
        .unwrap_or(0) as u64;

        if count == 0 {
            continue;
        }

        bar.set_length(count);
        bar.set_message("Processing players...");

        let server_ident = server
            .url
            .trim_start_matches("https:")
            .replace(['/', '.'], "");
        let path = format!("{server_ident}.zhof");

        let file = std::fs::File::create(&path)?;
        let mut encoder = ZlibEncoder::new(file, Compression::best());

        // Write JSON opening — header fields + start of characters array
        write!(
            encoder,
            r#"{{"export_time":"{}","characters":["#,
            Utc::now().to_rfc3339()
        )?;

        let mut stream = sqlx::query!(
            "SELECT name, level, server_player_id, equipment
            FROM player
            WHERE server_id = $1 AND is_removed = FALSE AND level IS NOT NULL",
            server.server_id
        )
        .fetch(&db);

        let mut first = true;

        while let Some(rec) = stream.try_next().await? {
            let Some(level) = rec.level else {
                continue;
            };
            let Some(uid) = rec.server_player_id else {
                continue;
            };

            if !first {
                write!(encoder, ",")?;
            }
            first = false;

            let equipment: Vec<_> = rec
                .equipment
                .unwrap_or_default()
                .into_iter()
                .map(decompress_ident)
                .collect();

            let info = CharacterInfo {
                equipment,
                name: rec.name,
                uid: uid as u32,
                level: level as u16,
                stats: None,
                fetch_date: None,
                class: None,
            };

            serde_json::to_writer(&mut encoder, &info)?;
            bar.inc(1);
        }

        write!(encoder, "]}}")?;
        encoder.finish()?;

        std::fs::write(
            format!("{server_ident}.version"),
            Utc::now().to_rfc2822(),
        )?;

        bar.finish_and_clear();
    }

    Ok(())
}
