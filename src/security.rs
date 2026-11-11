use crate::error::{Error, Result};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..=127).contains(&b))
        || (a == 198 && (b == 18 || b == 19))
        || (a == 192 && b == 0 && c == 0))
}

pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && s[0] != 0x2002
        }
    }
}

pub async fn resolve_public(host: &str, port: u16, allow_private: bool) -> Result<Vec<SocketAddr>> {
    if host.is_empty() || port == 0 {
        return Err(Error::Invalid("invalid destination".into()));
    }
    let addresses: Vec<_> = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| Error::Unavailable("DNS timeout".into()))?
    .map_err(|_| Error::Unavailable("DNS lookup failed".into()))?
    .collect();
    if addresses.is_empty() {
        return Err(Error::Unavailable("destination has no addresses".into()));
    }
    if !allow_private && addresses.iter().any(|a| !is_public(a.ip())) {
        return Err(Error::Invalid(
            "private or reserved network destination blocked".into(),
        ));
    }
    Ok(addresses)
}

pub async fn connect(host: &str, port: u16, allow_private: bool) -> Result<TcpStream> {
    let addresses = resolve_public(host, port, allow_private).await?;
    tokio::time::timeout(
        Duration::from_secs(15),
        TcpStream::connect(addresses.as_slice()),
    )
    .await
    .map_err(|_| Error::Unavailable("connection timeout".into()))?
    .map_err(|_| Error::Unavailable("connection failed".into()))
}

pub async fn tls(
    stream: TcpStream,
    host: &str,
    ca_file: Option<&Path>,
) -> Result<TlsStream<TcpStream>> {
    let mut roots =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = extra_ca_pem(ca_file).await? {
        use rustls::pki_types::pem::PemObject;
        for cert in rustls::pki_types::CertificateDer::pem_slice_iter(&pem) {
            roots
                .add(cert.map_err(|_| {
                    Error::Invalid("invalid security.tls_ca_file certificate".into())
                })?)
                .map_err(|_| Error::Invalid("invalid security.tls_ca_file root".into()))?;
        }
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|_| Error::Invalid("invalid TLS hostname".into()))?;
    tokio::time::timeout(
        Duration::from_secs(15),
        TlsConnector::from(Arc::new(config)).connect(name, stream),
    )
    .await
    .map_err(|_| Error::Unavailable("TLS timeout".into()))?
    .map_err(|_| Error::Unavailable("TLS verification or handshake failed".into()))
}

pub async fn safe_http_client(
    url: &str,
    allow_private: bool,
    ca_file: Option<&Path>,
) -> Result<reqwest::Client> {
    let parsed = url::Url::parse(url).map_err(|_| Error::Invalid("invalid webhook URL".into()))?;
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(Error::Invalid(
            "webhook URL must use HTTPS without credentials or fragment".into(),
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::Invalid("missing webhook host".into()))?;
    let addresses = resolve_public(
        host.trim_matches(['[', ']']),
        parsed.port_or_known_default().unwrap_or(443),
        allow_private,
    )
    .await?;
    let mut builder = reqwest::Client::builder();
    if let Some(pem) = extra_ca_pem(ca_file).await? {
        builder = builder.add_root_certificate(
            reqwest::Certificate::from_pem(&pem)
                .map_err(|_| Error::Invalid("invalid security.tls_ca_file certificate".into()))?,
        );
    }
    builder
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| Error::Internal("HTTP client initialization failed".into()))
}

pub async fn extra_ca_pem(ca_file: Option<&Path>) -> Result<Option<Vec<u8>>> {
    match ca_file {
        Some(path) => tokio::fs::read(path)
            .await
            .map(Some)
            .map_err(|_| Error::Invalid("cannot read security.tls_ca_file".into())),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_reserved_destinations() {
        for address in [
            "127.0.0.1",
            "10.1.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "192.0.2.2",
            "::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
            "2002:7f00:1::",
        ] {
            assert!(!is_public(address.parse().unwrap()), "{address}");
        }
        assert!(is_public("8.8.8.8".parse().unwrap()));
        assert!(is_public("2606:4700:4700::1111".parse().unwrap()));
    }
}
