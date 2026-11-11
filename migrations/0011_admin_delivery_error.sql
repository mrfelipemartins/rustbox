-- Sanitized summary of the latest webhook delivery failure; no response bodies or URLs.
ALTER TABLE events ADD COLUMN last_error VARCHAR(64) NULL;
