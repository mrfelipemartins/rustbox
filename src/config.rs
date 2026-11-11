use crate::{
    error::{Error, Result},
    oauth::config::OAuthConfig,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_yaml::Value;
use std::{
    collections::HashMap,
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
};

pub struct Config {
    pub database_url: String,
    pub listen: SocketAddr,
    pub api_keys: HashMap<String, String>,
    pub encryption_key: Vec<u8>,
    pub allow_private_networks: bool,
    pub tls_ca_file: Option<PathBuf>,
    pub log_filter: String,
    pub max_message_bytes: usize,
    pub imap_concurrency: usize,
    pub smtp_concurrency: usize,
    pub webhook_concurrency: usize,
    pub command_timeout_secs: u64,
    pub webhooks: WebhookConfig,
    pub oauth: OAuthConfig,
    pub native: NativeConfig,
    pub cache: CacheConfig,
    pub admin: AdminConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default)]
    server: Server,
    database: Database,
    security: Security,
    api: Api,
    #[serde(default)]
    mail: Mail,
    #[serde(default)]
    workers: Workers,
    #[serde(default)]
    webhooks: WebhookConfig,
    #[serde(default)]
    oauth: OAuthConfig,
    #[serde(default)]
    native: NativeConfig,
    #[serde(default)]
    cache: CacheConfig,
    #[serde(default)]
    admin: AdminConfig,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminConfig {
    pub enabled: bool,
    pub session_hours: u64,
    pub logs: AdminLogConfig,
}
impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            session_hours: 8,
            logs: AdminLogConfig::default(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminLogConfig {
    pub retention_days: u32,
    pub capacity_bytes: u64,
    pub max_record_bytes: usize,
}
impl Default for AdminLogConfig {
    fn default() -> Self {
        Self {
            retention_days: 7,
            capacity_bytes: 1024 * 1024 * 1024,
            max_record_bytes: 16 * 1024,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Server {
    listen: String,
    command_timeout_secs: u64,
    log_filter: String,
}
impl Default for Server {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".into(),
            command_timeout_secs: 60,
            log_filter: "rustbox=info".into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Database {
    url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Security {
    encryption_key: String,
    #[serde(default)]
    allow_private_networks: bool,
    tls_ca_file: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Api {
    keys: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Mail {
    max_message_bytes: usize,
}
impl Default for Mail {
    fn default() -> Self {
        Self {
            max_message_bytes: 25 * 1024 * 1024,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Workers {
    imap_concurrency: usize,
    smtp_concurrency: usize,
    webhook_concurrency: usize,
}
impl Default for Workers {
    fn default() -> Self {
        Self {
            imap_concurrency: 64,
            smtp_concurrency: 8,
            webhook_concurrency: 32,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookConfig {
    pub timeout_secs: u64,
    pub retry_window_hours: u32,
    pub retention_days: u32,
}
impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 10,
            retry_window_hours: 24,
            retention_days: 7,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativeConfig {
    pub public_url: Option<String>,
    pub poll_secs: u64,
    pub safety_poll_secs: u64,
    pub reconcile_secs: u64,
    pub concurrency: usize,
    pub gmail_topic: Option<String>,
    pub gmail_topics: HashMap<String, String>,
    pub pubsub_audience: Option<String>,
    pub pubsub_email: Option<String>,
}
impl Default for NativeConfig {
    fn default() -> Self {
        Self {
            public_url: None,
            poll_secs: 30,
            safety_poll_secs: 300,
            reconcile_secs: 600,
            concurrency: 16,
            gmail_topic: None,
            gmail_topics: HashMap::new(),
            pubsub_audience: None,
            pubsub_email: None,
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    pub retention_days: u32,
    pub max_message_bytes: usize,
    pub capacity_bytes: u64,
    pub concurrency: usize,
}
impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            retention_days: 7,
            max_message_bytes: 25 * 1024 * 1024,
            capacity_bytes: 10 * 1024 * 1024 * 1024,
            concurrency: 4,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let mut args = env::args_os().skip(1);
        let path = match args.next() {
            None => PathBuf::from("config.yaml"),
            Some(flag) if flag == "--config" => PathBuf::from(
                args.next()
                    .ok_or_else(|| Error::Invalid("--config requires a YAML file path".into()))?,
            ),
            Some(_) => return Err(Error::Invalid("usage: rustbox [--config PATH]".into())),
        };
        if args.next().is_some() {
            return Err(Error::Invalid("usage: rustbox [--config PATH]".into()));
        }
        Self::from_path(&path)
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        Self::from_path_with_env(path, env::vars().collect())
    }

    fn from_path_with_env(path: &Path, environment: HashMap<String, String>) -> Result<Self> {
        let base = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let dotenv = base.join(".env");
        let mut vars = HashMap::new();
        match dotenvy::from_path_iter(&dotenv) {
            Ok(entries) => {
                for entry in entries {
                    let (key, value) = entry.map_err(|_| {
                        Error::Invalid("invalid .env beside configuration file".into())
                    })?;
                    vars.insert(key, value);
                }
            }
            Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(Error::Invalid(
                    "cannot read .env beside configuration file".into(),
                ));
            }
        }
        vars.extend(environment);
        let yaml = std::fs::read_to_string(path)
            .map_err(|_| Error::Invalid("cannot read configuration YAML file".into()))?;
        Self::parse(&yaml, base, &vars)
    }

    fn parse(yaml: &str, base: &Path, vars: &HashMap<String, String>) -> Result<Self> {
        let mut value: Value = serde_yaml::from_str(yaml).map_err(|error| {
            let at = error
                .location()
                .map(|p| format!(" at line {}, column {}", p.line(), p.column()))
                .unwrap_or_default();
            Error::Invalid(format!(
                "invalid configuration YAML{at}; check syntax and duplicate keys"
            ))
        })?;
        interpolate_value(&mut value, "", vars)?;
        let mut doc: Document = serde_path_to_error::deserialize(value).map_err(|error| {
            Error::Invalid(format!("invalid configuration at {}", error.path()))
        })?;
        if doc.api.keys.is_empty()
            || doc.api.keys.iter().any(|(name, value)| {
                name.is_empty() || name.chars().count() > 128 || value.len() < 32
            })
        {
            return Err(Error::Invalid(
                "api.keys requires app names of 1–128 characters and keys of at least 32 characters".into(),
            ));
        }
        if doc
            .api
            .keys
            .values()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != doc.api.keys.len()
        {
            return Err(Error::Invalid(
                "api.keys must contain distinct keys for each app".into(),
            ));
        }
        if doc.database.url.parse::<url::Url>().is_err()
            || !doc.database.url.starts_with("mysql://")
        {
            return Err(Error::Invalid(
                "database.url must be a MySQL connection URL".into(),
            ));
        }
        let encryption_key = STANDARD
            .decode(doc.security.encryption_key)
            .map_err(|_| Error::Invalid("security.encryption_key must be base64".into()))?;
        if encryption_key.len() != 32 {
            return Err(Error::Invalid(
                "security.encryption_key must decode to 32 bytes".into(),
            ));
        }
        for (name, value) in [
            (
                "server.command_timeout_secs",
                doc.server.command_timeout_secs,
            ),
            ("mail.max_message_bytes", doc.mail.max_message_bytes as u64),
            (
                "workers.imap_concurrency",
                doc.workers.imap_concurrency as u64,
            ),
            (
                "workers.smtp_concurrency",
                doc.workers.smtp_concurrency as u64,
            ),
            (
                "workers.webhook_concurrency",
                doc.workers.webhook_concurrency as u64,
            ),
            ("admin.session_hours", doc.admin.session_hours),
            (
                "admin.logs.retention_days",
                u64::from(doc.admin.logs.retention_days),
            ),
            ("admin.logs.capacity_bytes", doc.admin.logs.capacity_bytes),
            (
                "admin.logs.max_record_bytes",
                doc.admin.logs.max_record_bytes as u64,
            ),
            ("webhooks.timeout_secs", doc.webhooks.timeout_secs),
            (
                "webhooks.retry_window_hours",
                u64::from(doc.webhooks.retry_window_hours),
            ),
            (
                "webhooks.retention_days",
                u64::from(doc.webhooks.retention_days),
            ),
        ] {
            if value == 0 {
                return Err(Error::Invalid(format!("{name} must be positive")));
            }
        }
        if doc.admin.session_hours > 24 * 30
            || doc.admin.logs.retention_days > 3650
            || doc.admin.logs.max_record_bytes < 256
            || doc.admin.logs.max_record_bytes > 16 * 1024
            || doc.admin.logs.capacity_bytes < doc.admin.logs.max_record_bytes as u64
        {
            return Err(Error::Invalid("admin limits exceed supported bounds (session <= 720 hours, retention <= 3650 days, record 256–16384 bytes, capacity >= record size)".into()));
        }
        if u64::from(doc.webhooks.retention_days) * 24 < u64::from(doc.webhooks.retry_window_hours)
        {
            return Err(Error::Invalid(
                "webhooks.retention_days must cover retry_window_hours".into(),
            ));
        }
        if tracing_subscriber::EnvFilter::try_new(&doc.server.log_filter).is_err() {
            return Err(Error::Invalid("server.log_filter is invalid".into()));
        }
        if let Some(file) = &mut doc.security.tls_ca_file {
            if file.is_relative() {
                *file = base.join(&*file);
            }
            std::fs::File::open(file).map_err(|_| {
                Error::Invalid("security.tls_ca_file must reference a readable file".into())
            })?;
        }
        doc.oauth.validate(base)?;
        if doc.native.concurrency == 0
            || doc.native.poll_secs == 0
            || doc.native.safety_poll_secs == 0
            || doc.native.reconcile_secs == 0
            || doc.cache.concurrency == 0
            || doc.cache.retention_days == 0
            || doc.cache.max_message_bytes == 0
            || doc.cache.capacity_bytes == 0
        {
            return Err(Error::Invalid(
                "native and cache limits must be positive".into(),
            ));
        }
        let gmail_push = doc.native.gmail_topic.is_some() || !doc.native.gmail_topics.is_empty();
        if gmail_push && (doc.native.pubsub_audience.is_none() || doc.native.pubsub_email.is_none())
        {
            return Err(Error::Invalid(
                "Gmail push requires native.pubsub_audience and native.pubsub_email".into(),
            ));
        }
        if let Some(public_url) = &doc.native.public_url {
            let url = url::Url::parse(public_url)
                .map_err(|_| Error::Invalid("native.public_url must be HTTPS".into()))?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(Error::Invalid("native.public_url must be HTTPS".into()));
            }
        }
        Ok(Self {
            database_url: doc.database.url,
            listen: doc
                .server
                .listen
                .parse()
                .map_err(|_| Error::Invalid("server.listen is invalid".into()))?,
            api_keys: doc.api.keys,
            encryption_key,
            allow_private_networks: doc.security.allow_private_networks,
            tls_ca_file: doc.security.tls_ca_file,
            log_filter: doc.server.log_filter,
            max_message_bytes: doc.mail.max_message_bytes,
            imap_concurrency: doc.workers.imap_concurrency,
            smtp_concurrency: doc.workers.smtp_concurrency,
            webhook_concurrency: doc.workers.webhook_concurrency,
            command_timeout_secs: doc.server.command_timeout_secs,
            webhooks: doc.webhooks,
            oauth: doc.oauth,
            native: doc.native,
            cache: doc.cache,
            admin: doc.admin,
        })
    }
}

fn interpolate_value(value: &mut Value, path: &str, vars: &HashMap<String, String>) -> Result<()> {
    match value {
        Value::String(text) => {
            let resolved = interpolate(text, path, vars)?;
            *value = match path {
                "server.command_timeout_secs"
                | "mail.max_message_bytes"
                | "workers.imap_concurrency"
                | "workers.smtp_concurrency"
                | "workers.webhook_concurrency"
                | "webhooks.timeout_secs"
                | "webhooks.retry_window_hours"
                | "webhooks.retention_days"
                | "native.poll_secs"
                | "native.safety_poll_secs"
                | "native.reconcile_secs"
                | "native.concurrency"
                | "cache.retention_days"
                | "cache.max_message_bytes"
                | "cache.capacity_bytes"
                | "cache.concurrency"
                | "admin.session_hours"
                | "admin.logs.retention_days"
                | "admin.logs.capacity_bytes"
                | "admin.logs.max_record_bytes"
                | "oauth.concurrency" => Value::Number(
                    resolved
                        .parse::<u64>()
                        .map_err(|_| {
                            Error::Invalid(format!("{path} must be a nonnegative integer"))
                        })?
                        .into(),
                ),
                "security.allow_private_networks" | "admin.enabled" => Value::Bool(
                    resolved
                        .parse::<bool>()
                        .map_err(|_| Error::Invalid(format!("{path} must be true or false")))?,
                ),
                _ => Value::String(resolved),
            };
        }
        Value::Mapping(map) => {
            for (key, value) in map {
                let name = key
                    .as_str()
                    .ok_or_else(|| Error::Invalid(format!("{path} requires string field names")))?;
                let child = if path.is_empty() {
                    name.to_owned()
                } else {
                    format!("{path}.{name}")
                };
                interpolate_value(value, &child, vars)?;
            }
        }
        Value::Sequence(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                interpolate_value(value, &format!("{path}[{index}]"), vars)?;
            }
        }
        Value::Tagged(_) => return Err(Error::Invalid(format!("{path} does not allow YAML tags"))),
        _ => {}
    }
    Ok(())
}

fn interpolate(value: &str, path: &str, vars: &HashMap<String, String>) -> Result<String> {
    let mut chars = value.chars().peekable();
    let mut output = String::new();
    while let Some(c) = chars.next() {
        if c != '$' {
            output.push(c);
            continue;
        }
        match chars.peek() {
            Some('$') => {
                chars.next();
                output.push('$');
            }
            Some('{') => {
                chars.next();
                let mut key = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    key.push(c);
                }
                if !closed
                    || key.is_empty()
                    || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    return Err(Error::Invalid(format!(
                        "{path} has invalid environment substitution"
                    )));
                }
                output.push_str(vars.get(&key).ok_or_else(|| {
                    Error::Invalid(format!(
                        "{path} references missing environment variable {key}"
                    ))
                })?);
            }
            _ => output.push('$'),
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn yaml() -> String {
        format!(
            "database:\n  url: mysql://localhost/rustbox\nsecurity:\n  encryption_key: {}\napi:\n  keys:\n    app: '{}'\n",
            STANDARD.encode([1; 32]),
            "k".repeat(32)
        )
    }
    fn parse(input: &str, vars: HashMap<String, String>) -> Result<Config> {
        Config::parse(input, Path::new("."), &vars)
    }
    #[test]
    fn defaults_and_no_implicit_overrides() {
        let c = parse(
            &yaml(),
            HashMap::from([("LISTEN".into(), "invalid".into())]),
        )
        .unwrap();
        assert_eq!(c.listen.to_string(), "127.0.0.1:8080");
        assert_eq!(c.imap_concurrency, 64);
        assert!(c.oauth.providers.is_empty());
        assert!(c.admin.enabled);
        assert_eq!(c.admin.session_hours, 8);
        assert_eq!(c.admin.logs.retention_days, 7);
        assert_eq!(c.admin.logs.capacity_bytes, 1024 * 1024 * 1024);
    }
    #[test]
    fn scalar_substitution_is_not_yaml_injection() {
        let secret = "key: value\nsecurity: [broken] # secret $$ ${UNSET}".repeat(2);
        let input = yaml().replace(&"k".repeat(32), "${API_KEY}")
            + "workers:\n  imap_concurrency: '${N}'\n";
        let c = parse(
            &input,
            HashMap::from([
                ("API_KEY".into(), secret.clone()),
                ("N".into(), "42".into()),
            ]),
        )
        .unwrap();
        assert_eq!(c.api_keys["app"], secret);
        assert_eq!(c.imap_concurrency, 42);
        assert_eq!(
            interpolate("$${NO} $$", "x", &HashMap::new()).unwrap(),
            "${NO} $"
        );
    }
    #[test]
    fn rejects_missing_duplicate_unknown_and_bad_type_without_values() {
        for suffix in [
            "server:\n  listen: '${MISSING}'\n",
            "server:\n  listen: 'bad-secret'\n",
            "server:\n  extra: 'bad-secret'\n",
            "workers:\n  imap_concurrency: 0\n",
            "server:\n  listen: a\n  listen: b\n",
        ] {
            let error = parse(&(yaml() + suffix), HashMap::new())
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains("bad-secret"));
        }
    }
    #[test]
    fn dotenv_is_adjacent_environment_wins_and_paths_are_relative() {
        let dir = env::temp_dir().join(format!("rustbox-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("config.yaml");
        std::fs::write(dir.join(".env"), "CONFIG_TEST_PORT=127.0.0.1:2222\n").unwrap();
        std::fs::write(dir.join("ca.pem"), "placeholder").unwrap();
        let text = yaml().replace("security:\n", "security:\n  tls_ca_file: ca.pem\n")
            + "server:\n  listen: '${CONFIG_TEST_PORT}'\n";
        std::fs::write(&file, text).unwrap();
        let c = Config::from_path_with_env(&file, HashMap::new()).unwrap();
        assert_eq!(c.listen.port(), 2222);
        assert_eq!(c.tls_ca_file, Some(dir.join("ca.pem")));
        let c = Config::from_path_with_env(
            &file,
            HashMap::from([("CONFIG_TEST_PORT".into(), "127.0.0.1:3333".into())]),
        )
        .unwrap();
        assert_eq!(c.listen.port(), 3333);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn app_names_fit_storage_and_authentication_keys_are_unique() {
        let oversized = yaml().replace("    app:", &format!("    {}:", "a".repeat(129)));
        assert!(parse(&oversized, HashMap::new()).is_err());
        let duplicate = yaml() + &format!("    second: '{}'\n", "k".repeat(32));
        assert!(parse(&duplicate, HashMap::new()).is_err());
    }

    #[test]
    fn rejects_incompatible_provider_options() {
        let input = yaml()
            + "oauth:\n  providers:\n    google:\n      provider: google\n      mode: application\n      client_id: value\n      client_secret: value\n";
        assert!(parse(&input, HashMap::new()).is_err());
    }
    #[test]
    fn admin_configuration_is_typed_bounded_and_substitutable() {
        let c = parse(
            &(yaml() + "admin:\n  enabled: '${ADMIN}'\n  session_hours: '${HOURS}'\n"),
            HashMap::from([
                ("ADMIN".into(), "false".into()),
                ("HOURS".into(), "2".into()),
            ]),
        )
        .unwrap();
        assert!(!c.admin.enabled);
        assert_eq!(c.admin.session_hours, 2);
        for suffix in [
            "admin:\n  session_hours: 0\n",
            "admin:\n  logs:\n    max_record_bytes: 32\n",
            "admin:\n  logs:\n    capacity_bytes: 1\n",
        ] {
            assert!(parse(&(yaml() + suffix), HashMap::new()).is_err());
        }
    }
}
