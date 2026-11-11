CREATE TABLE native_state (
 account_id CHAR(36) CHARACTER SET ascii PRIMARY KEY,
 wake_generation BIGINT UNSIGNED NOT NULL DEFAULT 0,
 next_sync_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
 last_sync_at DATETIME(6) NULL,
 last_error VARCHAR(128) NULL,
 subscription_id VARCHAR(512) CHARACTER SET ascii NULL,
 INDEX native_subscription(subscription_id),
 subscription_secret BLOB NULL,
 subscription_expires_at DATETIME(6) NULL,
 subscription_error VARCHAR(128) NULL,
 FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);
CREATE TABLE native_checkpoints (
 account_id CHAR(36) CHARACTER SET ascii NOT NULL,
 folder VARCHAR(512) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin NOT NULL,
 sync_cursor TEXT NULL,
 next_page TEXT NULL,
 recovery BOOLEAN NOT NULL DEFAULT FALSE,
 scan_id CHAR(36) CHARACTER SET ascii NULL,
 last_full_at DATETIME(6) NULL,
 PRIMARY KEY(account_id,folder),
 FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);
CREATE TABLE native_seen (
 account_id CHAR(36) CHARACTER SET ascii NOT NULL,
 folder VARCHAR(512) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin NOT NULL,
 recovery BOOLEAN NOT NULL DEFAULT FALSE,
 scan_id CHAR(36) CHARACTER SET ascii NOT NULL,
 provider_hash BINARY(32) NOT NULL,
 PRIMARY KEY(account_id,folder,provider_hash),
 FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);
CREATE TABLE sync_runs (
 id CHAR(36) CHARACTER SET ascii PRIMARY KEY,
 account_id CHAR(36) CHARACTER SET ascii NOT NULL,
 state VARCHAR(16) NOT NULL DEFAULT 'queued',
 created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
 finished_at DATETIME(6) NULL,
 INDEX(account_id,state),
 FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);
CREATE TABLE content_cache (
 message_id CHAR(36) CHARACTER SET ascii PRIMARY KEY,
 payload LONGBLOB NOT NULL,
 size BIGINT UNSIGNED NOT NULL,
 available_until DATETIME(6) NOT NULL,
 FOREIGN KEY(message_id) REFERENCES mail_messages(id) ON DELETE CASCADE
);
CREATE TABLE cache_budget (id TINYINT PRIMARY KEY, used_bytes BIGINT UNSIGNED NOT NULL DEFAULT 0);
INSERT INTO cache_budget(id) VALUES(1);
ALTER TABLE mail_messages ADD content_attempts INT UNSIGNED NOT NULL DEFAULT 0,
 ADD content_next_attempt_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
 ADD content_error VARCHAR(64) NULL;
