//! Plain HTTP fetch that impersonates a browser's TLS and HTTP/2 fingerprint.

use async_trait::async_trait;
use rustsolverr_common::{Page, Request, Result};

#[cfg(feature = "wreq")]
mod wreq_fetcher;
#[cfg(feature = "wreq")]
pub use wreq_fetcher::WreqFetcher;

/// User-agent for a fetch that has no cached clearance. Firefox, because the
/// default browser backend (Camoufox) is Firefox.
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:151.0) Gecko/20100101 Firefox/151.0";

#[async_trait]
pub trait Fetcher: Send + Sync {
    /// Loads `req.url` without a browser. Sends `req.user_agent` when set,
    /// else [`DEFAULT_USER_AGENT`], and reports the sent value in `Page::user_agent`.
    async fn fetch(&self, req: &Request) -> Result<Page>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Firefox,
    Chrome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
}

/// What a user-agent says about the browser. Selects the TLS/HTTP2 profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub family: Family,
    /// Major version; 0 when the user-agent has none.
    pub version: u32,
    pub os: Os,
}

impl Fingerprint {
    pub fn from_user_agent(ua: &str) -> Self {
        let major = |token: &str| -> Option<u32> {
            let rest = &ua[ua.find(token)? + token.len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        };
        let (family, version) = match (major("Firefox/"), major("Chrome/")) {
            (Some(v), _) => (Family::Firefox, v),
            (None, Some(v)) => (Family::Chrome, v),
            (None, None) => (Family::Firefox, 0),
        };
        let os = if ua.contains("Windows") {
            Os::Windows
        } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
            Os::MacOs
        } else {
            Os::Linux
        };
        Fingerprint {
            family,
            version,
            os,
        }
    }
}

/// Picks the profile for `wanted` from `available` (sorted ascending by
/// version): the highest version not above `wanted`, else the lowest.
pub fn nearest<T: Copy>(available: &[(u32, T)], wanted: u32) -> T {
    available
        .iter()
        .rev()
        .find(|(v, _)| *v <= wanted)
        .unwrap_or(&available[0])
        .1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_firefox() {
        let fp = Fingerprint::from_user_agent(DEFAULT_USER_AGENT);
        assert_eq!(
            fp,
            Fingerprint {
                family: Family::Firefox,
                version: 151,
                os: Os::Windows
            }
        );
    }

    #[test]
    fn parses_chrome() {
        let fp = Fingerprint::from_user_agent(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36",
        );
        assert_eq!(
            fp,
            Fingerprint {
                family: Family::Chrome,
                version: 142,
                os: Os::Linux
            }
        );
    }

    #[test]
    fn parses_mac_and_unknown() {
        let fp = Fingerprint::from_user_agent(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:135.0) Gecko/20100101 Firefox/135.0",
        );
        assert_eq!((fp.version, fp.os), (135, Os::MacOs));
        let fp = Fingerprint::from_user_agent("curl/8");
        assert_eq!((fp.family, fp.version), (Family::Firefox, 0));
    }

    #[test]
    fn nearest_prefers_not_newer() {
        let table = [(128, 'a'), (135, 'b'), (151, 'c')];
        assert_eq!(nearest(&table, 135), 'b');
        assert_eq!(nearest(&table, 140), 'b');
        assert_eq!(nearest(&table, 999), 'c');
        assert_eq!(nearest(&table, 100), 'a');
    }
}
