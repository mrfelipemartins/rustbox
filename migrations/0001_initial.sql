CREATE TABLE accounts (
    id CHAR(36) CHARACTER SET ascii PRIMARY KEY,
    config LONGBLOB NOT NULL,
    state VARCHAR(32) NOT NULL DEFAULT 'created',
    smtp_state VARCHAR(32) NOT NULL DEFAULT 'unvalidated',
    last_connected_at DATETIME(6) NULL,
    last_disconnected_at DATETIME(6) NULL,
    next_retry_at DATETIME(6) NULL,
    retry_count INT UNSIGNED NOT NULL DEFAULT 0,
    last_error VARCHAR(128) NULL,
    last_failure_event VARCHAR(64) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
);
CREATE TABLE event_sequence (
    id TINYINT PRIMARY KEY,
    last_id BIGINT UNSIGNED NOT NULL,
    pruned_through BIGINT UNSIGNED NOT NULL DEFAULT 0
);
INSERT INTO event_sequence (id,last_id) VALUES (1,0);
CREATE TABLE events (
    id BIGINT UNSIGNED PRIMARY KEY,
    schema_version INT UNSIGNED NOT NULL DEFAULT 1,
    account_id CHAR(36) CHARACTER SET ascii NOT NULL,
    event_type VARCHAR(64) NOT NULL,
    observed_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    payload JSON NOT NULL,
    webhook_url TEXT NULL,
    webhook_secret BLOB NULL,
    delivery_state VARCHAR(16) NOT NULL DEFAULT 'pending',
    attempt_count INT UNSIGNED NOT NULL DEFAULT 0,
    next_attempt_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    delivered_at DATETIME(6) NULL,
    claimed_at DATETIME(6) NULL,
    last_status SMALLINT UNSIGNED NULL,
    INDEX events_delivery (delivery_state,next_attempt_at,id),
    INDEX events_account (account_id,id),
    INDEX events_retention (observed_at,id)
);
CREATE TABLE mailbox_checkpoints (
    account_id CHAR(36) CHARACTER SET ascii NOT NULL,
    mailbox VARCHAR(512) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
    uidvalidity INT UNSIGNED NOT NULL,
    last_uid INT UNSIGNED NOT NULL DEFAULT 0,
    highest_modseq BIGINT UNSIGNED NULL,
    initial_uid_boundary INT UNSIGNED NOT NULL DEFAULT 0,
    backfill_complete BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (account_id,mailbox),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
);
CREATE TABLE messages (
    account_id CHAR(36) CHARACTER SET ascii NOT NULL,
    mailbox VARCHAR(512) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
    uidvalidity INT UNSIGNED NOT NULL,
    uid INT UNSIGNED NOT NULL,
    flags JSON NOT NULL,
    metadata JSON NOT NULL,
    internal_date DATETIME(6) NULL,
    size INT UNSIGNED NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id,mailbox,uidvalidity,uid),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
);
CREATE TABLE sends (
    id CHAR(36) CHARACTER SET ascii PRIMARY KEY,
    account_id CHAR(36) CHARACTER SET ascii NOT NULL,
    app_id VARCHAR(128) NOT NULL,
    idempotency_key VARCHAR(128) NOT NULL,
    request_hash BINARY(32) NOT NULL,
    status VARCHAR(24) NOT NULL,
    attempts INT UNSIGNED NOT NULL DEFAULT 0,
    next_attempt_at DATETIME(6) NOT NULL,
    created_at DATETIME(6) NOT NULL,
    updated_at DATETIME(6) NOT NULL,
    expires_at DATETIME(6) NOT NULL,
    last_error VARCHAR(64) NULL,
    message_id VARCHAR(255) NOT NULL,
    append_status VARCHAR(24) NOT NULL,
    append_mailbox VARCHAR(512) NULL,
    UNIQUE KEY sends_idempotency (app_id,account_id,idempotency_key),
    INDEX sends_queue (status,next_attempt_at),
    INDEX sends_retention (created_at),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
);
CREATE TABLE send_blobs (
    send_id CHAR(36) CHARACTER SET ascii PRIMARY KEY,
    payload LONGBLOB NOT NULL,
    FOREIGN KEY (send_id) REFERENCES sends(id) ON DELETE CASCADE
);
