use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct GetHofArgs {
    pub server: String,
    pub player_count: usize,
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GetCharactersArgs {
    pub server: String,
    pub limit: u32,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct BugReportArgs {
    pub version: i32,
    pub os: String,
    pub arch: String,
    pub hwid: String,

    pub stacktrace: Option<String>,
    pub additional_info: Option<String>,
    pub error_text: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RawOtherPlayer {
    pub info: String,
    pub description: Option<String>,
    pub guild: Option<String>,
    pub soldier_advice: Option<i64>,
    pub fetch_date: String,
    pub raw_equipment: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct ScrapBookAdviceArgs {
    pub raw_scrapbook: String,
    pub server: String,
    pub max_attrs: u64,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
#[cfg_attr(feature = "db", derive(sqlx::prelude::FromRow))]
pub struct ScrapBookAdvice {
    pub player_name: String,
    pub new_count: u32,
    #[serde(skip)]
    pub stats: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ReportHofArgs {
    pub server: String,
    // page => Ranklistplayer
    pub pages: HashMap<u32, String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct HofPlayerInfo {
    pub name: String,
    pub rank: u32,
    pub honor: u32,
    pub level: u32,
    pub guild: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GetHofPlayersArgs {
    pub server_id: i32,
    pub offset: u32,
    pub limit: u32,
}


#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Clone, Copy)]
pub enum ServerCategory {
    International,
    Fused,
    Europe,
    America,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Clone)]
pub struct ServerInfo {
    pub server_id: i32,
    pub url: String,
    pub shorthand: String,
    pub category: ServerCategory,
    pub player_count: i64,
    pub active_players: i64,
    pub classes: HashMap<String, i64>,
    pub lvl_avg: i32,
}

#[derive(Debug, Deserialize, Serialize)]
/// Maps the player name to its crawled data. If the data is none, the
/// player is no longer present on the server
pub struct ServerPlayerReport(pub HashMap<String, Option<RawOtherPlayer>>);

#[derive(Debug, Deserialize, Serialize)]
/// Maps the server url to its crawled players
pub struct CrawlReport(pub HashMap<String, ServerPlayerReport>);

#[derive(Debug, Deserialize, Serialize, PartialEq, Clone)]
pub struct DetailedServerInfo {
    pub server_id: i32,
    pub url: String,
    pub category: ServerCategory,

    pub player_count: i64,
    pub active_players: i64,
    pub new_players: i64,

    pub lvl_avg: i32,
    pub last_scan_hours: u32,

    pub category_rank: u32,
    pub global_rank: u32,

    pub highest_rank_guild: String,
    pub highest_lvl_guild: String,
    pub guild_count: u32,

    pub highest_rank_player: String,
    pub highest_lvl_player: String,
    pub average_player_lvl: u32,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Clone)]
pub struct ServerLevels {
    pub total_level_distribution: Vec<u32>,
    pub top_100_level_distribution: Vec<u32>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Clone)]
pub struct ServerClassDistributions {
    pub class_total: HashMap<String, ClassDistribution>,
    pub class_top_100: HashMap<String, ClassDistribution>,
    pub class_lvl_300_plus: HashMap<String, ClassDistribution>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Clone)]
pub struct ClassDistribution {
    pub count: u32,
    pub active_count: u32,
    pub lvl_avg: u32,
    pub attributes_avg: u32,
    pub honor_avg: u32,
}
