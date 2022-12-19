//! Event persistence and querying
//use crate::config::SETTINGS;
use crate::config::Settings;
use crate::error::{Error, Result};
use crate::event::{single_char_tagname, Event};
use crate::hexrange::hex_range;
use crate::hexrange::HexSearch;
use crate::nip05;
use crate::notice::Notice;
use crate::schema::{upgrade_db, STARTUP_SQL};
use crate::subscription::ReqFilter;
use crate::subscription::Subscription;
use crate::utils::{is_hex, is_lower_hex};
use crate::repo::sqlite::SqliteRepo;
use crate::repo::Repo;
use governor::clock::Clock;
use governor::{Quota, RateLimiter};
use hex;
use r2d2;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use rusqlite::types::ToSql;
use rusqlite::OpenFlags;
use std::fmt::Write as _;
use std::path::Path;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tokio::task;
use tracing::{debug, info, trace, warn};

pub type SqlitePool = r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>;
pub type PooledConnection = r2d2::PooledConnection<r2d2_sqlite::SqliteConnectionManager>;

/// Events submitted from a client, with a return channel for notices
pub struct SubmittedEvent {
    pub event: Event,
    pub notice_tx: tokio::sync::mpsc::Sender<Notice>,
}

/// Database file
pub const DB_FILE: &str = "nostr.db";
/// How many persisted events before optimization is triggered
pub const EVENT_COUNT_OPTIMIZE_TRIGGER: usize = 500;

/// Build a database connection pool.
/// # Panics
///
/// Will panic if the pool could not be created.
#[must_use]
pub fn build_pool(
    name: &str,
    settings: &Settings,
    flags: OpenFlags,
    min_size: u32,
    max_size: u32,
    wait_for_db: bool,
) -> SqlitePool {
    let db_dir = &settings.database.data_directory;
    let full_path = Path::new(db_dir).join(DB_FILE);
    // small hack; if the database doesn't exist yet, that means the
    // writer thread hasn't finished.  Give it a chance to work.  This
    // is only an issue with the first time we run.
    if !settings.database.in_memory {
        while !full_path.exists() && wait_for_db {
            debug!("Database reader pool is waiting on the database to be created...");
            thread::sleep(Duration::from_millis(500));
        }
    }
    let manager = if settings.database.in_memory {
        SqliteConnectionManager::memory()
            .with_flags(flags)
            .with_init(|c| c.execute_batch(STARTUP_SQL))
    } else {
        SqliteConnectionManager::file(&full_path)
            .with_flags(flags)
            .with_init(|c| c.execute_batch(STARTUP_SQL))
    };
    let pool: SqlitePool = r2d2::Pool::builder()
        .test_on_check_out(true) // no noticeable performance hit
        .min_idle(Some(min_size))
        .max_size(max_size)
        .max_lifetime(Some(Duration::from_secs(60)))
        .build(manager)
        .unwrap();
    info!(
        "Built a connection pool {:?} (min={}, max={})",
        name, min_size, max_size
    );
    pool
}

/// Perform normal maintenance
pub fn optimize_db(conn: &mut PooledConnection) -> Result<()> {
    conn.execute_batch("PRAGMA optimize;")?;
    Ok(())
}

/// Spawn a database writer that persists events to the SQLite store.
pub async fn db_writer(
    settings: Settings,
    mut event_rx: tokio::sync::mpsc::Receiver<SubmittedEvent>,
    bcast_tx: tokio::sync::broadcast::Sender<Event>,
    metadata_tx: tokio::sync::broadcast::Sender<Event>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<Result<()>> {
    // are we performing NIP-05 checking?
    let nip05_active = settings.verified_users.is_active();
    // are we requriing NIP-05 user verification?
    let nip05_enabled = settings.verified_users.is_enabled();

    task::spawn_blocking(move || {
        let db_dir = &settings.database.data_directory;
        let full_path = Path::new(db_dir).join(DB_FILE);
        // create a connection pool
        let pool = build_pool(
            "event writer",
            &settings,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
            1,
            2,
            false,
        );
        if settings.database.in_memory {
            info!("using in-memory database, this will not persist a restart!");
        } else {
            info!("opened database {:?} for writing", full_path);
        }
        upgrade_db(&mut pool.get()?)?;

        // Make a copy of the whitelist
        let whitelist = &settings.authorization.pubkey_whitelist.clone();

        // get rate limit settings
        let rps_setting = settings.limits.messages_per_sec;
        let mut most_recent_rate_limit = Instant::now();
        let mut lim_opt = None;
        // Keep rough track of events so we can run optimize eventually.
        let mut optimize_counter: usize = 0;
        let clock = governor::clock::QuantaClock::default();
        if let Some(rps) = rps_setting {
            if rps > 0 {
                info!("Enabling rate limits for event creation ({}/sec)", rps);
                let quota = core::num::NonZeroU32::new(rps * 60).unwrap();
                lim_opt = Some(RateLimiter::direct(Quota::per_minute(quota)));
            }
        }
        loop {
            if shutdown.try_recv().is_ok() {
                info!("shutting down database writer");
                break;
            }
            // call blocking read on channel
            let next_event = event_rx.blocking_recv();
            // if the channel has closed, we will never get work
            if next_event.is_none() {
                break;
            }
            // track if an event write occurred; this is used to
            // update the rate limiter
            let mut event_write = false;
            let subm_event = next_event.unwrap();
            let event = subm_event.event;
            let notice_tx = subm_event.notice_tx;
            // check if this event is authorized.
            if let Some(allowed_addrs) = whitelist {
                // TODO: incorporate delegated pubkeys
                // if the event address is not in allowed_addrs.
                if !allowed_addrs.contains(&event.pubkey) {
                    info!(
                        "Rejecting event {}, unauthorized author",
                        event.get_event_id_prefix()
                    );
                    notice_tx
                        .try_send(Notice::blocked(
                            event.id,
                            "pubkey is not allowed to publish to this relay",
                        ))
                        .ok();
                    continue;
                }
            }

            // send any metadata events to the NIP-05 verifier
            if nip05_active && event.is_kind_metadata() {
                // we are sending this prior to even deciding if we
                // persist it.  this allows the nip05 module to
                // inspect it, update if necessary, or persist a new
                // event and broadcast it itself.
                metadata_tx.send(event.clone()).ok();
            }

            // check for  NIP-05 verification
            if nip05_enabled {
                match nip05::query_latest_user_verification(pool.get()?, event.pubkey.to_owned()) {
                    Ok(uv) => {
                        if uv.is_valid(&settings.verified_users) {
                            info!(
                                "new event from verified author ({:?},{:?})",
                                uv.name.to_string(),
                                event.get_author_prefix()
                            );
                        } else {
                            info!("rejecting event, author ({:?} / {:?}) verification invalid (expired/wrong domain)",
                                  uv.name.to_string(),
                                  event.get_author_prefix()
                            );
                            notice_tx
                                .try_send(Notice::blocked(
                                    event.id,
                                    "NIP-05 verification is no longer valid (expired/wrong domain)",
                                ))
                                .ok();
                            continue;
                        }
                    }
                    Err(Error::SqlError(rusqlite::Error::QueryReturnedNoRows)) => {
                        debug!(
                            "no verification records found for pubkey: {:?}",
                            event.get_author_prefix()
                        );
                        notice_tx
                            .try_send(Notice::blocked(
                                event.id,
                                "NIP-05 verification needed to publish events",
                            ))
                            .ok();
                        continue;
                    }
                    Err(e) => {
                        warn!("checking nip05 verification status failed: {:?}", e);
                        continue;
                    }
                }
            }
            // TODO: cache recent list of authors to remove a DB call.
            let start = Instant::now();
            if event.kind >= 20000 && event.kind < 30000 {
                bcast_tx.send(event.clone()).ok();
                info!(
                    "published ephemeral event: {:?} from: {:?} in: {:?}",
                    event.get_event_id_prefix(),
                    event.get_author_prefix(),
                    start.elapsed()
                );
                event_write = true
            } else {
                let mut conn = pool.get()?;
                let mut sdb = SqliteRepo::new(&mut conn);
                match sdb.write_event(&event) {
                    Ok(updated) => {
                        if updated == 0 {
                            trace!("ignoring duplicate or deleted event");
                            notice_tx.try_send(Notice::duplicate(event.id)).ok();
                        } else {
                            info!(
                                "persisted event: {:?} from: {:?} in: {:?}",
                                event.get_event_id_prefix(),
                                event.get_author_prefix(),
                                start.elapsed()
                            );
                            event_write = true;
                            // send this out to all clients
                            bcast_tx.send(event.clone()).ok();
                            notice_tx.try_send(Notice::saved(event.id)).ok();
                        }
                    }
                    Err(err) => {
                        warn!("event insert failed: {:?}", err);
                        let msg = "relay experienced an error trying to publish the latest event";
                        notice_tx.try_send(Notice::error(event.id, msg)).ok();
                    }
                }
                // Use this as a trigger to do optimization
                optimize_counter += 1;
                if optimize_counter > EVENT_COUNT_OPTIMIZE_TRIGGER {
                    info!("running database optimizer");
                    optimize_counter = 0;
                    optimize_db(&mut pool.get()?).ok();
                }
            }

            // use rate limit, if defined, and if an event was actually written.
            if event_write {
                if let Some(ref lim) = lim_opt {
                    if let Err(n) = lim.check() {
                        let wait_for = n.wait_time_from(clock.now());
                        // check if we have recently logged rate
                        // limits, but print out a message only once
                        // per second.
                        if most_recent_rate_limit.elapsed().as_secs() > 10 {
                            warn!(
                                "rate limit reached for event creation (sleep for {:?}) (suppressing future messages for 10 seconds)",
                                wait_for
                            );
                            // reset last rate limit message
                            most_recent_rate_limit = Instant::now();
                        }
                        // block event writes, allowing them to queue up
                        thread::sleep(wait_for);
                        continue;
                    }
                }
            }
        }
        info!("database connection closed");
        Ok(())
    })
}

/// Serialized event associated with a specific subscription request.
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct QueryResult {
    /// Subscription identifier
    pub sub_id: String,
    /// Serialized event
    pub event: String,
}

/// Produce a arbitrary list of '?' parameters.
fn repeat_vars(count: usize) -> String {
    if count == 0 {
        return "".to_owned();
    }
    let mut s = "?,".repeat(count);
    // Remove trailing comma
    s.pop();
    s
}

/// Create a dynamic SQL subquery and params from a subscription filter.
fn query_from_filter(f: &ReqFilter) -> (String, Vec<Box<dyn ToSql>>) {
    // build a dynamic SQL query.  all user-input is either an integer
    // (sqli-safe), or a string that is filtered to only contain
    // hexadecimal characters.  Strings that require escaping (tag
    // names/values) use parameters.

    // if the filter is malformed, don't return anything.
    if f.force_no_match {
        let empty_query = "SELECT e.content, e.created_at FROM event e WHERE 1=0".to_owned();
        // query parameters for SQLite
        let empty_params: Vec<Box<dyn ToSql>> = vec![];
        return (empty_query, empty_params);
    }

    let mut query = "SELECT e.content, e.created_at FROM event e".to_owned();
    // query parameters for SQLite
    let mut params: Vec<Box<dyn ToSql>> = vec![];

    // individual filter components (single conditions such as an author or event ID)
    let mut filter_components: Vec<String> = Vec::new();
    // Query for "authors", allowing prefix matches
    if let Some(authvec) = &f.authors {
        // take each author and convert to a hexsearch
        let mut auth_searches: Vec<String> = vec![];
        for auth in authvec {
            match hex_range(auth) {
                Some(HexSearch::Exact(ex)) => {
                    auth_searches.push("author=? OR delegated_by=?".to_owned());
                    params.push(Box::new(ex.clone()));
                    params.push(Box::new(ex));
                }
                Some(HexSearch::Range(lower, upper)) => {
                    auth_searches.push(
                        "(author>? AND author<?) OR (delegated_by>? AND delegated_by<?)".to_owned(),
                    );
                    params.push(Box::new(lower.clone()));
                    params.push(Box::new(upper.clone()));
                    params.push(Box::new(lower));
                    params.push(Box::new(upper));
                }
                Some(HexSearch::LowerOnly(lower)) => {
                    auth_searches.push("author>? OR delegated_by>?".to_owned());
                    params.push(Box::new(lower.clone()));
                    params.push(Box::new(lower));
                }
                None => {
                    info!("Could not parse hex range from author {:?}", auth);
                }
            }
        }
        if !authvec.is_empty() {
            let authors_clause = format!("({})", auth_searches.join(" OR "));
            filter_components.push(authors_clause);
        } else {
            // if the authors list was empty, we should never return
            // any results.
            filter_components.push("false".to_owned());
        }
    }
    // Query for Kind
    if let Some(ks) = &f.kinds {
        // kind is number, no escaping needed
        let str_kinds: Vec<String> = ks.iter().map(|x| x.to_string()).collect();
        let kind_clause = format!("kind IN ({})", str_kinds.join(", "));
        filter_components.push(kind_clause);
    }
    // Query for event, allowing prefix matches
    if let Some(idvec) = &f.ids {
        // take each author and convert to a hexsearch
        let mut id_searches: Vec<String> = vec![];
        for id in idvec {
            match hex_range(id) {
                Some(HexSearch::Exact(ex)) => {
                    id_searches.push("event_hash=?".to_owned());
                    params.push(Box::new(ex));
                }
                Some(HexSearch::Range(lower, upper)) => {
                    id_searches.push("(event_hash>? AND event_hash<?)".to_owned());
                    params.push(Box::new(lower));
                    params.push(Box::new(upper));
                }
                Some(HexSearch::LowerOnly(lower)) => {
                    id_searches.push("event_hash>?".to_owned());
                    params.push(Box::new(lower));
                }
                None => {
                    info!("Could not parse hex range from id {:?}", id);
                }
            }
        }
        if !idvec.is_empty() {
            let id_clause = format!("({})", id_searches.join(" OR "));
            filter_components.push(id_clause);
        } else {
            // if the ids list was empty, we should never return
            // any results.
            filter_components.push("false".to_owned());
        }
    }
    // Query for tags
    if let Some(map) = &f.tags {
        for (key, val) in map.iter() {
            let mut str_vals: Vec<Box<dyn ToSql>> = vec![];
            let mut blob_vals: Vec<Box<dyn ToSql>> = vec![];
            for v in val {
                if (v.len() % 2 == 0) && is_lower_hex(v) {
                    if let Ok(h) = hex::decode(v) {
                        blob_vals.push(Box::new(h));
                    }
                } else {
                    str_vals.push(Box::new(v.to_owned()));
                }
            }
            // create clauses with "?" params for each tag value being searched
            let str_clause = format!("value IN ({})", repeat_vars(str_vals.len()));
            let blob_clause = format!("value_hex IN ({})", repeat_vars(blob_vals.len()));
            // find evidence of the target tag name/value existing for this event.
            let tag_clause = format!("e.id IN (SELECT e.id FROM event e LEFT JOIN tag t on e.id=t.event_id WHERE hidden!=TRUE and (name=? AND ({} OR {})))", str_clause, blob_clause);
            // add the tag name as the first parameter
            params.push(Box::new(key.to_string()));
            // add all tag values that are plain strings as params
            params.append(&mut str_vals);
            // add all tag values that are blobs as params
            params.append(&mut blob_vals);
            filter_components.push(tag_clause);
        }
    }
    // Query for timestamp
    if f.since.is_some() {
        let created_clause = format!("created_at > {}", f.since.unwrap());
        filter_components.push(created_clause);
    }
    // Query for timestamp
    if f.until.is_some() {
        let until_clause = format!("created_at < {}", f.until.unwrap());
        filter_components.push(until_clause);
    }
    // never display hidden events
    query.push_str(" WHERE hidden!=TRUE");
    // build filter component conditions
    if !filter_components.is_empty() {
        query.push_str(" AND ");
        query.push_str(&filter_components.join(" AND "));
    }
    // Apply per-filter limit to this subquery.
    // The use of a LIMIT implies a DESC order, to capture only the most recent events.
    if let Some(lim) = f.limit {
        let _ = write!(query, " ORDER BY e.created_at DESC LIMIT {}", lim);
    } else {
        query.push_str(" ORDER BY e.created_at ASC")
    }
    (query, params)
}

/// Create a dynamic SQL query string and params from a subscription.
fn query_from_sub(sub: &Subscription) -> (String, Vec<Box<dyn ToSql>>) {
    // build a dynamic SQL query for an entire subscription, based on
    // SQL subqueries for filters.
    let mut subqueries: Vec<String> = Vec::new();
    // subquery params
    let mut params: Vec<Box<dyn ToSql>> = vec![];
    // for every filter in the subscription, generate a subquery
    for f in sub.filters.iter() {
        let (f_subquery, mut f_params) = query_from_filter(f);
        subqueries.push(f_subquery);
        params.append(&mut f_params);
    }
    // encapsulate subqueries into select statements
    let subqueries_selects: Vec<String> = subqueries
        .iter()
        .map(|s| format!("SELECT distinct content, created_at FROM ({})", s))
        .collect();
    let query: String = subqueries_selects.join(" UNION ");
    (query, params)
}

fn log_pool_stats(pool: &SqlitePool) {
    let state: r2d2::State = pool.state();
    let in_use_cxns = state.connections - state.idle_connections;
    debug!(
        "DB pool usage (in_use: {}, available: {})",
        in_use_cxns, state.connections
    );
}

/// Perform a database query using a subscription.
///
/// The [`Subscription`] is converted into a SQL query.  Each result
/// is published on the `query_tx` channel as it is returned.  If a
/// message becomes available on the `abandon_query_rx` channel, the
/// query is immediately aborted.
pub async fn db_query(
    sub: Subscription,
    client_id: String,
    pool: SqlitePool,
    query_tx: tokio::sync::mpsc::Sender<QueryResult>,
    mut abandon_query_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let pre_spawn_start = Instant::now();
    task::spawn_blocking(move || {
        let db_queue_time = pre_spawn_start.elapsed();
        // report queuing time if it is slow
        if db_queue_time > Duration::from_secs(1) {
            debug!(
                "(slow) DB query queued for {:?} (cid: {}, sub: {:?})",
                db_queue_time, client_id, sub.id
            );
        }
        let start = Instant::now();
        let mut row_count: usize = 0;
        // generate SQL query
        let (q, p) = query_from_sub(&sub);
        debug!("SQL generated in {:?}", start.elapsed());
        // show pool stats
        log_pool_stats(&pool);
        // cutoff for displaying slow queries
        let slow_cutoff = Duration::from_millis(2000);
        // any client that doesn't cause us to generate new rows in 5
        // seconds gets dropped.
        let abort_cutoff = Duration::from_secs(5);
        let start = Instant::now();
        let mut slow_first_event;
        let mut last_successful_send = Instant::now();
        if let Ok(conn) = pool.get() {
            // execute the query. Don't cache, since queries vary so much.
            let mut stmt = conn.prepare(&q)?;
            let mut event_rows = stmt.query(rusqlite::params_from_iter(p))?;
            let mut first_result = true;
            while let Some(row) = event_rows.next()? {
                let first_event_elapsed = start.elapsed();
                slow_first_event = first_event_elapsed >= slow_cutoff;
                if first_result {
                    debug!(
                        "first result in {:?} (cid: {}, sub: {:?})",
                        first_event_elapsed, client_id, sub.id
                    );
                    first_result = false;
                }
                // logging for slow queries; show sub and SQL.
                // to reduce logging; only show 1/16th of clients (leading 0)
                if slow_first_event && client_id.starts_with("00") {
                    debug!(
                        "query req (slow): {:?} (cid: {}, sub: {:?})",
                        sub, client_id, sub.id
                    );
                    debug!(
                        "query string (slow): {} (cid: {}, sub: {:?})",
                        q, client_id, sub.id
                    );
                } else {
                    trace!(
                        "query req: {:?} (cid: {}, sub: {:?})",
                        sub,
                        client_id,
                        sub.id
                    );
                    trace!(
                        "query string: {} (cid: {}, sub: {:?})",
                        q,
                        client_id,
                        sub.id
                    );
                }
                // check if this is still active; every 100 rows
                if row_count % 100 == 0 && abandon_query_rx.try_recv().is_ok() {
                    debug!("query aborted (cid: {}, sub: {:?})", client_id, sub.id);
                    return Ok(());
                }
                row_count += 1;
                let event_json = row.get(0)?;
                loop {
                    if query_tx.capacity() != 0 {
                        // we have capacity to add another item
                        break;
                    } else {
                        // the queue is full
                        trace!("db reader thread is stalled");
                        if last_successful_send + abort_cutoff < Instant::now() {
                            // the queue has been full for too long, abort
                            info!("aborting database query due to slow client");
                            let ok: Result<()> = Ok(());
                            return ok;
                        }
                        // give the queue a chance to clear before trying again
                        thread::sleep(Duration::from_millis(100));
                    }
                }
                // TODO: we could use try_send, but we'd have to juggle
                // getting the query result back as part of the error
                // result.
                query_tx
                    .blocking_send(QueryResult {
                        sub_id: sub.get_id(),
                        event: event_json,
                    })
                    .ok();
                last_successful_send = Instant::now();
            }
            query_tx
                .blocking_send(QueryResult {
                    sub_id: sub.get_id(),
                    event: "EOSE".to_string(),
                })
                .ok();
            debug!(
                "query completed in {:?} (cid: {}, sub: {:?}, db_time: {:?}, rows: {})",
                pre_spawn_start.elapsed(),
                client_id,
                sub.id,
                start.elapsed(),
                row_count
            );
        } else {
            warn!("Could not get a database connection for querying");
        }
        let ok: Result<()> = Ok(());
        ok
    });
}
