#![allow(clippy::implicit_hasher, clippy::items_after_statements)]

pub mod crawling;
pub mod mfbot;
pub mod scrapbook;
pub mod stats;
pub mod underworld;
pub mod update;

use std::{
    collections::HashMap,
    sync::LazyLock,
    time::{Duration, Instant},
};

pub use crawling::*;
pub use mfbot::*;
pub use scrapbook::*;
use sf_api::gamestate::GameState;
use sqlx::{Pool, Postgres};
pub use stats::*;
use tokio::sync::{Mutex, MutexGuard, OnceCell, RwLock};

use crate::error::SFSError;

type CacheMap<K, V> = LazyLock<RwLock<HashMap<K, V>>>;
type CacheSlot<K> = LazyLock<Mutex<Option<(K, Instant)>>>;

struct CacheEntry<T> {
    result: T,
    insertion_time: chrono::DateTime<chrono::Utc>,
}

/// Gets the connection pool, that is going to be shared accross invocations
pub async fn get_db() -> Result<Pool<Postgres>, SFSError> {
    static DB: OnceCell<Pool<Postgres>> = OnceCell::const_new();

    let get_options = || {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(500)
            .max_lifetime(Some(Duration::from_mins(3)))
            .min_connections(10)
            .acquire_timeout(Duration::from_secs(100))
    };

    Ok(DB
        .get_or_try_init(|| get_options().connect(env!("DATABASE_URL")))
        .await?
        .to_owned())
}

/// Pool of cached gamestates
static GAMESTATE_POOL: LazyLock<Mutex<Vec<&'static Mutex<Box<GameState>>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Gets a gamestate from the pool of available gamestates. If the pool is
/// exhausted (all locked), a new gamestate will be created
pub async fn get_gamestate() -> MutexGuard<'static, Box<GameState>> {
    let mut pool = GAMESTATE_POOL.lock().await;
    if let Some(guard) = pool.iter().find_map(|m| m.try_lock().ok()) {
        return guard;
    }
    // All locked — create a new one and add it to the pool
    let new_gs = Box::new(GameState::default());
    let new_mutex: &'static Mutex<Box<GameState>> =
        Box::leak(Box::new(Mutex::new(new_gs)));
    pool.push(new_mutex);
    let gs = new_mutex.lock().await;
    drop(pool);
    gs
}

pub async fn get_server_id(
    db: &Pool<Postgres>,
    mut url: String,
) -> Result<i32, SFSError> {
    static LOOKUP_CACHE: LazyLock<RwLock<HashMap<String, i32>>> =
        LazyLock::new(|| RwLock::new(HashMap::new()));

    if !url.starts_with("http") {
        url = format!("https://{url}");
    }
    let Ok(mut server) = url::Url::parse(&url) else {
        log::error!("Could not parse url: {url}");
        return Err(SFSError::InvalidServer);
    };
    if server.set_scheme("https").is_err() {
        log::error!("Could not set scheme: {server}");
        return Err(SFSError::InvalidServer);
    }
    server.set_path("");
    let url = server.to_string();

    if let Some(id) = LOOKUP_CACHE.read().await.get(&url) {
        return Ok(*id);
    }

    let mut cache = LOOKUP_CACHE.write().await;

    if let Some(id) = cache.get(&url) {
        return Ok(*id);
    }
    let time = (chrono::Utc::now() - crate::common::days(30)).naive_utc();
    let server_id = sqlx::query_scalar!(
        "INSERT INTO server (url, last_hof_crawl)
        VALUES ($1, $2)
        ON CONFLICT(url) DO UPDATE SET last_hof_crawl = server.last_hof_crawl
        RETURNING server_id",
        url,
        time
    )
    .fetch_one(db)
    .await?;

    log::info!("Fed server cache with {url}");
    cache.insert(url, server_id);
    Ok(server_id)
}
