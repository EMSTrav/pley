-- no-transaction
-- Audiobooks: libraries of kind 'audiobooks' hold items of kind 'audiobook',
-- each with its files as items of kind 'part'.
--
-- The libraries' kinds are checked by the table itself, so the table is made
-- again with the new kind, as SQLite's documentation says to change a
-- constraint (https://sqlite.org/lang_altertable.html#otheralter): with
-- foreign keys off for the while, so that dropping the old table doesn't
-- delete every library's items, and in one transaction. The trigger on the
-- old table goes with it and is made again.
PRAGMA foreign_keys = OFF;
BEGIN;

CREATE TABLE libraries_new (
    id             INTEGER PRIMARY KEY,
    name           TEXT    NOT NULL,
    kind           TEXT    NOT NULL CHECK (kind IN ('movies', 'shows', 'music', 'audiobooks')),
    paths          TEXT    NOT NULL DEFAULT '[]',
    created_at     INTEGER NOT NULL,
    scanned_at     INTEGER,
    position       INTEGER NOT NULL DEFAULT 0,
    labels_pending INTEGER NOT NULL DEFAULT 0
);
INSERT INTO libraries_new (id, name, kind, paths, created_at, scanned_at, position, labels_pending)
    SELECT id, name, kind, paths, created_at, scanned_at, position, labels_pending FROM libraries;
DROP TABLE libraries;
ALTER TABLE libraries_new RENAME TO libraries;

CREATE TRIGGER library_ids_given AFTER INSERT ON libraries BEGIN
    UPDATE library_ids SET last = MAX(last, NEW.id);
END;

-- Who wrote and who reads a book, and the series it is part of, come from
-- the files' tags or folders and are kept on the book: `authors` and
-- `narrators` are JSON arrays of names, in their order; `series_index` is
-- the book's place in its series (2, or 2.5 for a novella between two
-- books).
ALTER TABLE items ADD COLUMN authors TEXT;
ALTER TABLE items ADD COLUMN narrators TEXT;
ALTER TABLE items ADD COLUMN series TEXT;
ALTER TABLE items ADD COLUMN series_index REAL;

COMMIT;
PRAGMA foreign_keys = ON;
