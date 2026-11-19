//! Movies of a library matched to the same TMDB entry are one movie, with
//! each copy a version.
//!
//! The scanner tells movies apart by their files' names: their TMDB id
//! (`{tmdb-157336}`), else their title and year. When TMDB corrects a film's
//! year or title, a download manager names the copies it fetches after with
//! the new ones, and the copies would stay two movies that metadata matches
//! to the same title. [`same_movies`] makes them one: the files and bonus
//! material go to the movie added first (one keyed by its TMDB id before
//! it), with what people did with either (watched, ratings, My List,
//! playlists, collections, labels, hand edits, track choices). The keys the
//! other's files made are kept in `item_aliases`, so that a later scan of
//! them finds the movie they went into.
//!
//! [`split`] undoes it when someone fixes the movie's match: each file goes
//! back to the movie its own name makes, to be matched on its own.

use std::path::Path;

use sqlx::SqliteConnection;

use super::LibraryRow;
use super::scanner::{movie_for_file, movie_key};
use crate::db::Db;
use crate::util::now;

/// Merge the movies of a library that share a TMDB id. Returns the movies
/// others went into.
pub async fn same_movies(db: &Db, library_id: i64) -> sqlx::Result<Vec<i64>> {
    // By TMDB id, the movie keyed by it first, then the first added.
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT tmdb_id, id FROM items
         WHERE library_id = ?1 AND kind = 'movie' AND tmdb_id IN (
             SELECT tmdb_id FROM items WHERE library_id = ?1 AND kind = 'movie' AND tmdb_id IS NOT NULL
             GROUP BY tmdb_id HAVING COUNT(*) > 1)
         ORDER BY tmdb_id, key NOT LIKE 'movie:tmdb:%', added_at, id",
    )
    .bind(library_id)
    .fetch_all(&db.read)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut kept = Vec::new();
    let mut tx = db.write.begin().await?;
    let mut current: Option<(i64, i64)> = None;
    for (tmdb_id, id) in rows {
        match current {
            Some((tmdb, into)) if tmdb == tmdb_id => {
                // Matched again since the list was read: still the same title?
                let same: Option<i64> = sqlx::query_scalar(
                    "SELECT 1 FROM items a JOIN items b ON a.tmdb_id = b.tmdb_id WHERE a.id = ? AND b.id = ?",
                )
                .bind(into)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
                if same.is_some() {
                    merge_into(&mut tx, into, id).await?;
                    tracing::info!("merged movie {id} into {into}, both TMDB {tmdb_id}");
                    if kept.last() != Some(&into) {
                        kept.push(into);
                    }
                }
            }
            _ => current = Some((tmdb_id, id)),
        }
    }
    tx.commit().await?;
    // Hand edits that came with the other copies show.
    crate::overrides::apply(db, &kept).await?;
    Ok(kept)
}

/// Move everything of `from` to `into`, then delete `from`. What both have,
/// `into`'s wins, except what adds up: plays, watched, the latest position.
async fn merge_into(conn: &mut SqliteConnection, into: i64, from: i64) -> sqlx::Result<()> {
    let statements = [
        // Its files' keys now find `into`, and so do the keys of the movies
        // that went into it before.
        "INSERT INTO item_aliases (library_id, key, item_id) SELECT library_id, key, ?1 FROM items WHERE id = ?2
         ON CONFLICT(library_id, key) DO UPDATE SET item_id = excluded.item_id",
        "UPDATE item_aliases SET item_id = ?1 WHERE item_id = ?2",
        "UPDATE media_files SET item_id = ?1 WHERE item_id = ?2",
        // Bonus material.
        "UPDATE items SET parent_id = ?1 WHERE parent_id = ?2",
        "INSERT INTO user_item_state (user_id, item_id, position_ms, watched, play_count, last_played_at, updated_at,
                                      rating, rated_at)
         SELECT user_id, ?1, position_ms, watched, play_count, last_played_at, updated_at, rating, rated_at
         FROM user_item_state WHERE item_id = ?2
         ON CONFLICT(user_id, item_id) DO UPDATE SET
            position_ms = CASE WHEN excluded.updated_at > user_item_state.updated_at
                               THEN excluded.position_ms ELSE user_item_state.position_ms END,
            watched = MAX(user_item_state.watched, excluded.watched),
            play_count = user_item_state.play_count + excluded.play_count,
            last_played_at = CASE WHEN user_item_state.last_played_at IS NULL THEN excluded.last_played_at
                                  WHEN excluded.last_played_at IS NULL THEN user_item_state.last_played_at
                                  ELSE MAX(user_item_state.last_played_at, excluded.last_played_at) END,
            rating = CASE WHEN excluded.rated_at > COALESCE(user_item_state.rated_at, 0)
                          THEN excluded.rating ELSE user_item_state.rating END,
            rated_at = CASE WHEN excluded.rated_at > COALESCE(user_item_state.rated_at, 0)
                            THEN excluded.rated_at ELSE user_item_state.rated_at END,
            updated_at = MAX(user_item_state.updated_at, excluded.updated_at)",
        "UPDATE play_history SET item_id = ?1 WHERE item_id = ?2",
        "INSERT INTO watchlist (user_id, item_id, added_at) SELECT user_id, ?1, added_at FROM watchlist WHERE item_id = ?2
         ON CONFLICT(user_id, item_id) DO UPDATE SET added_at = MIN(watchlist.added_at, excluded.added_at)",
        "INSERT OR IGNORE INTO continue_watching_hidden (user_id, item_id, hidden_at)
         SELECT user_id, ?1, hidden_at FROM continue_watching_hidden WHERE item_id = ?2",
        // In a playlist that has `into` already, `from` goes (with `from`).
        "UPDATE playlist_items SET item_id = ?1
         WHERE item_id = ?2 AND playlist_id NOT IN (SELECT playlist_id FROM playlist_items WHERE item_id = ?1)",
        "INSERT OR IGNORE INTO collection_items (collection_id, item_id, position)
         SELECT collection_id, ?1, position FROM collection_items WHERE item_id = ?2",
        "INSERT OR IGNORE INTO item_labels (item_id, label) SELECT ?1, label FROM item_labels WHERE item_id = ?2",
        "INSERT OR IGNORE INTO stream_preferences (user_id, item_id, audio_language, audio_title, subtitles,
                                                   subtitle_language, subtitle_title, subtitle_forced, updated_at)
         SELECT user_id, ?1, audio_language, audio_title, subtitles, subtitle_language, subtitle_title,
                subtitle_forced, updated_at
         FROM stream_preferences WHERE item_id = ?2",
        "INSERT OR IGNORE INTO item_overrides (item_id, field, value)
         SELECT ?1, field, value FROM item_overrides WHERE item_id = ?2",
        "INSERT OR IGNORE INTO item_nfo (item_id, path, size, mtime, ids, fields, locked)
         SELECT ?1, path, size, mtime, ids, fields, locked FROM item_nfo WHERE item_id = ?2",
        "UPDATE requests SET item_id = ?1 WHERE item_id = ?2",
        // It was in the library since the first copy came.
        "UPDATE items SET added_at = MIN(added_at, (SELECT added_at FROM items WHERE id = ?2)), updated_at = ?3
         WHERE id = ?1",
        // Cast, genres and the rest of what is left of it go with it.
        "DELETE FROM items WHERE id = ?2",
    ];
    let now = now();
    for sql in statements {
        sqlx::query(sql).bind(into).bind(from).bind(now).execute(&mut *conn).await?;
    }
    Ok(())
}

/// Before a movie is matched to another title: its files that its own key
/// doesn't make go back to the movies their names make, and the keys merged
/// into it are let go. Returns those movies, which have yet to be matched.
pub async fn split(db: &Db, lib: &LibraryRow, movie: i64) -> sqlx::Result<Vec<i64>> {
    let roots = lib.paths();
    let mut tx = db.write.begin().await?;
    let key: Option<String> = sqlx::query_scalar("SELECT key FROM items WHERE id = ? AND kind = 'movie'")
        .bind(movie)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(key) = key else { return Ok(Vec::new()) };
    let aliases = sqlx::query("DELETE FROM item_aliases WHERE item_id = ?").bind(movie).execute(&mut *tx).await?;
    if aliases.rows_affected() == 0 {
        // Never merged: nothing to split.
        return Ok(Vec::new());
    }
    let files: Vec<(i64, String)> = sqlx::query_as("SELECT id, path FROM media_files WHERE item_id = ? ORDER BY id")
        .bind(movie)
        .fetch_all(&mut *tx)
        .await?;
    let rooted =
        |path: &Path| roots.iter().filter(|r| path.starts_with(r)).max_by_key(|r| r.as_os_str().len()).cloned();
    let mut leaving: Vec<(i64, String, std::path::PathBuf)> = Vec::new();
    let mut stays = false;
    for (id, path) in files {
        let path = std::path::PathBuf::from(path);
        let Some(root) = rooted(&path) else {
            stays = true;
            continue;
        };
        if movie_key(&path, &root) == key {
            stays = true;
        } else {
            leaving.push((id, path.to_string_lossy().into_owned(), root));
        }
    }
    // A movie none of whose files make its key keeps its first one, and
    // with it what people did with it.
    if !stays && !leaving.is_empty() {
        leaving.remove(0);
    }
    let mut made = Vec::new();
    for (file, path, root) in leaving {
        let duration: Option<i64> = sqlx::query_scalar("SELECT duration_ms FROM media_files WHERE id = ?")
            .bind(file)
            .fetch_one(&mut *tx)
            .await?;
        let id = movie_for_file(&mut tx, lib.id, Path::new(&path), &root, duration, false).await?;
        sqlx::query("UPDATE media_files SET item_id = ? WHERE id = ?").bind(id).bind(file).execute(&mut *tx).await?;
        if id != movie && !made.contains(&id) {
            made.push(id);
        }
    }
    tx.commit().await?;
    Ok(made)
}
