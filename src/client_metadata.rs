use crate::core::now_ms;
use futures::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_DOCUMENT_BYTES: usize = 64 * 1024;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

const MAX_CACHED_CLIENTS: usize = 256;

const MAX_DOWNLOADS_PER_MINUTE: usize = 20;

const CACHE_FOR_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug)]
pub struct Metadata {
    pub host: String,
    pub name: Option<String>,
    pub redirect_uris: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ClientIdentity {
    pub name: String,
    pub verified_host: Option<String>,
}

impl ClientIdentity {
    #[must_use]
    pub fn label(&self) -> String {
        match &self.verified_host {
            Some(_) => self.name.clone(),
            None => format!("{} (name not verified)", self.name),
        }
    }
}

#[derive(Deserialize)]
struct Document {
    redirect_uris: Vec<String>,
    client_name: Option<String>,
}

fn private_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, d] = address.octets();

    a == 0
        || a == 10
        || (a == 100 && (64..128).contains(&b))
        || a == 127
        || (a == 169 && b == 254)
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224
        || (a == 255 && b == 255 && c == 255 && d == 255)
}

fn private_v6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return private_v4(mapped);
    }

    let segments = address.segments();
    let first = segments.first().copied().unwrap_or_default();
    let second = segments.get(1).copied().unwrap_or_default();
    let third = segments.get(2).copied().unwrap_or_default();
    let [second_high, second_low] = second.to_be_bytes();
    let [third_high, third_low] = third.to_be_bytes();
    let six_to_four = Ipv4Addr::from([second_high, second_low, third_high, third_low]);
    let nat64_high = segments.get(6).copied().unwrap_or_default();
    let nat64_low = segments.get(7).copied().unwrap_or_default();
    let [nat64_high_high, nat64_high_low] = nat64_high.to_be_bytes();
    let [nat64_low_high, nat64_low_low] = nat64_low.to_be_bytes();
    let nat64 = Ipv4Addr::from([nat64_high_high, nat64_high_low, nat64_low_high, nat64_low_low]);

    address.is_unspecified()
        || address.is_loopback()
        || (first & 0xe000) != 0x2000
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first & 0xffc0) == 0xfec0
        || (first & 0xff00) == 0xff00
        || (first == 0x2001 && second <= 0x01ff)
        || (first == 0x2001 && second == 0x0db8)
        || (first == 0x3fff && (second & 0xf000) == 0)
        || (first == 0x2002 && private_v4(six_to_four))
        || (first == 0x0064 && second == 0xff9b && private_v4(nat64))
}
fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => !private_v4(v4),
        IpAddr::V6(v6) => !private_v6(v6),
    }
}

pub fn is_valid_redirect_uri(uri: &str) -> bool {
    url::Url::parse(uri).is_ok_and(|url| {
        url.scheme() == "https" || (url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost")))
    })
}

pub fn redirect_matches(registered: &str, requested: &str) -> bool {
    if registered == requested {
        return true;
    }

    let (Ok(registered), Ok(requested)) = (url::Url::parse(registered), url::Url::parse(requested)) else {
        return false;
    };
    let loopback = |url: &url::Url| url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost"));

    loopback(&registered)
        && loopback(&requested)
        && registered.host_str() == requested.host_str()
        && registered.path() == requested.path()
        && registered.query() == requested.query()
        && requested.username().is_empty()
        && requested.password().is_none()
        && requested.fragment().is_none()
}

async fn public_addresses(host: &str, port: u16) -> Option<Vec<SocketAddr>> {
    let mut addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await.ok()?.collect();

    if addresses.is_empty() || !addresses.iter().all(|address| is_public(address.ip())) {
        return None;
    }

    addresses.sort_by_key(SocketAddr::is_ipv6);

    Some(addresses)
}

async fn download(client_id: &str) -> Option<String> {
    let url = url::Url::parse(client_id).ok()?;
    let host = url.host_str()?.trim_start_matches('[').trim_end_matches(']').to_owned();
    let addresses = public_addresses(&host, url.port_or_known_default()?).await?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(&host, &addresses)
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()?;
    let response = client.get(url).send().await.ok()?;

    if response.status() != reqwest::StatusCode::OK {
        return None;
    }

    let mut body = response.bytes_stream();
    let mut bytes = Vec::new();

    while let Some(chunk) = body.next().await {
        let chunk = chunk.ok()?;

        if bytes.len() + chunk.len() > MAX_DOCUMENT_BYTES {
            return None;
        }

        bytes.extend_from_slice(&chunk);
    }

    String::from_utf8(bytes).ok()
}

async fn document_of(client_id: &str) -> Option<Metadata> {
    let text = tokio::time::timeout(FETCH_TIMEOUT, download(client_id)).await.ok()??;
    let document: Document = serde_json::from_str(&text).ok()?;

    let host = url::Url::parse(client_id).ok().map(|url| match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    })?;

    if !document.redirect_uris.iter().all(|uri| is_valid_redirect_uri(uri)) {
        return None;
    }

    Some(Metadata { host, name: document.client_name, redirect_uris: document.redirect_uris })
}

#[derive(Clone, Default)]
pub struct ClientMetadata {
    cache: Arc<Mutex<HashMap<String, (i64, Metadata)>>>,
    downloads: Arc<Mutex<Vec<i64>>>,
}

impl ClientMetadata {
    fn cached(&self, client_id: &str) -> Option<Metadata> {
        let now = now_ms();

        self.cache.lock().ok().and_then(|cache| {
            cache.get(client_id).filter(|(at, _)| now - at < CACHE_FOR_MS).map(|(_, found)| found.clone())
        })
    }

    fn take_download_slot(&self) -> bool {
        let now = now_ms();

        self.downloads.lock().is_ok_and(|mut recent| {
            recent.retain(|at| *at >= now - 60_000);

            if recent.len() >= MAX_DOWNLOADS_PER_MINUTE {
                return false;
            }

            recent.push(now);
            true
        })
    }

    pub async fn fetch(&self, client_id: &str) -> Option<Metadata> {
        if !client_id.starts_with("https://") || url::Url::parse(client_id).is_err() {
            return None;
        }

        if let Some(found) = self.cached(client_id) {
            return Some(found);
        }

        if !self.take_download_slot() {
            return None;
        }

        let found = document_of(client_id).await?;

        if let Ok(mut cache) = self.cache.lock() {
            if cache.len() >= MAX_CACHED_CLIENTS {
                cache.clear();
            }

            cache.insert(client_id.to_owned(), (now_ms(), found.clone()));
        }

        Some(found)
    }

    pub async fn identify(&self, client_id: &str, fallback_name: String) -> ClientIdentity {
        match self.fetch(client_id).await {
            Some(found) => ClientIdentity {
                name: found.name.unwrap_or_else(|| found.host.clone()),
                verified_host: Some(found.host),
            },
            None => ClientIdentity { name: fallback_name, verified_host: None },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn client_metadata_is_never_fetched_from_a_host_that_resolves_to_a_private_address() {
        let listener = tokio::net::TcpListener::bind("[::]:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = connections.clone();
        tokio::spawn(async move {
            while listener.accept().await.is_ok() {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let metadata = ClientMetadata::default();

        for host in ["localhost", "127.0.0.1", "[::1]", "[::ffff:127.0.0.1]"] {
            assert!(metadata.fetch(&format!("https://{host}:{port}/client.json")).await.is_none());
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
    #[test]
    fn special_use_addresses_are_not_public() {
        for address in [
            IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
            IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1)),
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)),
            IpAddr::V6(Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0xc000, 0x0201)),
            IpAddr::V6(Ipv6Addr::new(0x2002, 0xc000, 0x0201, 0, 0, 0, 0, 0)),
        ] {
            assert!(!is_public(address));
        }
    }

    #[test]
    fn client_metadata_redirects_use_registration_validation() {
        assert!(is_valid_redirect_uri("https://client.example/callback"));
        assert!(!is_valid_redirect_uri("javascript:alert(1)"));
    }
}
