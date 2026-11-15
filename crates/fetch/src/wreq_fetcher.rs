use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use rustsolverr_common::{now_secs, Cookie, Error, Method, Page, Request, Result};
use wreq::header::{HeaderMap, HeaderValue, CONTENT_TYPE, COOKIE, USER_AGENT};
use wreq_util::{Emulation, Platform, Profile};

use crate::{nearest, Family, Fetcher, Fingerprint, Os, DEFAULT_USER_AGENT};

const FIREFOX: &[(u32, Profile)] = &[
    (109, Profile::Firefox109),
    (117, Profile::Firefox117),
    (128, Profile::Firefox128),
    (133, Profile::Firefox133),
    (135, Profile::Firefox135),
    (136, Profile::Firefox136),
    (139, Profile::Firefox139),
    (142, Profile::Firefox142),
    (143, Profile::Firefox143),
    (144, Profile::Firefox144),
    (145, Profile::Firefox145),
    (146, Profile::Firefox146),
    (147, Profile::Firefox147),
    (148, Profile::Firefox148),
    (149, Profile::Firefox149),
    (150, Profile::Firefox150),
    (151, Profile::Firefox151),
];
const CHROME: &[(u32, Profile)] = &[
    (120, Profile::Chrome120),
    (124, Profile::Chrome124),
    (128, Profile::Chrome128),
    (131, Profile::Chrome131),
    (133, Profile::Chrome133),
    (135, Profile::Chrome135),
    (137, Profile::Chrome137),
    (140, Profile::Chrome140),
    (141, Profile::Chrome141),
    (142, Profile::Chrome142),
    (143, Profile::Chrome143),
    (144, Profile::Chrome144),
    (145, Profile::Chrome145),
    (146, Profile::Chrome146),
    (147, Profile::Chrome147),
    (148, Profile::Chrome148),
    (149, Profile::Chrome149),
];

/// TLS/HTTP2 profile that matches the browser a user-agent names.
fn emulation_for(user_agent: &str) -> Emulation {
    let fp = Fingerprint::from_user_agent(user_agent);
    let table = match fp.family {
        Family::Firefox => FIREFOX,
        Family::Chrome => CHROME,
    };
    // Version 0 (unknown) gets the newest profile.
    let wanted = if fp.version == 0 {
        u32::MAX
    } else {
        fp.version
    };
    Emulation::builder()
        .profile(nearest(table, wanted))
        .platform(match fp.os {
            Os::Windows => Platform::Windows,
            Os::MacOs => Platform::MacOS,
            Os::Linux => Platform::Linux,
        })
        .build()
}

/// [`Fetcher`] built on `wreq` (BoringSSL with browser TLS/HTTP2 profiles).
pub struct WreqFetcher {
    client: wreq::Client,
}

impl WreqFetcher {
    pub fn new() -> Result<Self> {
        let client = wreq::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Fetch(e.to_string()))?;
        Ok(WreqFetcher { client })
    }
}

#[async_trait]
impl Fetcher for WreqFetcher {
    async fn fetch(&self, req: &Request) -> Result<Page> {
        let fail = |e: wreq::Error| Error::Fetch(e.to_string());
        let host = url::Url::parse(&req.url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .and_then(|u| u.host_str().map(str::to_string))
            .ok_or_else(|| Error::Invalid(format!("invalid url: {}", req.url)))?;
        let user_agent = req.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT);

        let method = match req.method {
            Method::Get => wreq::Method::GET,
            Method::Post => wreq::Method::POST,
        };
        // `RequestBuilder::header` appends, and the emulation already sets a
        // User-Agent. `headers` replaces, so the explicit values win.
        let header = |v: &str| HeaderValue::from_str(v).map_err(|e| Error::Invalid(e.to_string()));
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, header(user_agent)?);

        let now = now_secs();
        let cookie_header = req
            .cookies
            .iter()
            .filter(|c| c.matches_host(&host) && !c.is_expired(now))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ");
        if !cookie_header.is_empty() {
            headers.insert(COOKIE, header(&cookie_header)?);
        }
        if req.method == Method::Post {
            headers.insert(CONTENT_TYPE, header("application/x-www-form-urlencoded")?);
        }
        let mut builder = self
            .client
            .request(method, req.url.as_str())
            .emulation(emulation_for(user_agent))
            .headers(headers)
            .timeout(req.timeout);
        if let Some(proxy) = req.proxy.clone().map(rustsolverr_common::Proxy::normalized) {
            let mut p = wreq::Proxy::all(proxy.url.as_str()).map_err(fail)?;
            if let Some(username) = &proxy.username {
                p = p.basic_auth(username, proxy.password.as_deref().unwrap_or(""));
            }
            builder = builder.proxy(p);
        }
        if req.method == Method::Post {
            builder = builder.body(req.post_data.clone().unwrap_or_default());
        }

        let resp = builder.send().await.map_err(fail)?;
        let mut page = Page {
            url: resp.uri().to_string(),
            status: resp.status().as_u16(),
            user_agent: user_agent.to_string(),
            ..Page::default()
        };
        for (name, value) in resp.headers() {
            let value = String::from_utf8_lossy(value.as_bytes());
            page.headers
                .entry(name.as_str().to_string())
                .and_modify(|v| {
                    v.push_str(", ");
                    v.push_str(&value);
                })
                .or_insert_with(|| value.to_string());
        }
        page.cookies = resp
            .cookies()
            .map(|c| Cookie {
                name: c.name().to_string(),
                value: c.value().to_string(),
                domain: c.domain().unwrap_or(&host).to_string(),
                path: c.path().unwrap_or("/").to_string(),
                expires: c
                    .max_age()
                    .map(|d| now + d.as_secs_f64())
                    .or_else(|| {
                        let since = c.expires()?.duration_since(UNIX_EPOCH).ok()?;
                        Some(since.as_secs_f64())
                    })
                    .unwrap_or(-1.0),
                http_only: c.http_only(),
                secure: c.secure(),
                same_site: if c.same_site_strict() {
                    "Strict"
                } else if c.same_site_lax() {
                    "Lax"
                } else {
                    "None"
                }
                .to_string(),
            })
            .collect();
        page.body = resp.text().await.map_err(fail)?;
        Ok(page)
    }
}
