-- Site favicon, fetched from the site's domain and stored locally (no third-party favicon service).
ALTER TABLE sites ADD COLUMN icon_data BLOB;
ALTER TABLE sites ADD COLUMN icon_type TEXT;
-- When we last tried to fetch it (set even when none was found, so we don't retry constantly).
ALTER TABLE sites ADD COLUMN icon_checked_at TEXT;
