//! Clearance cache: the cookies and user-agent a browser solve produced,
//! kept per cache key, in memory, with optional persistence to a JSON file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use rustsolverr_common::{now_secs, Cookie};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clearance {
    pub cookies: Vec<Cookie>,
    pub user_agent: String,
    /// Unix seconds.
    pub stored_at: f64,
}

pub struct CookieCache {
    ttl: Duration,
    path: Option<PathBuf>,
    entries: Mutex<HashMap<String, Clearance>>,
}

impl CookieCache {
    /// Loads `path` when it exists. A missing or unreadable file gives an empty cache.
    pub fn new(ttl: Duration, path: Option<PathBuf>) -> Self {
        let entries = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| match serde_json::from_slice(&bytes) {
                Ok(map) => Some(map),
                Err(e) => {
                    tracing::warn!("ignoring unreadable cookie cache file: {e}");
                    None
                }
            })
            .unwrap_or_default();
        CookieCache {
            ttl,
            path,
            entries: Mutex::new(entries),
        }
    }

    pub fn get(&self, key: &str) -> Option<Clearance> {
        self.get_at(key, now_secs())
    }

    /// Entry for `key` as seen at time `now`. Drops expired cookies; an entry
    /// past the TTL, or with no cookie left, is removed.
    pub fn get_at(&self, key: &str, now: f64) -> Option<Clearance> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get_mut(key)?;
        entry.cookies.retain(|c| !c.is_expired(now));
        if entry.cookies.is_empty() || now - entry.stored_at >= self.ttl.as_secs_f64() {
            entries.remove(key);
            self.persist(&entries);
            return None;
        }
        Some(entry.clone())
    }

    pub fn put(&self, key: &str, cookies: Vec<Cookie>, user_agent: String) {
        self.put_at(key, cookies, user_agent, now_secs());
    }

    pub fn put_at(&self, key: &str, cookies: Vec<Cookie>, user_agent: String, now: f64) {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(
            key.to_string(),
            Clearance {
                cookies,
                user_agent,
                stored_at: now,
            },
        );
        self.persist(&entries);
    }

    pub fn invalidate(&self, key: &str) {
        let mut entries = self.entries.lock().unwrap();
        if entries.remove(key).is_some() {
            self.persist(&entries);
        }
    }

    /// Writes the whole map; a temporary file plus rename keeps the old file
    /// intact if the write fails. A persistence failure only logs: the cache
    /// still works in memory.
    fn persist(&self, entries: &HashMap<String, Clearance>) {
        let Some(path) = &self.path else { return };
        let tmp = path.with_extension("tmp");
        let result = serde_json::to_vec(entries)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = result {
            tracing::warn!("cannot persist cookie cache to {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, expires: f64) -> Cookie {
        Cookie {
            expires,
            ..Cookie::new(name, "v")
        }
    }

    #[test]
    fn entry_expires_after_ttl() {
        let cache = CookieCache::new(Duration::from_secs(100), None);
        cache.put_at("k", vec![cookie("a", -1.0)], "UA".into(), 1000.0);
        let hit = cache.get_at("k", 1099.0).unwrap();
        assert_eq!(hit.user_agent, "UA");
        assert_eq!(hit.cookies.len(), 1);
        assert!(cache.get_at("k", 1100.0).is_none());
        assert!(
            cache.get_at("k", 1000.0).is_none(),
            "expired entry is removed"
        );
    }

    #[test]
    fn expired_cookies_are_dropped() {
        let cache = CookieCache::new(Duration::from_secs(1000), None);
        cache.put_at(
            "k",
            vec![cookie("short", 1050.0), cookie("long", 5000.0)],
            "UA".into(),
            1000.0,
        );
        let names = |now| -> Vec<String> {
            cache
                .get_at("k", now)
                .map(|c| c.cookies.into_iter().map(|c| c.name).collect())
                .unwrap_or_default()
        };
        assert_eq!(names(1010.0), ["short", "long"]);
        assert_eq!(names(1060.0), ["long"]);
    }

    #[test]
    fn entry_without_live_cookie_is_removed() {
        let cache = CookieCache::new(Duration::from_secs(1000), None);
        cache.put_at("k", vec![cookie("a", 1050.0)], "UA".into(), 1000.0);
        assert!(cache.get_at("k", 1051.0).is_none());
    }

    #[test]
    fn invalidate_removes_entry() {
        let cache = CookieCache::new(Duration::from_secs(1000), None);
        cache.put_at("k", vec![cookie("a", -1.0)], "UA".into(), 1000.0);
        cache.invalidate("k");
        assert!(cache.get_at("k", 1001.0).is_none());
    }

    #[test]
    fn persists_to_file() {
        let path =
            std::env::temp_dir().join(format!("rustsolverr-cache-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let ttl = Duration::from_secs(1000);
        CookieCache::new(ttl, Some(path.clone())).put_at(
            "k",
            vec![cookie("a", -1.0)],
            "UA".into(),
            1000.0,
        );
        let reloaded = CookieCache::new(ttl, Some(path.clone()));
        assert_eq!(reloaded.get_at("k", 1001.0).unwrap().user_agent, "UA");
        reloaded.invalidate("k");
        assert!(CookieCache::new(ttl, Some(path.clone()))
            .get_at("k", 1001.0)
            .is_none());
        std::fs::remove_file(&path).unwrap();
    }
}
