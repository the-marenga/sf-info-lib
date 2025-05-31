use std::time::Duration;

pub const fn minutes(minutes: u64) -> Duration {
    Duration::from_secs(60 * minutes)
}
pub const fn hours(hours: u64) -> Duration {
    Duration::from_secs(60 * 60 * hours)
}
pub const fn days(days: u64) -> Duration {
    Duration::from_secs(60 * 60 * 24 * days)
}

/// Compresses the original Equipment Ident into a single i32
#[cfg(feature = "db")]
pub fn compress_ident(
    ident: sf_api::gamestate::unlockables::EquipmentIdent,
) -> i32 {
    let mut res = i64::from(ident.model_id); // 0..16
    res |= i64::from(ident.color) << 16; // 16..24
    res |= (ident.typ as i64) << 24; // 24..28
    res |= ident.class.map_or(0, |a| a as i64 + 1) << 28; // 28..32
    // We never use more than 32 bits, so this is fine. Could probably have
    // used i32 everywhere, but I was unsure about signed bit causing issues
    res as i32
}
