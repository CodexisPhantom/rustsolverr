//! Runtime configuration, read from environment variables.

use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rustsolverr_common::Proxy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Camoufox,
    Chromium,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub host: IpAddr,
    pub port: u16,
    pub log_level: String,
    pub max_concurrent_solves: usize,
    pub browser_idle_timeout: Duration,
    pub cookie_cache_ttl: Duration,
    /// Default proxy for requests that carry none.
    pub proxy: Option<Proxy>,
    pub backend: Backend,
    /// JSON file that persists the cookie cache. `None`: memory only.
    pub cache_file: Option<PathBuf>,
    /// `true`, `false` or `virtual` (Camoufox under Xvfb, Linux only).
    pub headless: String,
    /// Python interpreter that has the `camoufox` package.
    pub python: String,
    /// Chromium executable for the chromium backend. `None`: auto-detect.
    pub chrome_path: Option<PathBuf>,
    /// Extra Chromium command line flags, separated by spaces.
    pub chrome_args: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// `lookup` returns the value of one variable. An empty value counts as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());

        fn parse<T: FromStr>(name: &str, value: Option<String>, default: T) -> Result<T>
        where
            T::Err: std::error::Error + Send + Sync + 'static,
        {
            match value {
                Some(v) => v
                    .trim()
                    .parse()
                    .with_context(|| format!("invalid {name}: {v:?}")),
                None => Ok(default),
            }
        }
        let secs = |name: &str, default: u64| -> Result<Duration> {
            Ok(Duration::from_secs(parse(name, get(name), default)?))
        };

        let backend = match get("BROWSER_BACKEND")
            .unwrap_or_else(|| "camoufox".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "camoufox" => Backend::Camoufox,
            "chromium" | "chrome" => Backend::Chromium,
            other => bail!("invalid BROWSER_BACKEND: {other:?} (camoufox or chromium)"),
        };
        let max_concurrent_solves =
            parse("MAX_CONCURRENT_SOLVES", get("MAX_CONCURRENT_SOLVES"), 2)?;
        if max_concurrent_solves == 0 {
            bail!("MAX_CONCURRENT_SOLVES must be at least 1");
        }

        Ok(Config {
            host: parse("HOST", get("HOST"), IpAddr::from([0, 0, 0, 0]))?,
            port: parse("PORT", get("PORT"), 8191)?,
            log_level: get("LOG_LEVEL").unwrap_or_else(|| "info".into()),
            max_concurrent_solves,
            browser_idle_timeout: secs("BROWSER_IDLE_TIMEOUT", 300)?,
            cookie_cache_ttl: secs("COOKIE_CACHE_TTL", 86400)?,
            proxy: get("PROXY_URL").map(|url| Proxy {
                url,
                username: get("PROXY_USERNAME"),
                password: get("PROXY_PASSWORD"),
            }),
            backend,
            cache_file: get("CACHE_FILE").map(PathBuf::from),
            headless: get("HEADLESS")
                .unwrap_or_else(|| "true".into())
                .to_ascii_lowercase(),
            python: get("PYTHON").unwrap_or_else(|| "python3".into()),
            chrome_path: get("CHROME_PATH").map(PathBuf::from),
            chrome_args: get("CHROME_ARGS")
                .map(|v| v.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(pairs: &[(&str, &str)]) -> Result<Config> {
        Config::from_lookup(|name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn defaults() {
        let c = from(&[]).unwrap();
        assert_eq!(c.port, 8191);
        assert_eq!(c.backend, Backend::Camoufox);
        assert_eq!(c.max_concurrent_solves, 2);
        assert_eq!(c.browser_idle_timeout, Duration::from_secs(300));
        assert_eq!(c.proxy, None);
        assert_eq!(c.cache_file, None);
    }

    #[test]
    fn overrides() {
        let c = from(&[
            ("PORT", "9000"),
            ("BROWSER_BACKEND", "Chromium"),
            ("COOKIE_CACHE_TTL", "60"),
            ("PROXY_URL", "socks5://10.0.0.1:1080"),
            ("PROXY_USERNAME", "u"),
            ("CACHE_FILE", "/config/cookies.json"),
        ])
        .unwrap();
        assert_eq!(c.port, 9000);
        assert_eq!(c.backend, Backend::Chromium);
        assert_eq!(c.cookie_cache_ttl, Duration::from_secs(60));
        let proxy = c.proxy.unwrap();
        assert_eq!(proxy.url, "socks5://10.0.0.1:1080");
        assert_eq!(proxy.username.as_deref(), Some("u"));
        assert_eq!(proxy.password, None);
        assert_eq!(c.cache_file, Some(PathBuf::from("/config/cookies.json")));
    }

    #[test]
    fn rejects_bad_values() {
        assert!(from(&[("PORT", "abc")]).is_err());
        assert!(from(&[("BROWSER_BACKEND", "netscape")]).is_err());
        assert!(from(&[("MAX_CONCURRENT_SOLVES", "0")]).is_err());
    }

    #[test]
    fn empty_value_is_unset() {
        assert_eq!(from(&[("PROXY_URL", " ")]).unwrap().proxy, None);
    }
}
