CREATE TABLE account_provisioning (
    app_id VARBINARY(128) NOT NULL,
    idempotency_key VARBINARY(128) NOT NULL,
    request_hash BINARY(32) NOT NULL,
    account_id CHAR(36) CHARACTER SET ascii NOT NULL,
    deleted_at DATETIME(6) NULL,
    PRIMARY KEY (app_id,idempotency_key),
    INDEX account_provisioning_account (account_id),
    INDEX account_provisioning_retention (deleted_at)
);
CREATE TABLE mail_messages (
    id CHAR(36) CHARACTER SET ascii PRIMARY KEY,
    account_id CHAR(36) CHARACTER SET ascii NOT NULL,
    backend VARCHAR(16) NOT NULL,
    provider_id VARCHAR(512) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin NULL,
    metadata JSON NOT NULL,
    flags JSON NOT NULL,
    received_at DATETIME(6) NULL,
    size BIGINT UNSIGNED NOT NULL DEFAULT 0,
    content_state VARCHAR(32) NOT NULL DEFAULT 'pending',
    discovery_reason VARCHAR(32) NOT NULL DEFAULT 'live',
    identity_confidence VARCHAR(32) NOT NULL DEFAULT 'provider',
    removed_at DATETIME(6) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    UNIQUE KEY mail_messages_provider (account_id,provider_id),
    INDEX mail_messages_account (account_id,received_at,id),
    INDEX mail_messages_content (content_state,created_at),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
);
CREATE TABLE message_locations (
    message_id CHAR(36) CHARACTER SET ascii NOT NULL,
    folder VARCHAR(512) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin NOT NULL,
    provider_id VARCHAR(512) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin NULL,
    uidvalidity INT UNSIGNED NULL,
    uid INT UNSIGNED NULL,
    active BOOLEAN NOT NULL DEFAULT TRUE,
    PRIMARY KEY (message_id,folder),
    FOREIGN KEY (message_id) REFERENCES mail_messages(id) ON DELETE CASCADE
);
ALTER TABLE messages ADD COLUMN rustbox_id CHAR(36) CHARACTER SET ascii NULL, ADD UNIQUE KEY messages_rustbox_id (rustbox_id);
