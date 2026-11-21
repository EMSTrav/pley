//! SQLite access. Writes go through a single-connection pool so they never
//! contend for the write lock; reads use a separate read-only pool (WAL mode
//! lets them run concurrently with the writer).
//!
//! Whoever holds the writer holds up every other write (progress reports,
//! logins, settings), and those fail after the 60 s acquire timeout: work out
//! what to write first (reading through `read`, computing in
//! `spawn_blocking`), then write it in short transactions, as the scanner
//! does.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous};

/// The schema, brought up to date when the database is opened.
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct Db {
    pub read: SqlitePool,
    pub write: SqlitePool,
    /// What search keeps in memory of the library (see [`crate::search`]).
    pub search: Arc<crate::search::Index>,
}

impl Db {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let base = SqliteConnectOptions::from_str("sqlite://")?
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(30))
            .foreign_keys(true)
            .pragma("cache_size", "-32768")
            .pragma("mmap_size", "268435456")
            .pragma("temp_store", "memory");

        let write = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(60))
            .connect_with(base.clone())
            .await?;
        backup_before_upgrade(&write, path).await?;
        MIGRATOR.run(&write).await.map_err(explain_migration)?;

        let readers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(4, 16) as u32;
        let read = SqlitePoolOptions::new()
            .max_connections(readers)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(base.read_only(true))
            .await?;
        Ok(Self { read, write, search: Arc::default() })
    }

    /// Refresh query planner statistics (cheap; run after large scans).
    pub async fn optimize(&self) {
        let _ = sqlx::query("PRAGMA optimize").execute(&self.write).await;
    }

    pub async fn close(&self) {
        self.read.close().await;
        self.write.close().await;
    }
}

/// What to do when the database's schema isn't one this version made or
/// knows. That happens to a database another build of Pley upgraded, a test
/// build of a change not yet released, say, whose migrations were numbered
/// or written otherwise than the ones that shipped: no released version
/// changes a migration that shipped (`shipped_migrations_never_change`).
fn explain_migration(err: sqlx::migrate::MigrateError) -> anyhow::Error {
    use sqlx::migrate::MigrateError;
    let what = match &err {
        MigrateError::VersionMismatch(version) => {
            format!(
                "its migration {version} isn't this version's: another build of Pley, a test build say, upgraded it"
            )
        }
        MigrateError::VersionMissing(version) => {
            format!("its migration {version} is unknown to this version: a newer or a test build of Pley upgraded it")
        }
        _ => return err.into(),
    };
    anyhow::anyhow!(
        "the database can't be upgraded: {what}. Start the build that upgraded it, or put back the copy made \
         before that upgrade (in the backups folder, its name ending in {}.db) ({err})",
        crate::backup::BEFORE_UPGRADE
    )
}

/// Copies an existing database to `backups/` before migrations change it:
/// nothing else could undo a migration that went wrong. pley doesn't upgrade
/// a database it could not copy.
async fn backup_before_upgrade(write: &SqlitePool, path: &Path) -> anyhow::Result<()> {
    let migrated: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations')",
    )
    .fetch_one(write)
    .await?;
    if !migrated {
        // A new database: nothing to keep.
        return Ok(());
    }
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success").fetch_all(write).await?;
    let pending = MIGRATOR.iter().any(|m| m.migration_type.is_up_migration() && !applied.contains(&m.version));
    if applied.is_empty() || !pending {
        return Ok(());
    }
    let dir = path.parent().unwrap_or(Path::new("")).join("backups");
    // Logged by write_checked; the name says why it was made.
    crate::backup::write_checked(write, &dir, crate::backup::BEFORE_UPGRADE)
        .await
        .context("could not back up the database before upgrading it")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::ConnectOptions;

    use super::*;

    async fn versions(path: &Path) -> Vec<i64> {
        let mut conn = SqliteConnectOptions::new().filename(path).read_only(true).connect().await.unwrap();
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version").fetch_all(&mut conn).await.unwrap()
    }

    /// Every migration on master, and the start of its SHA-384. A migration
    /// that has been on master ships in the next release, and a server that
    /// ran it refuses to start on one whose file says anything else, a
    /// comment included: a change is a new migration, never an edit.
    const SHIPPED: &[(&str, &str)] = &[
        ("0001_init.sql", "fa10fe09d7bdf5a8"),
        ("0002_stream_preferences.sql", "739e261f0cc7df35"),
        ("0003_markers.sql", "516dbf2a21c9d5ac"),
        ("0004_accounts.sql", "6c8dfb7d8b12d8c0"),
        ("0005_collections_playlists.sql", "15a13fcf45679b1b"),
        ("0006_item_overrides.sql", "5e4ea0013300787c"),
        ("0007_people.sql", "15a06c7d14e861d4"),
        ("0008_play_history.sql", "b8653da2d53ef82e"),
        ("0009_user_ratings.sql", "b9ed6c3ccceec01b"),
        ("0010_storage.sql", "81807b2bbe700b62"),
        ("0011_extras.sql", "a7becd248bbe7a68"),
        ("0012_metadata_providers.sql", "59655ceb72a9a835"),
        ("0013_query_indexes.sql", "9f6b93dd4eb61698"),
        ("0014_track_defaults.sql", "763939d3be15bb68"),
        ("0015_labels.sql", "883b018ea7cfa6ab"),
        ("0016_smart_collections.sql", "1758a15eb0325ef3"),
        ("0017_smart_playlists.sql", "6583a92796cc386a"),
        ("0018_sso.sql", "a01f0305f9645d9f"),
        ("0019_two_step.sql", "67f103e0ac75a3f9"),
        ("0020_library_order.sql", "ab7305fb2dec77a8"),
        ("0021_library_ids.sql", "4bf528b32b0b9c65"),
        ("0022_no_single_sign_on.sql", "cf568f92ef8b3a5c"),
        ("0023_apps.sql", "420ea4e9f3c9c54f"),
        ("0024_decode_order_times.sql", "43c1ade68192e0e2"),
        ("0025_my_list.sql", "762ac3ef5726fe79"),
        ("0026_lasting_ids.sql", "b83550c2ccfdad90"),
        ("0027_own_library_order.sql", "2aacc42144009683"),
        ("0028_hdr10plus_and_dv_layers.sql", "78abfffe700123eb"),
        ("0029_lasting_grant_ids.sql", "22d1bf83a4470e6f"),
        ("0030_playback_networks.sql", "22f9dc7594825bab"),
        ("0031_added_subtitles.sql", "632c02d31126fb97"),
        ("0032_live_tv.sql", "3fe97ac7fe512ee4"),
        ("0033_live_offline.sql", "ae7b13deda0a391c"),
        ("0034_live_source_uid.sql", "56fd28ea94f655e2"),
        ("0035_apps_moved.sql", "e96729b66038e743"),
        ("0036_requests.sql", "f2dbecaf7ecbe9f7"),
        ("0037_request_followers.sql", "eba20827624ffbe5"),
        ("0038_requests_unused.sql", "28e699d6bd7c02b2"),
        ("0039_request_sends.sql", "647f3f380338f7e8"),
        ("0040_file_dynamic_range.sql", "c6b944f2dbf6694a"),
        ("0041_compatibility_tokens.sql", "1e986a941ff3bd6d"),
        ("0042_compatibility_token_origins.sql", "a968fdf0d2b3e754"),
        ("0043_collection_labels.sql", "d65c9ac1ca791836"),
        ("0044_dolby_vision_in_frames.sql", "c55d8967bf522e11"),
        ("0045_item_nfo.sql", "18baddd841fa8fa1"),
        ("0046_user_language.sql", "49cff4bdf3f1c0a7"),
        ("0047_rating_country.sql", "9894a35a55c042af"),
        ("0048_age_limit_system.sql", "2b62ea12ad360d0d"),
        ("0049_birth_month.sql", "e5ed9e75c0fe2a8f"),
        ("0050_label_rules.sql", "f1448d0f575c5bff"),
        ("0051_import_accounts.sql", "63b687195e871dfc"),
        ("0052_labels_pending.sql", "47d6f960fef626bc"),
        ("0053_airplay_pairings.sql", "5c3a1af2a34e36eb"),
        ("0054_grants_by_cast.sql", "21b034ff2e65cb58"),
        ("0055_audiobooks.sql", "05df403b96411fde"),
        ("0056_audiobook_details.sql", "621a0692b705e942"),
        ("0057_collection_categories.sql", "987d745fdb9e478d"),
        ("0058_other_category.sql", "86e77284a66b588f"),
        ("0059_strm_files.sql", "a27b258e2fb87444"),
        ("0060_box_sets_category.sql", "eea834126080318f"),
        ("0061_tmdb_collections_switch.sql", "0990e98956afb58e"),
        ("0062_book_metadata.sql", "2814d1f94ba73ffb"),
        ("0063_collection_artwork.sql", "e9d2cd80c77bb271"),
        ("0064_item_aliases.sql", "0f075a2251edeba2"),
    ];

    #[test]
    fn shipped_migrations_never_change() {
        let files: Vec<(String, String)> = MIGRATOR
            .iter()
            .map(|m| {
                let hex: String = m.checksum.iter().map(|b| format!("{b:02x}")).collect();
                (format!("{:04}_{}.sql", m.version, m.description.replace(' ', "_")), hex)
            })
            .collect();
        for (i, (name, start)) in SHIPPED.iter().enumerate() {
            let Some((_, hex)) = files.iter().find(|(file, _)| file == name) else {
                panic!("{name} shipped and can't go: servers that ran it refuse to start without it");
            };
            assert!(
                hex.starts_with(start),
                "{name} shipped and can't change, not even a comment: servers that ran it refuse to start on \
                 another. Put it back, and make the change a new migration"
            );
            assert_eq!(files[i].0, *name, "a new migration goes after the ones that shipped");
        }
        // A new migration is added to SHIPPED as it goes to master, with the
        // line this prints.
        if let Some((name, hex)) = files.get(SHIPPED.len()) {
            panic!(
                "{name} is new: add it to SHIPPED as it goes to master, numbered after master's last:\n        \
                 (\"{name}\", \"{}\"),",
                &hex[..16]
            );
        }
    }

    #[tokio::test]
    async fn a_database_another_build_changed_says_what_to_do() {
        let dir = crate::testing::tempdir();
        let path = dir.path().join("pley.db");
        Db::open(&path).await.unwrap().close().await;
        // As a test build would leave it: its own migration 62 run.
        let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&path)).await.unwrap();
        sqlx::query("UPDATE _sqlx_migrations SET checksum = x'00' WHERE version = 62").execute(&pool).await.unwrap();
        pool.close().await;
        let err = format!("{:#}", Db::open(&path).await.err().expect("refused"));
        assert!(err.contains("migration 62 isn't this version's") && err.contains("-before-upgrade.db"), "{err}");

        // Or one this version doesn't have.
        let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&path)).await.unwrap();
        let shipped = MIGRATOR.iter().find(|m| m.version == 62).unwrap().checksum.to_vec();
        sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 62")
            .bind(shipped)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (9999, 'test', 1, x'00', 0)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let err = format!("{:#}", Db::open(&path).await.err().expect("refused"));
        assert!(err.contains("migration 9999 is unknown to this version"), "{err}");
    }

    #[tokio::test]
    async fn databases_are_backed_up_before_an_upgrade_and_only_then() {
        let dir = crate::testing::tempdir();
        let path = dir.path().join("pley.db");
        let backups = dir.path().join("backups");
        // A database made by an older version: every migration but the last.
        let older = dir.path().join("older");
        std::fs::create_dir(&older).unwrap();
        let mut files: Vec<_> = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        files.sort();
        for file in &files[..files.len() - 1] {
            std::fs::copy(file, older.join(file.file_name().unwrap())).unwrap();
        }
        let options = SqliteConnectOptions::new().filename(&path).create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        Migrator::new(older.as_path()).await.unwrap().run(&pool).await.unwrap();
        sqlx::query("INSERT INTO settings (key, value) VALUES ('probe', 'before')").execute(&pool).await.unwrap();
        pool.close().await;

        let db = Db::open(&path).await.unwrap();
        let latest: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        assert_eq!(versions(&path).await, latest, "upgraded");
        let copies: Vec<_> = std::fs::read_dir(&backups).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(copies.len(), 1, "{copies:?}");
        assert!(copies[0].to_string_lossy().ends_with("-before-upgrade.db"), "{copies:?}");
        assert_eq!(versions(&copies[0]).await, latest[..latest.len() - 1], "the copy is the old database");
        let mut copy = SqliteConnectOptions::new().filename(&copies[0]).read_only(true).connect().await.unwrap();
        let probe: String =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'probe'").fetch_one(&mut copy).await.unwrap();
        assert_eq!(probe, "before");
        db.close().await;

        // Up to date: no copy. A new database: none either.
        Db::open(&path).await.unwrap().close().await;
        Db::open(&dir.path().join("new.db")).await.unwrap().close().await;
        assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 1);
    }

    /// A database at `path` as an older version left it: only the
    /// migrations numbered below `version`.
    async fn database_before(version: i64, path: &Path) -> SqlitePool {
        let older = path.with_extension("migrations");
        std::fs::create_dir(&older).unwrap();
        for file in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations")).unwrap().flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            let number: i64 = name.split('_').next().and_then(|n| n.parse().ok()).expect(&name);
            if number < version {
                std::fs::copy(file.path(), older.join(&name)).unwrap();
            }
        }
        let options = SqliteConnectOptions::new().filename(path).create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        Migrator::new(older.as_path()).await.unwrap().run(&pool).await.unwrap();
        pool
    }

    /// Audiobook libraries come with a table of libraries made again: what
    /// every library holds stays, and so does a library's id after it is
    /// deleted (`library_ids`).
    #[tokio::test]
    async fn libraries_and_what_they_hold_stay_through_the_audiobooks_upgrade() {
        let dir = crate::testing::tempdir();
        let path = dir.path().join("pley.db");
        let pool = database_before(55, &path).await;
        sqlx::query("PRAGMA foreign_keys = ON").execute(&pool).await.unwrap();
        for sql in [
            "INSERT INTO libraries (id, name, kind, paths, created_at, scanned_at, position, labels_pending)
             VALUES (3, 'Music', 'music', '[\"/m\"]', 10, 20, 1, 0), (5, 'Films', 'movies', '[]', 11, NULL, 0, 1)",
            "INSERT INTO items (id, library_id, kind, key, title, sort_title, added_at, updated_at)
             VALUES (7, 5, 'movie', 'movie:a film:2001', 'A Film', 'film', 0, 0)",
            "INSERT INTO media_files (id, item_id, library_id, path, size, mtime, added_at, updated_at)
             VALUES (9, 7, 5, '/films/A Film.mkv', 1, 0, 0, 0)",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&pool).await.unwrap();
        }
        pool.close().await;

        let db = Db::open(&path).await.unwrap();
        // A book's writers, narrators and series have their columns.
        sqlx::query("SELECT authors, narrators, series, series_index FROM items").fetch_all(&db.read).await.unwrap();
        type Library = (i64, String, String, String, i64, Option<i64>, i64, bool);
        let libraries: Vec<Library> = sqlx::query_as(
            "SELECT id, name, kind, paths, created_at, scanned_at, position, labels_pending FROM libraries ORDER BY id",
        )
        .fetch_all(&db.read)
        .await
        .unwrap();
        assert_eq!(
            libraries,
            [
                (3, "Music".into(), "music".into(), r#"["/m"]"#.into(), 10, Some(20), 1, false),
                (5, "Films".into(), "movies".into(), "[]".into(), 11, None, 0, true),
            ]
        );
        let held: (i64, i64) =
            sqlx::query_as("SELECT (SELECT COUNT(*) FROM items), (SELECT COUNT(*) FROM media_files)")
                .fetch_one(&db.read)
                .await
                .unwrap();
        assert_eq!(held, (1, 1), "nothing went with the old table");
        let broken: Vec<(String,)> = sqlx::query_as("PRAGMA foreign_key_check").fetch_all(&db.write).await.unwrap();
        assert!(broken.is_empty(), "{broken:?}");

        // The new kind is taken, an unknown one still isn't, and the
        // libraries' foreign keys still hold: a deleted library takes what
        // it held, and its id isn't given again.
        sqlx::query("INSERT INTO libraries (name, kind, created_at) VALUES ('Books', 'audiobooks', 0)")
            .execute(&db.write)
            .await
            .unwrap();
        assert!(
            sqlx::query("INSERT INTO libraries (name, kind, created_at) VALUES ('Pics', 'photos', 0)")
                .execute(&db.write)
                .await
                .is_err()
        );
        sqlx::query("DELETE FROM libraries WHERE id IN (5, 6)").execute(&db.write).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items").fetch_one(&db.read).await.unwrap();
        assert_eq!(left, 0);
        let last: i64 = sqlx::query_scalar("SELECT last FROM library_ids").fetch_one(&db.read).await.unwrap();
        assert_eq!(last, 6);
        db.close().await;
    }

    /// The audiobooks migration runs outside sqlx's transaction, which
    /// records it only after it ran: a server stopped in between runs it
    /// again as it starts, and starts.
    #[tokio::test]
    async fn the_audiobooks_upgrade_runs_again_after_a_crash() {
        let dir = crate::testing::tempdir();
        let path = dir.path().join("pley.db");
        let pool = database_before(56, &path).await;
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 55").execute(&pool).await.unwrap();
        sqlx::query("PRAGMA foreign_keys = ON").execute(&pool).await.unwrap();
        for sql in [
            "INSERT INTO libraries (id, name, kind, created_at) VALUES (4, 'Books', 'audiobooks', 0)",
            "INSERT INTO items (id, library_id, kind, key, title, sort_title, added_at, updated_at)
             VALUES (8, 4, 'audiobook', 'audiobook:a book', 'A Book', 'book', 0, 0)",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&pool).await.unwrap();
        }
        pool.close().await;

        let db = Db::open(&path).await.unwrap();
        let latest: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        assert_eq!(versions(&path).await, latest);
        let held: (i64, i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM libraries), (SELECT COUNT(*) FROM items)")
            .fetch_one(&db.read)
            .await
            .unwrap();
        assert_eq!(held, (1, 1));
        sqlx::query("UPDATE items SET authors = '[\"A Writer\"]' WHERE id = 8").execute(&db.write).await.unwrap();
        db.close().await;
    }

    async fn server_settings(db: &Db) -> String {
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'server'").fetch_one(&db.read).await.unwrap()
    }

    #[tokio::test]
    async fn single_sign_on_leaves_nothing_behind_after_an_upgrade() {
        let dir = crate::testing::tempdir();
        let path = dir.path().join("pley.db");
        // A version with single sign-on: an account it made, linked to an
        // identity at the provider, and its settings with the client secret.
        let pool = database_before(22, &path).await;
        sqlx::query("INSERT INTO users (id, username, password_hash, created_at) VALUES (1, 'ann', '!sso', 0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO user_identities (issuer, subject, user_id, email, created_at)
             VALUES ('https://idp', 'ann-at-idp', 1, 'ann@example.com', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let settings = serde_json::json!({
            "server_name": "Attic",
            "public_url": "https://p",
            "sso_enabled": true,
            "sso_name": "Idp",
            "sso_issuer": "https://idp",
            "sso_client_id": "pley",
            "sso_client_secret": "s3cret",
            "sso_new_accounts": true,
            "sso_group": "pley-users",
        });
        sqlx::query("INSERT INTO settings (key, value) VALUES ('server', ?)")
            .bind(settings.to_string())
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let db = Db::open(&path).await.unwrap();
        let left: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE name IN ('user_identities', 'user_identities_user')",
        )
        .fetch_all(&db.read)
        .await
        .unwrap();
        assert!(left.is_empty(), "{left:?}");
        let value = server_settings(&db).await;
        let kept: serde_json::Value = serde_json::from_str(&value).unwrap();
        assert_eq!(kept, serde_json::json!({"server_name": "Attic"}), "the other settings stay");
        assert!(!value.contains("s3cret"), "{value}");
        // The account stays, without a password until an administrator gives it one.
        let hash: String =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE id = 1").fetch_one(&db.read).await.unwrap();
        assert_eq!(hash, "!sso");
        db.close().await;

        // Settings that aren't JSON don't stop the upgrade, and stay as they were.
        let path = dir.path().join("unreadable.db");
        let pool = database_before(22, &path).await;
        sqlx::query("INSERT INTO settings (key, value) VALUES ('server', 'not json')").execute(&pool).await.unwrap();
        pool.close().await;
        let db = Db::open(&path).await.unwrap();
        assert_eq!(server_settings(&db).await, "not json");
        db.close().await;
    }

    #[tokio::test]
    async fn the_apps_sign_ins_move_with_their_client_ids() {
        let dir = crate::testing::tempdir();
        let path = dir.path().join("pley.db");
        let pool = database_before(35, &path).await;
        sqlx::query("INSERT INTO users (id, username, password_hash, created_at) VALUES (1, 'ann', '!', 0)")
            .execute(&pool)
            .await
            .unwrap();
        for (id, client) in [
            (1, "https://hhaidar.github.io/pley/oauth/android.json"),
            (2, "https://hhaidar.github.io/pley/oauth/webos.json"),
            (3, "https://apps.example.com/pley.json"),
        ] {
            sqlx::query(
                "INSERT INTO app_grants (id, user_id, approved_by, client_id, scope, created_at, last_used_at,
                 expires_at) VALUES (?, 1, 1, ?, 'library', 0, 0, 0)",
            )
            .bind(id)
            .bind(client)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool.close().await;

        let db = Db::open(&path).await.unwrap();
        let clients: Vec<String> =
            sqlx::query_scalar("SELECT client_id FROM app_grants ORDER BY id").fetch_all(&db.read).await.unwrap();
        assert_eq!(
            clients,
            [crate::oauth::clients::ANDROID_ID, crate::oauth::clients::WEBOS_ID, "https://apps.example.com/pley.json"]
        );
        db.close().await;
    }

    /// Probes now find Dolby Vision in the frames of 10-bit HEVC whose file
    /// doesn't name it: an upgrade has those files probed again, and only
    /// those.
    #[tokio::test]
    async fn hevc_that_may_hide_dolby_vision_is_probed_again() {
        let dir = crate::testing::tempdir();
        let path = dir.path().join("pley.db");
        let pool = database_before(44, &path).await;
        sqlx::query("INSERT INTO libraries (id, name, kind, created_at) VALUES (1, 'Movies', 'movies', 0)")
            .execute(&pool)
            .await
            .unwrap();
        let video = |codec: &str, range: Option<&str>, bits: Option<i64>| {
            serde_json::json!({"streams": [{"index": 0, "kind": "video", "codec": codec,
                "dynamic_range": range, "bit_depth": bits}]})
            .to_string()
        };
        let files = [
            ("hdr10", Some(video("hevc", Some("hdr10"), Some(10)))),
            ("hdr10plus", Some(video("hevc", Some("hdr10plus"), Some(10)))),
            ("hlg", Some(video("hevc", Some("hlg"), Some(10)))),
            ("sdr 10-bit", Some(video("hevc", Some("sdr"), Some(10)))),
            ("no range or depth", Some(video("hevc", None, None))),
            ("named", Some(video("hevc", Some("dolby_vision"), Some(10)))),
            ("8-bit", Some(video("hevc", Some("sdr"), Some(8)))),
            ("avc", Some(video("h264", Some("sdr"), Some(8)))),
            ("unreadable", None),
        ];
        for (id, (name, info)) in files.iter().enumerate() {
            sqlx::query(
                "INSERT INTO items (id, library_id, kind, key, title, sort_title, added_at, updated_at)
                 VALUES (?, 1, 'movie', ?, ?, ?, 0, 0)",
            )
            .bind(id as i64)
            .bind(name)
            .bind(name)
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO media_files (item_id, library_id, path, size, mtime, media_info, probed, added_at, updated_at)
                 VALUES (?, 1, ?, 1, 0, ?, 1, 0, 0)",
            )
            .bind(id as i64)
            .bind(format!("/m/{name}.mkv"))
            .bind(info)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool.close().await;

        let db = Db::open(&path).await.unwrap();
        let again: Vec<String> = sqlx::query_scalar("SELECT path FROM media_files WHERE probed = 0 ORDER BY id")
            .fetch_all(&db.read)
            .await
            .unwrap();
        assert_eq!(
            again,
            ["/m/hdr10.mkv", "/m/hdr10plus.mkv", "/m/hlg.mkv", "/m/sdr 10-bit.mkv", "/m/no range or depth.mkv"]
        );
        db.close().await;
    }

    /// Home's and Recommended's built-in rows become collections Pley makes
    /// in each movie and TV library, in the order Home showed the libraries;
    /// TMDB's collections are Box sets.
    #[tokio::test]
    async fn the_built_in_rows_become_collections() {
        let dir = crate::testing::tempdir();
        let path = dir.path().join("pley.db");
        let pool = database_before(57, &path).await;
        for sql in [
            "INSERT INTO libraries (id, name, kind, paths, created_at, position, labels_pending)
             VALUES (3, 'Music', 'music', '[]', 0, 0, 0), (5, 'Films', 'movies', '[]', 0, 1, 0),
                    (6, 'Shows', 'shows', '[]', 0, 1, 0)",
            "INSERT INTO collections (id, library_id, title, sort_title, tmdb_id, created_at, updated_at)
             VALUES (1, 5, 'Alien Collection', 'alien collection', 8091, 0, 0),
                    (2, 5, 'Mine', 'mine', NULL, 0, 0)",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&pool).await.unwrap();
        }
        pool.close().await;

        let db = Db::open(&path).await.unwrap();
        type Made = (i64, String, Option<String>, Option<String>, Option<i64>, Option<i64>);
        let made: Vec<Made> = sqlx::query_as(
            "SELECT c.library_id, c.title, c.builtin, k.key, c.home_position, c.recommended_position
             FROM collections c LEFT JOIN collection_categories k ON k.id = c.category_id
             ORDER BY c.library_id, c.recommended_position IS NULL, c.recommended_position, c.id",
        )
        .fetch_all(&db.read)
        .await
        .unwrap();
        let highlights = Some("highlights".to_owned());
        let row = |library: i64, title: &str, builtin: &str, home: Option<i64>, at: i64| {
            (library, title.to_owned(), Some(builtin.to_owned()), highlights.clone(), home, Some(at))
        };
        // Not a Recommended row: it would repeat Top Rated.
        let unwatched = |library: i64| {
            (library, "Top Unwatched".to_owned(), Some("top_unwatched".to_owned()), highlights.clone(), None, None)
        };
        assert_eq!(
            made,
            [
                row(5, "Recently Added", "recently_added", Some(1), 0),
                row(5, "Recently Released", "recently_released", None, 1),
                row(5, "Top Rated", "top_rated", None, 2),
                (5, "Alien Collection".into(), None, Some("box_sets".into()), None, None),
                (5, "Mine".into(), None, None, None, None),
                unwatched(5),
                row(6, "Recently Added Shows", "recently_added", Some(2), 0),
                row(6, "Recently Released", "recently_released", None, 1),
                row(6, "Top Rated", "top_rated", None, 2),
                unwatched(6),
            ]
        );
        let music: Option<i64> =
            sqlx::query_scalar("SELECT home_position FROM libraries WHERE id = 3").fetch_one(&db.read).await.unwrap();
        assert_eq!(music, Some(0));
        db.close().await;
    }
}
