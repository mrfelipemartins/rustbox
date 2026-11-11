ALTER TABLE sends
    ADD COLUMN append_last_error VARCHAR(64) NULL,
    ADD COLUMN append_reconciliation_state VARCHAR(16) NULL,
    ADD COLUMN append_reconciliation_evidence JSON NULL,
    ADD COLUMN append_attempts INT UNSIGNED NOT NULL DEFAULT 0,
    ADD COLUMN append_revision BIGINT UNSIGNED NOT NULL DEFAULT 0;
