//! Database schema and migrations
use crate::error::Result;
use crate::event::{single_char_tagname, Event};
use crate::utils::is_lower_hex;
use const_format::formatcp;
use futures_util::StreamExt;
use sqlx::{Executor, Row, SqlitePool};
use std::cmp::Ordering;
use std::time::Instant;
use tracing::{debug, error, info};

/// Startup DB Pragmas
pub const STARTUP_SQL: &str = r##"
PRAGMA main.synchronous=NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA journal_size_limit=32768;
pragma mmap_size = 17179869184; -- cap mmap at 16GB
"##;

/// Latest database version
pub const DB_VERSION: usize = 11;

/// Schema definition
const INIT_SQL: &str = formatcp!(
    r##"
-- Database settings
PRAGMA encoding = "UTF-8";
PRAGMA journal_mode=WAL;
PRAGMA main.synchronous=NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA application_id = 1654008667;
PRAGMA user_version = {};

-- Event Table
CREATE TABLE IF NOT EXISTS event (
id INTEGER PRIMARY KEY,
event_hash BLOB NOT NULL, -- 4-byte hash
first_seen INTEGER NOT NULL, -- when the event was first seen (not authored!) (seconds since 1970)
created_at INTEGER NOT NULL, -- when the event was authored
author BLOB NOT NULL, -- author pubkey
delegated_by BLOB, -- delegator pubkey (NIP-26)
kind INTEGER NOT NULL, -- event kind
hidden INTEGER, -- relevant for queries
content TEXT NOT NULL -- serialized json of event object
);

-- Event Indexes
CREATE UNIQUE INDEX IF NOT EXISTS event_hash_index ON event(event_hash);
CREATE INDEX IF NOT EXISTS author_index ON event(author);
CREATE INDEX IF NOT EXISTS created_at_index ON event(created_at);
CREATE INDEX IF NOT EXISTS delegated_by_index ON event(delegated_by);
CREATE INDEX IF NOT EXISTS event_composite_index ON event(kind,created_at);

-- Tag Table
-- Tag values are stored as either a BLOB (if they come in as a
-- hex-string), or TEXT otherwise.
-- This means that searches need to select the appropriate column.
CREATE TABLE IF NOT EXISTS tag (
id INTEGER PRIMARY KEY,
event_id INTEGER NOT NULL, -- an event ID that contains a tag.
name TEXT, -- the tag name ("p", "e", whatever)
value TEXT, -- the tag value, if not hex.
value_hex BLOB, -- the tag value, if it can be interpreted as a lowercase hex string.
FOREIGN KEY(event_id) REFERENCES event(id) ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS tag_val_index ON tag(value);
CREATE INDEX IF NOT EXISTS tag_val_hex_index ON tag(value_hex);
CREATE INDEX IF NOT EXISTS tag_composite_index ON tag(event_id,name,value_hex,value);
CREATE INDEX IF NOT EXISTS tag_name_eid_index ON tag(name,event_id,value_hex);

-- NIP-05 User Validation
CREATE TABLE IF NOT EXISTS user_verification (
id INTEGER PRIMARY KEY,
metadata_event INTEGER NOT NULL, -- the metadata event used for this validation.
name TEXT NOT NULL, -- the nip05 field value (user@domain).
verified_at INTEGER, -- timestamp this author/nip05 was most recently verified.
failed_at INTEGER, -- timestamp a verification attempt failed (host down).
failure_count INTEGER DEFAULT 0, -- number of consecutive failures.
FOREIGN KEY(metadata_event) REFERENCES event(id) ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS user_verification_name_index ON user_verification(name);
CREATE INDEX IF NOT EXISTS user_verification_event_index ON user_verification(metadata_event);
"##,
    DB_VERSION
);

/// Determine the current application database schema version.
pub async fn curr_db_version(conn: &SqlitePool) -> Result<usize> {
    let curr_version: u32 = sqlx::query_scalar("PRAGMA user_version;")
        .fetch_one(conn)
        .await?;
    Ok(curr_version as usize)
}

async fn mig_init(conn: &SqlitePool) -> Result<usize> {
    match conn.execute(INIT_SQL).await {
        Ok(_) => {
            info!(
                "database pragma/schema initialized to v{}, and ready",
                DB_VERSION
            );
        }
        Err(err) => {
            error!("update failed: {}", err);
            panic!("database could not be initialized");
        }
    }
    Ok(DB_VERSION)
}

/// Upgrade DB to latest version, and execute pragma settings
pub async fn upgrade_db(conn: &SqlitePool) -> Result<usize> {
    // check the version.
    let mut curr_version = curr_db_version(conn).await?;
    info!("DB version = {:?}", curr_version);

    /* todo!("Redo this")
    debug!(
        "SQLite max query parameters: {}",
        conn.limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER)
    );
    debug!(
        "SQLite max table/blob/text length: {} MB",
        (conn.limit(Limit::SQLITE_LIMIT_LENGTH) as f64 / (1024 * 1024) as f64).floor()
    );
    debug!(
        "SQLite max SQL length: {} MB",
        (conn.limit(Limit::SQLITE_LIMIT_SQL_LENGTH) as f64 / (1024 * 1024) as f64).floor()
    );*/

    match curr_version.cmp(&DB_VERSION) {
        // Database is new or not current
        Ordering::Less => {
            // initialize from scratch
            if curr_version == 0 {
                curr_version = mig_init(conn).await?;
            }
            // for initialized but out-of-date schemas, proceed to
            // upgrade sequentially until we are current.
            if curr_version == 1 {
                curr_version = mig_1_to_2(conn).await?;
            }

            if curr_version == 2 {
                curr_version = mig_2_to_3(conn).await?;
            }

            if curr_version == 3 {
                curr_version = mig_3_to_4(conn).await?;
            }

            if curr_version == 4 {
                curr_version = mig_4_to_5(conn).await?;
            }

            if curr_version == 5 {
                curr_version = mig_5_to_6(conn).await?;
            }
            if curr_version == 6 {
                curr_version = mig_6_to_7(conn).await?;
            }
            if curr_version == 7 {
                curr_version = mig_7_to_8(conn).await?;
            }
            if curr_version == 8 {
                curr_version = mig_8_to_9(conn).await?;
            }
            if curr_version == 9 {
                curr_version = mig_9_to_10(conn).await?;
            }
            if curr_version == 10 {
                curr_version = mig_10_to_11(conn).await?;
            }

            if curr_version == DB_VERSION {
                info!(
                    "All migration scripts completed successfully.  Welcome to v{}.",
                    DB_VERSION
                );
            }
        }
        // Database is current, all is good
        Ordering::Equal => {
            debug!("Database version was already current (v{})", DB_VERSION);
        }
        // Database is newer than what this code understands, abort
        Ordering::Greater => {
            panic!(
                "Database version is newer than supported by this executable (v{} > v{})",
                curr_version, DB_VERSION
            );
        }
    }

    // Setup PRAGMA
    conn.execute(STARTUP_SQL).await?;
    debug!("SQLite PRAGMA startup completed");
    Ok(curr_version)
}

//// Migration Scripts

async fn mig_1_to_2(conn: &SqlitePool) -> Result<usize> {
    // only change is adding a hidden column to events.
    let upgrade_sql = r##"
ALTER TABLE event ADD hidden INTEGER;
UPDATE event SET hidden=FALSE;
PRAGMA user_version = 2;
"##;
    match conn.execute(upgrade_sql).await {
        Ok(_) => {
            info!("database schema upgraded v1 -> v2");
        }
        Err(err) => {
            error!("update failed: {}", err);
            panic!("database could not be upgraded");
        }
    }
    Ok(2)
}

async fn mig_2_to_3(conn: &SqlitePool) -> Result<usize> {
    // this version lacks the tag column
    info!("database schema needs update from 2->3");
    let upgrade_sql = r##"
CREATE TABLE IF NOT EXISTS tag (
id INTEGER PRIMARY KEY,
event_id INTEGER NOT NULL, -- an event ID that contains a tag.
name TEXT, -- the tag name ("p", "e", whatever)
value TEXT, -- the tag value, if not hex.
value_hex BLOB, -- the tag value, if it can be interpreted as a hex string.
FOREIGN KEY(event_id) REFERENCES event(id) ON UPDATE CASCADE ON DELETE CASCADE
);
PRAGMA user_version = 3;
"##;
    // TODO: load existing refs into tag table
    match conn.execute(upgrade_sql).await {
        Ok(_) => {
            info!("database schema upgraded v2 -> v3");
        }
        Err(err) => {
            error!("update failed: {}", err);
            panic!("database could not be upgraded");
        }
    }
    // iterate over every event/pubkey tag
    let mut tx = conn.begin().await?;
    {
        let mut query = sqlx::query(
            "select event_id, \"e\", lower(hex(referenced_event)) from event_ref \
            union select event_id, \"p\", lower(hex(referenced_pubkey)) from pubkey_ref;",
        )
        .fetch(conn);

        while let Some(Ok(row)) = query.next().await {
            // we want to capture the event_id that had the tag, the tag name, and the tag hex value.
            let event_id: i64 = row.get(0);
            let tag_name: String = row.get(1);
            let tag_value: String = row.get(2);
            // this will leave behind p/e tags that were non-hex, but they are invalid anyways.
            if is_lower_hex(&tag_value) {
                sqlx::query("INSERT INTO tag (event_id, name, value_hex) VALUES (?1, ?2, ?3);")
                    .bind(event_id)
                    .bind(tag_name)
                    .bind(hex::decode(tag_value).ok())
                    .execute(&mut tx)
                    .await?;
            }
        }
    }
    info!("Updated tag values");
    tx.commit().await?;
    Ok(3)
}

async fn mig_3_to_4(conn: &SqlitePool) -> Result<usize> {
    info!("database schema needs update from 3->4");
    let upgrade_sql = r##"
-- incoming metadata events with nip05
CREATE TABLE IF NOT EXISTS user_verification (
id INTEGER PRIMARY KEY,
metadata_event INTEGER NOT NULL, -- the metadata event used for this validation.
name TEXT NOT NULL, -- the nip05 field value (user@domain).
verified_at INTEGER, -- timestamp this author/nip05 was most recently verified.
failed_at INTEGER, -- timestamp a verification attempt failed (host down).
failure_count INTEGER DEFAULT 0, -- number of consecutive failures.
FOREIGN KEY(metadata_event) REFERENCES event(id) ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS user_verification_name_index ON user_verification(name);
CREATE INDEX IF NOT EXISTS user_verification_event_index ON user_verification(metadata_event);
PRAGMA user_version = 4;
"##;
    match conn.execute(upgrade_sql).await {
        Ok(_) => {
            info!("database schema upgraded v3 -> v4");
        }
        Err(err) => {
            error!("update failed: {}", err);
            panic!("database could not be upgraded");
        }
    }
    Ok(4)
}

async fn mig_4_to_5(conn: &SqlitePool) -> Result<usize> {
    info!("database schema needs update from 4->5");
    let upgrade_sql = r##"
DROP TABLE IF EXISTS event_ref;
DROP TABLE IF EXISTS pubkey_ref;
PRAGMA user_version=5;
"##;
    match conn.execute(upgrade_sql).await {
        Ok(_) => {
            info!("database schema upgraded v4 -> v5");
        }
        Err(err) => {
            error!("update failed: {}", err);
            panic!("database could not be upgraded");
        }
    }
    Ok(5)
}

async fn mig_5_to_6(conn: &SqlitePool) -> Result<usize> {
    info!("database schema needs update from 5->6");
    // We need to rebuild the tags table.  iterate through the
    // event table.  build event from json, insert tags into a
    // fresh tag table.  This was needed due to a logic error in
    // how hex-like tags got indexed.
    let start = Instant::now();
    let mut tx = conn.begin().await?;
    {
        // Clear out table
        tx.execute("DELETE FROM tag;").await?;
        let mut query = sqlx::query("select id, content from event order by id").fetch(conn);

        while let Some(Ok(row)) = query.next().await {
            // we want to capture the event_id that had the tag, the tag name, and the tag hex value.
            let event_id: i64 = row.get(0);
            let event_json: String = row.get(1);
            let event: Event = serde_json::from_str(&event_json)?;
            // look at each event, and each tag, creating new tag entries if appropriate.
            for t in event.tags.iter().filter(|x| x.len() > 1) {
                let tag_name = t.get(0).unwrap();
                let tag_name_char_opt = single_char_tagname(tag_name);
                if tag_name_char_opt.is_none() {
                    continue;
                }
                // safe because len was > 1
                let tag_val = t.get(1).unwrap();
                // insert as BLOB if we can restore it losslessly.
                // this means it needs to be even length and lowercase.
                if (tag_val.len() % 2 == 0) && is_lower_hex(tag_val) {
                    sqlx::query("INSERT INTO tag (event_id, name, value_hex) VALUES (?1, ?2, ?3)")
                        .bind(event_id)
                        .bind(tag_name)
                        .bind(hex::decode(tag_val).ok())
                        .execute(&mut tx)
                        .await?;
                } else {
                    // otherwise, insert as text
                    sqlx::query("INSERT INTO tag (event_id, name, value) VALUES (?1, ?2, ?3)")
                        .bind(event_id)
                        .bind(tag_name)
                        .bind(tag_val)
                        .execute(&mut tx)
                        .await?;
                }
            }
        }
        tx.execute("PRAGMA user_version = 6").await?;
    }
    tx.commit().await?;
    info!("database schema upgraded v5 -> v6 in {:?}", start.elapsed());
    // vacuum after large table modification
    let start = Instant::now();
    conn.execute("VACUUM").await?;
    info!("vacuumed DB after tags rebuild in {:?}", start.elapsed());
    Ok(6)
}

async fn mig_6_to_7(conn: &SqlitePool) -> Result<usize> {
    info!("database schema needs update from 6->7");
    // only change is adding a hidden column to events.
    let upgrade_sql = r##"
ALTER TABLE event ADD delegated_by BLOB;
CREATE INDEX IF NOT EXISTS delegated_by_index ON event(delegated_by);
PRAGMA user_version = 7;
"##;
    match conn.execute(upgrade_sql).await {
        Ok(_) => {
            info!("database schema upgraded v6 -> v7");
        }
        Err(err) => {
            error!("update failed: {}", err);
            panic!("database could not be upgraded");
        }
    }
    Ok(7)
}

async fn mig_7_to_8(conn: &SqlitePool) -> Result<usize> {
    info!("database schema needs update from 7->8");
    // Remove redundant indexes, and add a better multi-column index.
    let upgrade_sql = r##"
DROP INDEX IF EXISTS created_at_index;
DROP INDEX IF EXISTS kind_index;
CREATE INDEX IF NOT EXISTS event_composite_index ON event(kind,created_at);
PRAGMA user_version = 8;
"##;
    match conn.execute(upgrade_sql).await {
        Ok(_) => {
            info!("database schema upgraded v7 -> v8");
        }
        Err(err) => {
            error!("update failed: {}", err);
            panic!("database could not be upgraded");
        }
    }
    Ok(8)
}

async fn mig_8_to_9(conn: &SqlitePool) -> Result<usize> {
    info!("database schema needs update from 8->9");
    // Those old indexes were actually helpful...
    let upgrade_sql = r##"
CREATE INDEX IF NOT EXISTS created_at_index ON event(created_at);
CREATE INDEX IF NOT EXISTS event_composite_index ON event(kind,created_at);
PRAGMA user_version = 9;
"##;
    match conn.execute(upgrade_sql).await {
        Ok(_) => {
            info!("database schema upgraded v8 -> v9");
        }
        Err(err) => {
            error!("update failed: {}", err);
            panic!("database could not be upgraded");
        }
    }
    Ok(9)
}

async fn mig_9_to_10(conn: &SqlitePool) -> Result<usize> {
    info!("database schema needs update from 9->10");
    // Those old indexes were actually helpful...
    let upgrade_sql = r##"
CREATE INDEX IF NOT EXISTS tag_composite_index ON tag(event_id,name,value_hex,value);
PRAGMA user_version = 10;
"##;
    match conn.execute(upgrade_sql).await {
        Ok(_) => {
            info!("database schema upgraded v9 -> v10");
        }
        Err(err) => {
            error!("update failed: {}", err);
            panic!("database could not be upgraded");
        }
    }
    Ok(10)
}

async fn mig_10_to_11(conn: &SqlitePool) -> Result<usize> {
    info!("database schema needs update from 10->11");
    // Those old indexes were actually helpful...
    let upgrade_sql = r##"
CREATE INDEX IF NOT EXISTS tag_name_eid_index ON tag(name,event_id,value_hex);
reindex;
pragma optimize;
PRAGMA user_version = 11;
"##;
    match conn.execute(upgrade_sql).await {
        Ok(_) => {
            info!("database schema upgraded v10 -> v11");
        }
        Err(err) => {
            error!("update failed: {}", err);
            panic!("database could not be upgraded");
        }
    }
    Ok(11)
}
