-- Audiobooks' online metadata: the Audible product (ASIN, "B08G9PRS1K") or,
-- for a book Audible doesn't have, the Open Library work ("OL21745884W")
-- a book was matched to. A later refresh looks the book up by them instead
-- of searching again.
ALTER TABLE items ADD COLUMN audible_asin TEXT;
ALTER TABLE items ADD COLUMN openlibrary_id TEXT;
