mod event;
pub use event::DbEvent;

mod event_flags;
pub use event_flags::DbEventFlags;

mod event_relay;
pub use event_relay::DbEventRelay;

mod event_hashtag;
pub use event_hashtag::DbEventHashtag;

mod event_tag;
pub use event_tag::DbEventTag;

mod event_relationship;
pub use event_relationship::DbEventRelationship;

mod relay;
pub use relay::DbRelay;

mod person_relay;
pub use person_relay::DbPersonRelay;

use crate::error::Error;
use crate::globals::GLOBALS;
use fallible_iterator::FallibleIterator;
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use std::fs;
use std::sync::atomic::Ordering;

pub fn init_database() -> Result<Pool<SqliteConnectionManager>, Error> {
    let mut data_dir =
        dirs::data_dir().ok_or("Cannot find a directory to store application data.")?;
    data_dir.push("gossip");

    // Create our data directory only if it doesn't exist
    fs::create_dir_all(&data_dir)?;

    // Connect to (or create) our database
    let mut db_path = data_dir.clone();
    db_path.push("gossip.sqlite");

    let sqlite_connection_manager = SqliteConnectionManager::file(db_path).with_flags(
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    );

    let pool = Pool::new(sqlite_connection_manager)
        .map_err(|_| Error::from("Failed to create r2d2 SQLite connection pool"))?;

    // Turn on foreign keys
    let connection = pool.get()?;
    connection.execute("PRAGMA foreign_keys = ON", ())?;

    Ok(pool)
}

/// Check and upgrade our data schema
pub fn check_and_upgrade() -> Result<(), Error> {
    let db = GLOBALS.db.get()?;

    match db.query_row(
        "SELECT schema_version FROM local_settings LIMIT 1",
        [],
        |row| row.get::<usize, usize>(0),
    ) {
        Ok(version) => {
            upgrade(db, version)?;
        }
        Err(e) => {
            if let rusqlite::Error::SqliteFailure(_, Some(ref s)) = e {
                if s.contains("no such table") {
                    old_check_and_upgrade(db)?;
                    // falls through
                }
            } else {
                return Err(e.into());
            }
        }
    }

    // This only happens once
    normalize_urls()
}

fn old_check_and_upgrade(db: PooledConnection<SqliteConnectionManager>) -> Result<(), Error> {
    match db.query_row(
        "SELECT value FROM settings WHERE key='version'",
        [],
        |row| row.get::<usize, String>(0),
    ) {
        Ok(v) => {
            let version = v.parse::<usize>().unwrap();
            if version < 2 {
                GLOBALS.first_run.store(true, Ordering::Relaxed);
            }
            upgrade(db, version)
        }
        Err(_e) => {
            GLOBALS.first_run.store(true, Ordering::Relaxed);
            // Check the error first!
            upgrade(db, 0)
        }
    }
}

fn upgrade(db: PooledConnection<SqliteConnectionManager>, mut version: usize) -> Result<(), Error> {
    if version > UPGRADE_SQL.len() {
        panic!(
            "Database version {} is newer than this binary which expects version {}.",
            version,
            UPGRADE_SQL.len()
        );
    }

    while version < UPGRADE_SQL.len() {
        tracing::info!("Upgrading database to version {}", version + 1);
        db.execute_batch(UPGRADE_SQL[version + 1 - 1])?;
        version += 1;
        if version < 24 {
            // 24 is when we switched to local_settings
            db.execute(
                "UPDATE settings SET value=? WHERE key='version'",
                (version,),
            )?;
        } else {
            db.execute("UPDATE local_settings SET schema_version=?", (version,))?;
        }
    }

    tracing::info!("Database is at version {}", version);

    Ok(())
}

pub async fn prune() -> Result<(), Error> {
    let db = GLOBALS.db.get()?;
    db.execute_batch(include_str!("sql/prune.sql"))?;

    *GLOBALS.status_message.write().await = "Database prune has completed.".to_owned();

    Ok(())
}

pub fn normalize_urls() -> Result<(), Error> {
    // FIXME make a database backup first (I got a "database disk image is malformed" from this process once)

    tracing::info!("Normalizing Database URLs (this will take some time)");

    let db = GLOBALS.db.get()?;

    let urls_are_normalized: bool = db.query_row(
        "SELECT urls_are_normalized FROM local_settings LIMIT 1",
        [],
        |row| row.get::<usize, bool>(0),
    )?;

    if urls_are_normalized {
        return Ok(());
    }

    db.pragma_update(None, "foreign_keys", "OFF")?;

    // relay.url
    let sql = "SELECT url FROM relay";
    let mut stmt = db.prepare(sql)?;
    let rows = stmt.query([])?;
    let all_rows: Vec<String> = rows.map(|row| row.get(0)).collect()?;
    for urlkey in all_rows.iter() {
        match nostr_types::RelayUrl::try_from_str(urlkey) {
            Ok(url) => {
                let urlstr = url.as_str().to_owned();
                // Update if not equal
                if *urlkey != urlstr {
                    // this one is too verbose
                    // tracing::debug!("Updating non-canonical URL from {} to {}", urlkey, urlstr);
                    let usql = "UPDATE relay SET url=? WHERE url=?";
                    let mut stmt = db.prepare(usql)?;
                    if let Err(e) = stmt.execute((&urlstr, urlkey)) {
                        if let rusqlite::Error::SqliteFailure(_, Some(ref s)) = e {
                            if s.contains("constraint failed") {
                                // Delete this row instead, there is some other row that is already
                                // what we are trying to turn this row into
                                let dsql = "DELETE FROM relay WHERE url=?";
                                let mut stmt = db.prepare(dsql)?;
                                stmt.execute((&urlkey,))?;
                            }
                        } else {
                            return Err(e.into());
                        }
                    }

                    let usql = "UPDATE person_relay SET relay=? WHERE relay=?";
                    let mut stmt = db.prepare(usql)?;
                    stmt.execute((&urlstr, urlkey))?;

                    let usql = "UPDATE event_relay SET relay=? WHERE relay=?";
                    let mut stmt = db.prepare(usql)?;
                    stmt.execute((&urlstr, urlkey))?;
                }
            }
            Err(_) => {
                // Delete if did not parse properly
                tracing::debug!("Deleting invalid relay url {}", urlkey);

                let dsql = "DELETE FROM relay WHERE url=?";
                let mut stmt = db.prepare(dsql)?;
                stmt.execute((urlkey,))?;

                let dsql = "DELETE FROM person_relay WHERE relay=?";
                let mut stmt = db.prepare(dsql)?;
                stmt.execute((urlkey,))?;

                let dsql = "DELETE FROM event_relay WHERE relay=?";
                let mut stmt = db.prepare(dsql)?;
                stmt.execute((urlkey,))?;
            }
        };
    }

    let sql = "UPDATE local_settings SET urls_are_normalized=1";
    let mut stmt = db.prepare(sql)?;
    stmt.execute(())?;

    Ok(())
}

const UPGRADE_SQL: [&str; 31] = [
    include_str!("sql/schema1.sql"),
    include_str!("sql/schema2.sql"),
    include_str!("sql/schema3.sql"),
    include_str!("sql/schema4.sql"),
    include_str!("sql/schema5.sql"),
    include_str!("sql/schema6.sql"),
    include_str!("sql/schema7.sql"),
    include_str!("sql/schema8.sql"),
    include_str!("sql/schema9.sql"),
    include_str!("sql/schema10.sql"),
    include_str!("sql/schema11.sql"),
    include_str!("sql/schema12.sql"),
    include_str!("sql/schema13.sql"),
    include_str!("sql/schema14.sql"),
    include_str!("sql/schema15.sql"),
    include_str!("sql/schema16.sql"),
    include_str!("sql/schema17.sql"),
    include_str!("sql/schema18.sql"),
    include_str!("sql/schema19.sql"),
    include_str!("sql/schema20.sql"),
    include_str!("sql/schema21.sql"),
    include_str!("sql/schema22.sql"),
    include_str!("sql/schema23.sql"),
    include_str!("sql/schema24.sql"),
    include_str!("sql/schema25.sql"),
    include_str!("sql/schema26.sql"),
    include_str!("sql/schema27.sql"),
    include_str!("sql/schema28.sql"),
    include_str!("sql/schema29.sql"),
    include_str!("sql/schema30.sql"),
    include_str!("sql/schema31.sql"),
];
