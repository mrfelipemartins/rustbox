use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Google,
    Microsoft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Delegated,
    DomainWideDelegation,
    Application,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRegistration {
    pub provider: Provider,
    pub mode: Mode,
    #[serde(default)]
    pub shared: bool,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub tenant: Option<String>,
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    pub service_account_file: Option<PathBuf>,
    pub certificate_file: Option<PathBuf>,
    pub private_key_file: Option<PathBuf>,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OAuthConfig {
    pub providers: BTreeMap<String, ProviderRegistration>,
    pub concurrency: usize,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            providers: BTreeMap::new(),
            concurrency: 8,
        }
    }
}

impl OAuthConfig {
    pub fn validate(&mut self, base: &Path) -> Result<()> {
        if self.concurrency == 0 {
            return Err(Error::Invalid("oauth.concurrency must be positive".into()));
        }
        for (id, provider) in &mut self.providers {
            if id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
            {
                return Err(Error::Invalid(
                    "oauth.providers contains an invalid registration ID".into(),
                ));
            }
            provider.validate(&format!("oauth.providers.{id}"), base)?;
        }
        Ok(())
    }
}

impl ProviderRegistration {
    fn validate(&mut self, path: &str, base: &Path) -> Result<()> {
        let required = |value: &Option<String>, field: &str| -> Result<()> {
            if value.as_ref().is_none_or(|v| v.trim().is_empty()) {
                return Err(Error::Invalid(format!("{path}.{field} is required")));
            }
            Ok(())
        };
        let invalid = || Error::Invalid(format!("{path} has incompatible provider options"));
        if self.shared && !(self.provider == Provider::Microsoft && self.mode == Mode::Delegated) {
            return Err(invalid());
        }
        let certificate = self.certificate_file.is_some() && self.private_key_file.is_some();
        if self.certificate_file.is_some() != self.private_key_file.is_some() {
            return Err(invalid());
        }
        match (self.provider, self.mode) {
            (Provider::Google, Mode::Delegated) | (Provider::Microsoft, Mode::Delegated) => {
                required(&self.client_id, "client_id")?;
                required(&self.client_secret, "client_secret")?;
                if self.service_account_file.is_some() || certificate {
                    return Err(invalid());
                }
                if self.provider == Provider::Google && self.tenant.is_some() {
                    return Err(invalid());
                }
                if self.provider == Provider::Microsoft {
                    required(&self.tenant, "tenant")?;
                }
            }
            (Provider::Google, Mode::DomainWideDelegation) => {
                if self.service_account_file.is_none()
                    || self.client_id.is_some()
                    || self.client_secret.is_some()
                    || self.tenant.is_some()
                    || certificate
                    || !self.redirect_uris.is_empty()
                {
                    return Err(invalid());
                }
            }
            (Provider::Microsoft, Mode::Application) => {
                required(&self.client_id, "client_id")?;
                required(&self.tenant, "tenant")?;
                if self.service_account_file.is_some()
                    || !self.redirect_uris.is_empty()
                    || self.client_secret.is_some() == certificate
                {
                    return Err(invalid());
                }
                if self
                    .tenant
                    .as_deref()
                    .is_some_and(|v| matches!(v, "common" | "organizations" | "consumers"))
                {
                    return Err(invalid());
                }
                if self.client_secret.as_ref().is_some_and(|v| v.is_empty()) {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
        if self.tenant.as_ref().is_some_and(|v| {
            !v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        }) {
            return Err(Error::Invalid(format!("{path}.tenant is invalid")));
        }
        for uri in &self.redirect_uris {
            let url = url::Url::parse(uri)
                .map_err(|_| Error::Invalid(format!("{path}.redirect_uris is invalid")))?;
            let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
            if !(url.scheme() == "https" || url.scheme() == "http" && local)
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err(Error::Invalid(format!(
                    "{path}.redirect_uris requires HTTPS or explicit loopback HTTP URLs"
                )));
            }
        }
        for (name, value) in [
            ("service_account_file", &mut self.service_account_file),
            ("certificate_file", &mut self.certificate_file),
            ("private_key_file", &mut self.private_key_file),
        ] {
            if let Some(file) = value {
                if file.is_relative() {
                    *file = base.join(&*file);
                }
                if !file.is_file() {
                    return Err(Error::Invalid(format!(
                        "{path}.{name} must reference a readable file"
                    )));
                }
                std::fs::File::open(file).map_err(|_| {
                    Error::Invalid(format!("{path}.{name} must reference a readable file"))
                })?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(text: &str) -> Result<OAuthConfig> {
        let mut config: OAuthConfig = serde_yaml::from_str(text)
            .map_err(|_| Error::Invalid("invalid test configuration".into()))?;
        config.validate(Path::new("."))?;
        Ok(config)
    }

    #[test]
    fn delegated_clients_allow_import_without_redirects() {
        for (provider, tenant) in [("google", ""), ("microsoft", "    tenant: common\n")] {
            let input = format!(
                "providers:\n  main:\n    provider: {provider}\n    mode: delegated\n    client_id: client\n    client_secret: secret\n{tenant}"
            );
            assert!(validate(&input).is_ok());
        }
    }

    #[test]
    fn rejects_incompatible_modes_and_secret_sources() {
        for fields in [
            "provider: google\n    mode: application\n    client_id: client\n    client_secret: secret",
            "provider: microsoft\n    mode: application\n    tenant: common\n    client_id: client\n    client_secret: secret",
            "provider: microsoft\n    mode: application\n    tenant: tenant\n    client_id: client",
            "provider: microsoft\n    mode: application\n    tenant: tenant\n    client_id: client\n    client_secret: secret\n    certificate_file: cert.pem\n    private_key_file: key.pem",
            "provider: google\n    mode: delegated\n    client_id: client\n    client_secret: secret\n    tenant: tenant",
            "provider: microsoft\n    mode: delegated\n    client_id: client\n    client_secret: secret\n    tenant: bad/tenant",
            "provider: google\n    mode: domain_wide_delegation\n    service_account_file: missing.json",
        ] {
            assert!(
                validate(&format!("providers:\n  main:\n    {fields}\n")).is_err(),
                "{fields}"
            );
        }
    }

    #[test]
    fn validates_redirect_urls_without_requiring_a_public_callback() {
        for (url, accepted) in [
            ("https://app.example.com/callback", true),
            ("http://localhost:8888/callback", true),
            ("http://127.0.0.1:8888/callback", true),
            ("http://[::1]:8888/callback", true),
            ("http://app.example.com/callback", false),
            ("https://app.example.com/callback#fragment", false),
            ("https://user:secret@app.example.com/callback", false),
        ] {
            let input = format!(
                "providers:\n  main:\n    provider: google\n    mode: delegated\n    client_id: client\n    client_secret: secret\n    redirect_uris: ['{url}']\n"
            );
            assert_eq!(validate(&input).is_ok(), accepted, "{url}");
        }
    }

    #[test]
    fn resolves_service_key_files_against_yaml_directory() {
        let dir = std::env::temp_dir().join(format!("rustbox-provider-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("key.json"), "{}").unwrap();
        let mut config: OAuthConfig = serde_yaml::from_str("providers:\n  service:\n    provider: google\n    mode: domain_wide_delegation\n    service_account_file: key.json\n").unwrap();
        config.validate(&dir).unwrap();
        assert_eq!(
            config.providers["service"].service_account_file,
            Some(dir.join("key.json"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
