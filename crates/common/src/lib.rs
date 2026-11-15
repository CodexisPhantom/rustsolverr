//! Data types shared by every rustsolverr crate.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum Error {
    /// The request did not finish inside `maxTimeout`.
    #[error("timeout")]
    Timeout,
    /// The site refused the browser (Cloudflare "Access denied").
    #[error("blocked")]
    Blocked,
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Fetch(String),
    #[error("{0}")]
    Browser(String),
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn session_expiry() -> f64 {
    -1.0
}

fn default_path() -> String {
    "/".into()
}

fn default_same_site() -> String {
    "None".into()
}

fn de_string<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

fn de_path<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?
        .filter(|s| !s.is_empty())
        .unwrap_or_else(default_path))
}

fn de_same_site<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?
        .filter(|s| !s.is_empty())
        .unwrap_or_else(default_same_site))
}

fn de_bool<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(d)?.unwrap_or(false))
}

fn de_expires<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrStr {
        Num(f64),
        Str(String),
    }
    Ok(match Option::<NumOrStr>::deserialize(d)? {
        Some(NumOrStr::Num(n)) => n,
        Some(NumOrStr::Str(s)) => s.trim().parse().unwrap_or_else(|_| session_expiry()),
        None => session_expiry(),
    })
}

/// A browser cookie. Field names follow the FlareSolverr / Playwright JSON shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cookie {
    pub name: String,
    #[serde(default, deserialize_with = "de_string")]
    pub value: String,
    /// Empty when the caller gave only name and value.
    #[serde(default, deserialize_with = "de_string")]
    pub domain: String,
    #[serde(default = "default_path", deserialize_with = "de_path")]
    pub path: String,
    /// Unix seconds; `-1` for a session cookie. Selenium calls this `expiry`.
    #[serde(
        default = "session_expiry",
        alias = "expiry",
        deserialize_with = "de_expires"
    )]
    pub expires: f64,
    #[serde(default, deserialize_with = "de_bool")]
    pub http_only: bool,
    #[serde(default, deserialize_with = "de_bool")]
    pub secure: bool,
    #[serde(default = "default_same_site", deserialize_with = "de_same_site")]
    pub same_site: String,
}

impl Cookie {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Cookie {
            name: name.into(),
            value: value.into(),
            domain: String::new(),
            path: default_path(),
            expires: session_expiry(),
            http_only: false,
            secure: false,
            same_site: default_same_site(),
        }
    }

    pub fn is_session(&self) -> bool {
        self.expires <= 0.0
    }

    pub fn is_expired(&self, now: f64) -> bool {
        !self.is_session() && self.expires <= now
    }

    /// True when the cookie may be sent to `host`. A cookie without a domain
    /// was supplied for the request URL, so it always matches.
    pub fn matches_host(&self, host: &str) -> bool {
        let domain = self.domain.trim_start_matches('.');
        domain.is_empty()
            || host.eq_ignore_ascii_case(domain)
            || host
                .to_ascii_lowercase()
                .ends_with(&format!(".{}", domain.to_ascii_lowercase()))
    }
}

/// Merges `newer` into `base`; a cookie in `newer` replaces one with the same name.
pub fn merge_cookies(base: &[Cookie], newer: &[Cookie]) -> Vec<Cookie> {
    let mut out: Vec<Cookie> = base
        .iter()
        .filter(|c| !newer.iter().any(|n| n.name == c.name))
        .cloned()
        .collect();
    out.extend(newer.iter().cloned());
    out
}

/// Upstream proxy. The same proxy must serve the browser solve and the replay,
/// because clearance is bound to the exit IP.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Proxy {
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl Proxy {
    /// Extracts `user:pass@` embedded in `url` into `username` and `password`
    /// and strips it from `url`.
    pub fn normalized(mut self) -> Self {
        if let Some((scheme, rest)) = self.url.split_once("://") {
            let authority_len = rest.find('/').unwrap_or(rest.len());
            let authority = &rest[..authority_len];
            let path = &rest[authority_len..];
            if let Some((userinfo, host_port)) = authority.rsplit_once('@') {
                let (u, p) = match userinfo.split_once(':') {
                    Some((u, p)) => (u, Some(p)),
                    None => (userinfo, None),
                };
                if self.username.is_none() && !u.is_empty() {
                    self.username = Some(u.to_string());
                }
                if self.password.is_none() {
                    if let Some(p) = p.filter(|p| !p.is_empty()) {
                        self.password = Some(p.to_string());
                    }
                }
                self.url = format!("{scheme}://{host_port}{path}");
            }
        }
        self
    }

    /// Identifies the exit point for cache keys. Never contains the password.
    pub fn key(&self) -> String {
        let clean = self.clone().normalized();
        format!("{}@{}", clean.username.as_deref().unwrap_or(""), clean.url)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// One page load, done either by the HTTP client or by the browser.
#[derive(Debug, Clone)]
pub struct Request {
    pub url: String,
    pub method: Method,
    /// `application/x-www-form-urlencoded` body for `Method::Post`.
    pub post_data: Option<String>,
    pub cookies: Vec<Cookie>,
    /// `None`: the executor uses its own user-agent.
    pub user_agent: Option<String>,
    pub proxy: Option<Proxy>,
    pub timeout: Duration,
    /// Extra wait after the page is ready (browser only).
    pub wait: Duration,
}

impl Request {
    pub fn get(url: impl Into<String>) -> Self {
        Request {
            url: url.into(),
            method: Method::Get,
            post_data: None,
            cookies: Vec::new(),
            user_agent: None,
            proxy: None,
            timeout: Duration::from_secs(60),
            wait: Duration::ZERO,
        }
    }
}

/// Result of a page load.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Page {
    pub url: String,
    pub status: u16,
    /// Lower-case header names.
    pub headers: BTreeMap<String, String>,
    pub body: String,
    /// HTTP client: cookies set by the response. Browser: every cookie of the context.
    pub cookies: Vec<Cookie>,
    /// The user-agent that was sent.
    pub user_agent: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_accepts_name_value_only() {
        let c: Cookie = serde_json::from_str(r#"{"name":"a","value":"b"}"#).unwrap();
        assert_eq!(c, Cookie::new("a", "b"));
        assert!(c.is_session());
        assert!(!c.is_expired(1e12));
    }

    #[test]
    fn cookie_accepts_selenium_expiry() {
        let c: Cookie =
            serde_json::from_str(r#"{"name":"a","value":"b","expiry":100,"httpOnly":true}"#)
                .unwrap();
        assert_eq!(c.expires, 100.0);
        assert!(c.http_only);
        assert!(c.is_expired(101.0));
        assert!(!c.is_expired(99.0));
    }

    #[test]
    fn cookie_host_matching() {
        let mut c = Cookie::new("a", "b");
        assert!(c.matches_host("anything.test"));
        c.domain = ".example.com".into();
        assert!(c.matches_host("example.com"));
        assert!(c.matches_host("www.Example.com"));
        assert!(!c.matches_host("badexample.com"));
    }

    #[test]
    fn merge_replaces_by_name() {
        let merged = merge_cookies(
            &[Cookie::new("a", "1"), Cookie::new("b", "2")],
            &[Cookie::new("a", "3")],
        );
        assert_eq!(merged, vec![Cookie::new("b", "2"), Cookie::new("a", "3")]);
    }

    #[test]
    fn cookie_accepts_null_fields() {
        let c: Cookie = serde_json::from_str(
            r#"{"name":"a","value":"b","domain":null,"path":null,"expires":null,"httpOnly":null,"secure":null,"sameSite":null}"#,
        )
        .unwrap();
        assert_eq!(c, Cookie::new("a", "b"));
    }

    #[test]
    fn proxy_normalizes_embedded_credentials_and_hides_password() {
        let p = Proxy {
            url: "socks5://alice:secret@10.0.0.1:1080".into(),
            username: None,
            password: None,
        };
        assert_eq!(p.key(), "alice@socks5://10.0.0.1:1080");
        assert!(!p.key().contains("secret"));
        let n = p.normalized();
        assert_eq!(n.url, "socks5://10.0.0.1:1080");
        assert_eq!(n.username.as_deref(), Some("alice"));
        assert_eq!(n.password.as_deref(), Some("secret"));
    }
}
