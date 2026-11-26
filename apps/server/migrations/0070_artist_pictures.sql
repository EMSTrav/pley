-- Artists are given pictures from fanart.tv and TheAudioDB once their
-- MusicBrainz id is known: those without one are looked up again.
UPDATE items SET metadata_at = NULL
WHERE kind = 'artist' AND metadata_locked = 0 AND metadata_at > 0 AND (poster IS NULL OR backdrop IS NULL);
