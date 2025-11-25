use chrono::Utc;

use crate::{db::get_db, error::SFSError, types::BugReportArgs};

/// Inserts a new bug report from the mfbot into the db
pub async fn insert_bug(args: BugReportArgs) -> Result<(), SFSError> {
    let current_time = Utc::now().naive_utc();
    sqlx::query!(
        "INSERT INTO error (stacktrace, version, additional_info, os, arch, \
         error_text, hwid, timestamp) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        args.stacktrace,
        args.version,
        args.additional_info,
        args.os,
        args.arch,
        args.error_text,
        args.hwid,
        current_time
    )
    .execute(&get_db().await?)
    .await?;

    Ok(())
}
