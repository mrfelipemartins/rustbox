ALTER TABLE sends
    ADD COLUMN evidence JSON NULL,
    ADD COLUMN provider_id VARCHAR(512) NULL,
    ADD COLUMN thread_id VARCHAR(512) NULL,
    ADD COLUMN submitted_at DATETIME(6) NULL,
    ADD COLUMN reconciliation_state VARCHAR(24) NULL,
    ADD COLUMN reconciliation_evidence JSON NULL,
    ADD COLUMN reconciled_at DATETIME(6) NULL,
    ADD COLUMN resolution VARCHAR(24) NULL,
    ADD COLUMN resolution_reason VARCHAR(1024) NULL,
    ADD COLUMN resolution_evidence JSON NULL,
    ADD COLUMN resolved_at DATETIME(6) NULL,
    ADD COLUMN retry_of_send_id CHAR(36) CHARACTER SET ascii NULL;
