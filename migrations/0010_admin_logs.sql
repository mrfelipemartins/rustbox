CREATE TABLE admin_logs (
    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    observed_at DATETIME(6) NOT NULL,
    level VARCHAR(8) CHARACTER SET ascii NOT NULL,
    component VARCHAR(128) CHARACTER SET ascii NOT NULL,
    account_id CHAR(36) CHARACTER SET ascii NULL,
    send_id CHAR(36) CHARACTER SET ascii NULL,
    sync_id CHAR(36) CHARACTER SET ascii NULL,
    message VARCHAR(512) NOT NULL,
    fields JSON NOT NULL,
    payload_bytes INT UNSIGNED NOT NULL,
    INDEX admin_logs_time (observed_at, id),
    INDEX admin_logs_level (level, id),
    INDEX admin_logs_component (component, id),
    INDEX admin_logs_account (account_id, id),
    INDEX admin_logs_send (send_id, id),
    INDEX admin_logs_sync (sync_id, id)
);
CREATE TABLE admin_log_budget (
    id TINYINT PRIMARY KEY,
    used_bytes BIGINT UNSIGNED NOT NULL DEFAULT 0
);
INSERT INTO admin_log_budget (id, used_bytes) VALUES (1, 0);
