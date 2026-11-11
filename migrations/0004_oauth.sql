CREATE TABLE oauth_credentials (
    id CHAR(36) CHARACTER SET ascii PRIMARY KEY,
    client_id VARCHAR(128) COLLATE utf8mb4_bin NOT NULL,
    secret BLOB NOT NULL,
    revision BIGINT UNSIGNED NOT NULL DEFAULT 1,
    health VARCHAR(32) NOT NULL DEFAULT 'healthy',
    last_error VARCHAR(64) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
);
CREATE TABLE oauth_account_credentials (
    account_id CHAR(36) CHARACTER SET ascii NOT NULL,
    protocol VARCHAR(4) CHARACTER SET ascii NOT NULL,
    credential_id CHAR(36) CHARACTER SET ascii NOT NULL,
    PRIMARY KEY (account_id,protocol),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE,
    FOREIGN KEY (credential_id) REFERENCES oauth_credentials(id) ON DELETE RESTRICT
);
CREATE TABLE oauth_authorizations (
    id CHAR(36) CHARACTER SET ascii PRIMARY KEY,
    app_id VARCHAR(128) COLLATE utf8mb4_bin NOT NULL,
    secret BLOB NULL,
    state VARCHAR(24) NOT NULL DEFAULT 'pending',
    credential_id CHAR(36) CHARACTER SET ascii NULL,
    expires_at DATETIME(6) NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    INDEX oauth_authorizations_expiry (expires_at)
);
