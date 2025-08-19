use std::time::Duration;

#[cfg(feature = "db")]
use sf_api::gamestate::unlockables::EquipmentIdent;

use crate::types::ServerCategory;

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

#[cfg(feature = "db")]
#[allow(clippy::missing_panics_doc)]
pub fn decompress_ident(ident: i32) -> EquipmentIdent {
    use sf_api::gamestate::{character::Class, items::EquipmentSlot};
    let model_id = ident & 0xFF;
    let color = (ident >> 16) & 0xF;
    let typ = (ident >> 24) & 0xF;
    let typ = match typ {
        1 => EquipmentSlot::Hat,
        2 => EquipmentSlot::BreastPlate,
        3 => EquipmentSlot::Gloves,
        4 => EquipmentSlot::FootWear,
        5 => EquipmentSlot::Amulet,
        6 => EquipmentSlot::Belt,
        7 => EquipmentSlot::Ring,
        8 => EquipmentSlot::Talisman,
        9 => EquipmentSlot::Weapon,
        10 => EquipmentSlot::Shield,
        _ => panic!(),
    };
    let class = (ident >> 28) & 0xF;
    let class = match class {
        0 => None,
        1 => Some(Class::Warrior),
        2 => Some(Class::Mage),
        3 => Some(Class::Scout),
        _ => panic!(),
    };

    EquipmentIdent {
        class,
        typ,
        model_id: model_id as u16,
        color: color as u8,
    }
}

pub fn ident_to_info(ident: &str) -> (String, ServerCategory) {
    if let Some((_, num)) = ident.split_once("eu") {
        (format!("https://s{num}.sfgame.eu/"), ServerCategory::Europe)
    } else if let Some((_, num)) = ident.split_once("f") {
        (format!("https://f{num}.sfgame.net/"), ServerCategory::Fused)
    } else if let Some((_, num)) = ident.split_once("am") {
        (
            format!("https://am{num}.sfgame.net/"),
            ServerCategory::America,
        )
    } else if let Some((_, num)) = ident.split_once("w") {
        (
            format!("https://w{num}.sfgame.net/"),
            ServerCategory::International,
        )
    } else {
        (String::new(), ServerCategory::Fused)
    }
}
