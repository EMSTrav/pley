-- Movies of a library matched to the same TMDB entry are one movie, with
-- each copy a version (see `library::merge`). The keys the merged movies'
-- files made (`movie:interstellar:2014`) are kept here, so that a later
-- scan of those files finds the movie they went into instead of making
-- them a movie of their own again. Fixing the movie's match lets them go.
CREATE TABLE item_aliases (
    library_id INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    key        TEXT    NOT NULL,
    item_id    INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    PRIMARY KEY (library_id, key)
) WITHOUT ROWID;
CREATE INDEX item_aliases_item ON item_aliases(item_id);
