//! Tiered execution, cheapest first:
//!
//! * tier a, cached clearance: replay with the cached cookies and user-agent
//!   through the HTTP client. No browser.
//! * tier b, plain fetch: no cache entry, so try a direct fetch.
//! * tier c, browser solve: a challenge was detected. Solve it in the
//!   browser and cache the cookies and user-agent.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustsolverr_browser::BrowserBackend;
use rustsolverr_cache::CookieCache;
use rustsolverr_challenge::{detect, Verdict};
use rustsolverr_common::{merge_cookies, now_secs, Cookie, Error, Page, Proxy, Request, Result};
use rustsolverr_fetch::{Fetcher, DEFAULT_USER_AGENT};
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Cached,
    Plain,
    Browser,
}

#[derive(Debug, Clone)]
pub struct Solved {
    pub page: Page,
    pub tier: Tier,
}

/// A FlareSolverr session: a named cookie jar with an optional proxy.
struct Session {
    cookies: Vec<Cookie>,
    proxy: Option<Proxy>,
    created: Instant,
}

impl Session {
    fn new(proxy: Option<Proxy>) -> Self {
        Session {
            cookies: Vec::new(),
            proxy,
            created: Instant::now(),
        }
    }
}

pub struct Solver {
    fetcher: Arc<dyn Fetcher>,
    browser: Arc<dyn BrowserBackend>,
    cache: CookieCache,
    default_proxy: Option<Proxy>,
    /// One lock per cache key: simultaneous requests for one domain cause one solve.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    sessions: Mutex<HashMap<String, Session>>,
    last_user_agent: Mutex<String>,
}

fn random_id() -> String {
    let part = || RandomState::new().build_hasher().finish();
    format!("{:016x}{:016x}", part(), part())
}

/// Maximum time a speculative HTTP fetch may spend before falling through to
/// the browser, so a stalled plain fetch does not consume the whole `maxTimeout`.
const MAX_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

impl Solver {
    pub fn new(
        fetcher: Arc<dyn Fetcher>,
        browser: Arc<dyn BrowserBackend>,
        cache: CookieCache,
        default_proxy: Option<Proxy>,
    ) -> Self {
        Solver {
            fetcher,
            browser,
            cache,
            default_proxy,
            locks: Mutex::default(),
            sessions: Mutex::default(),
            last_user_agent: Mutex::new(DEFAULT_USER_AGENT.to_string()),
        }
    }

    /// User-agent of the last browser solve; the HTTP client's default before one.
    pub fn user_agent(&self) -> String {
        self.last_user_agent.lock().unwrap().clone()
    }

    /// Creates a session. Returns its id, and `false` when it existed already.
    pub fn create_session(&self, id: Option<String>, proxy: Option<Proxy>) -> (String, bool) {
        let id = id.unwrap_or_else(random_id);
        let mut sessions = self.sessions.lock().unwrap();
        let created = !sessions.contains_key(&id);
        if created {
            sessions.insert(id.clone(), Session::new(proxy));
        }
        (id, created)
    }

    pub fn list_sessions(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Returns `false` when the session does not exist.
    pub fn destroy_session(&self, id: &str) -> bool {
        self.sessions.lock().unwrap().remove(id).is_some()
    }

    /// Runs the request through the tiers, inside `req.timeout`.
    ///
    /// `session`: id and optional time-to-live. An unknown session is created;
    /// one older than the time-to-live is replaced by an empty one.
    pub async fn solve(
        &self,
        req: Request,
        session: Option<(&str, Option<Duration>)>,
    ) -> Result<Solved> {
        let timeout = req.timeout;
        // Dropping the inner future on timeout also drops the browser solve,
        // and the backend then closes its tab.
        tokio::time::timeout(timeout, self.run(req, session))
            .await
            .unwrap_or(Err(Error::Timeout))
    }

    async fn run(
        &self,
        mut req: Request,
        session: Option<(&str, Option<Duration>)>,
    ) -> Result<Solved> {
        let host = url::Url::parse(&req.url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .and_then(|u| u.host_str().map(str::to_string))
            .ok_or_else(|| Error::Invalid(format!("invalid url: {}", req.url)))?;

        if let Some((id, ttl)) = session {
            let mut sessions = self.sessions.lock().unwrap();
            let entry = sessions
                .entry(id.to_string())
                .or_insert_with(|| Session::new(None));
            if ttl.is_some_and(|ttl| entry.created.elapsed() >= ttl) {
                *entry = Session::new(entry.proxy.take());
            }
            req.cookies = merge_cookies(&entry.cookies, &req.cookies);
            // FlareSolverr semantics: a session uses the proxy it was created with.
            if entry.proxy.is_some() {
                req.proxy = entry.proxy.clone();
            }
        }
        if req.proxy.is_none() {
            req.proxy = self.default_proxy.clone();
        }
        // Clearance is bound to the exit IP, so the proxy is part of the key.
        let key = format!(
            "{host}|{}",
            req.proxy.as_ref().map(Proxy::key).unwrap_or_default()
        );

        let mut solved = self.tiers(&req, &key).await?;
        solved.page.cookies = merge_cookies(&req.cookies, &solved.page.cookies);
        if solved.tier == Tier::Browser {
            *self.last_user_agent.lock().unwrap() = solved.page.user_agent.clone();
        }
        if let Some((id, _)) = session {
            if let Some(entry) = self.sessions.lock().unwrap().get_mut(id) {
                entry.cookies = merge_cookies(&entry.cookies, &solved.page.cookies);
            }
        }
        Ok(solved)
    }

    async fn fetch_bounded(&self, req: &Request) -> Result<Page> {
        let limit = (req.timeout / 2)
            .min(MAX_FETCH_TIMEOUT)
            .max(Duration::from_millis(50));
        let mut bounded = req.clone();
        bounded.timeout = limit;
        tokio::time::timeout(limit, self.fetcher.fetch(&bounded))
            .await
            .unwrap_or(Err(Error::Timeout))
    }

    async fn tiers(&self, req: &Request, key: &str) -> Result<Solved> {
        let started = now_secs();

        if self.cache.get(key).is_some() {
            if let Some(page) = self.replay(req, key).await {
                return Ok(Solved {
                    page,
                    tier: Tier::Cached,
                });
            }
        } else {
            match self.fetch_bounded(req).await {
                Ok(page) if detect(&page) == Verdict::Clear => {
                    return Ok(Solved {
                        page,
                        tier: Tier::Plain,
                    })
                }
                Ok(page) => tracing::info!(status = page.status, "challenge detected on {key}"),
                // The browser may still get through, so fall through.
                Err(e) => tracing::warn!("plain fetch failed on {key}: {e}"),
            }
        }

        let lock = self
            .locks
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default()
            .clone();
        let _solving = lock.lock().await;
        // Another request may have solved this domain while this one waited.
        if self.cache.get(key).is_some_and(|c| c.stored_at >= started) {
            if let Some(page) = self.replay(req, key).await {
                return Ok(Solved {
                    page,
                    tier: Tier::Cached,
                });
            }
        }

        tracing::info!("solving {key} in the browser");
        let page = self.browser.solve(req).await?;
        match detect(&page) {
            Verdict::Clear => {}
            Verdict::Blocked => return Err(Error::Blocked),
            Verdict::Challenge => {
                return Err(Error::Browser("the challenge was not solved".into()))
            }
        }
        let site_cookies: Vec<Cookie> = page
            .cookies
            .iter()
            .filter(|c| {
                !req.cookies
                    .iter()
                    .any(|rc| rc.name == c.name && rc.value == c.value)
            })
            .cloned()
            .collect();
        if !site_cookies.is_empty() {
            self.cache.put(key, site_cookies, page.user_agent.clone());
        }
        Ok(Solved {
            page,
            tier: Tier::Browser,
        })
    }

    /// Tier a. Sends the cached cookies with the exact user-agent of the
    /// browser that got them. `None`: no usable clearance; a clearance that
    /// met a challenge is invalidated.
    async fn replay(&self, req: &Request, key: &str) -> Option<Page> {
        let clearance = self.cache.get(key)?;
        let mut replay = req.clone();
        replay.cookies = merge_cookies(&clearance.cookies, &req.cookies);
        replay.user_agent = Some(clearance.user_agent.clone());
        match self.fetch_bounded(&replay).await {
            Ok(mut page) if detect(&page) == Verdict::Clear => {
                page.cookies = merge_cookies(&clearance.cookies, &page.cookies);
                Some(page)
            }
            Ok(_) => {
                tracing::info!("cached clearance for {key} was challenged; invalidating it");
                self.cache.invalidate(key);
                None
            }
            Err(e) => {
                tracing::warn!("replay failed on {key}: {e}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const BROWSER_UA: &str =
        "Mozilla/5.0 (X11; Linux x86_64; rv:135.0) Gecko/20100101 Firefox/135.0";

    fn challenge_page() -> Page {
        Page {
            status: 403,
            body: "<title>Just a moment...</title>".into(),
            ..Page::default()
        }
    }

    fn content_page(user_agent: &str) -> Page {
        Page {
            status: 200,
            body: "<title>Site</title>content".into(),
            user_agent: user_agent.into(),
            ..Page::default()
        }
    }

    /// A site that shows a challenge unless the request has the `cf_clearance`
    /// cookie with the wanted value and the browser's user-agent.
    struct ProtectedSite {
        accepted: Mutex<String>,
        requests: Mutex<Vec<Request>>,
    }

    impl ProtectedSite {
        fn new() -> Arc<Self> {
            Arc::new(ProtectedSite {
                accepted: Mutex::new("token-1".into()),
                requests: Mutex::default(),
            })
        }
    }

    #[async_trait]
    impl Fetcher for ProtectedSite {
        async fn fetch(&self, req: &Request) -> Result<Page> {
            self.requests.lock().unwrap().push(req.clone());
            let accepted = self.accepted.lock().unwrap().clone();
            let cleared = req.user_agent.as_deref() == Some(BROWSER_UA)
                && req
                    .cookies
                    .iter()
                    .any(|c| c.name == "cf_clearance" && c.value == accepted);
            Ok(if cleared {
                content_page(BROWSER_UA)
            } else {
                challenge_page()
            })
        }
    }

    /// A site without any challenge.
    struct OpenSite;

    #[async_trait]
    impl Fetcher for OpenSite {
        async fn fetch(&self, _req: &Request) -> Result<Page> {
            Ok(content_page(DEFAULT_USER_AGENT))
        }
    }

    /// Browser that solves the challenge of `site` after `delay`.
    struct MockBrowser {
        site: Option<Arc<ProtectedSite>>,
        delay: Duration,
        solves: AtomicUsize,
        requests: Mutex<Vec<Request>>,
    }

    impl MockBrowser {
        fn new(site: Option<Arc<ProtectedSite>>, delay: Duration) -> Arc<Self> {
            Arc::new(MockBrowser {
                site,
                delay,
                solves: AtomicUsize::new(0),
                requests: Mutex::default(),
            })
        }

        fn solves(&self) -> usize {
            self.solves.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl BrowserBackend for MockBrowser {
        async fn solve(&self, req: &Request) -> Result<Page> {
            self.solves.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(req.clone());
            tokio::time::sleep(self.delay).await;
            let token = match &self.site {
                Some(site) => site.accepted.lock().unwrap().clone(),
                None => "unused".into(),
            };
            let mut page = content_page(BROWSER_UA);
            page.cookies = vec![Cookie::new("cf_clearance", token)];
            Ok(page)
        }

        async fn shutdown(&self) {}
    }

    fn solver(fetcher: Arc<dyn Fetcher>, browser: Arc<MockBrowser>) -> Solver {
        let cache = CookieCache::new(Duration::from_secs(3600), None);
        Solver::new(fetcher, browser, cache, None)
    }

    fn get(url: &str) -> Request {
        Request::get(url)
    }

    #[tokio::test]
    async fn plain_fetch_without_challenge_skips_the_browser() {
        let browser = MockBrowser::new(None, Duration::ZERO);
        let solver = solver(Arc::new(OpenSite), browser.clone());
        let solved = solver.solve(get("http://open.test/"), None).await.unwrap();
        assert_eq!(solved.tier, Tier::Plain);
        assert_eq!(browser.solves(), 0);
    }

    #[tokio::test]
    async fn challenge_falls_through_to_browser_then_cache_replays() {
        let site = ProtectedSite::new();
        let browser = MockBrowser::new(Some(site.clone()), Duration::ZERO);
        let solver = solver(site.clone(), browser.clone());

        let first = solver.solve(get("http://cf.test/a"), None).await.unwrap();
        assert_eq!(first.tier, Tier::Browser);
        assert_eq!(first.page.user_agent, BROWSER_UA);
        assert_eq!(first.page.cookies[0].name, "cf_clearance");
        assert_eq!(solver.user_agent(), BROWSER_UA);

        let second = solver.solve(get("http://cf.test/b"), None).await.unwrap();
        assert_eq!(second.tier, Tier::Cached);
        assert_eq!(browser.solves(), 1, "the second request needs no browser");
        assert_eq!(second.page.user_agent, BROWSER_UA);
        assert_eq!(
            second.page.cookies[0].value, "token-1",
            "the client gets the cached clearance cookies"
        );

        let requests = site.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "plain fetch, then cached replay");
        assert_eq!(requests[0].user_agent, None);
        assert_eq!(
            requests[1].user_agent.as_deref(),
            Some(BROWSER_UA),
            "the replay sends the exact user-agent of the browser"
        );
    }

    #[tokio::test]
    async fn challenged_replay_invalidates_and_solves_again() {
        let site = ProtectedSite::new();
        let browser = MockBrowser::new(Some(site.clone()), Duration::ZERO);
        let solver = solver(site.clone(), browser.clone());
        solver.solve(get("http://cf.test/"), None).await.unwrap();

        // The site stops accepting the cached clearance.
        *site.accepted.lock().unwrap() = "token-2".into();
        let again = solver.solve(get("http://cf.test/"), None).await.unwrap();
        assert_eq!(again.tier, Tier::Browser);
        assert_eq!(browser.solves(), 2);

        let cached = solver.solve(get("http://cf.test/"), None).await.unwrap();
        assert_eq!(cached.tier, Tier::Cached);
        assert_eq!(cached.page.cookies[0].value, "token-2");
        assert_eq!(browser.solves(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn simultaneous_requests_for_one_domain_solve_once() {
        let site = ProtectedSite::new();
        let browser = MockBrowser::new(Some(site.clone()), Duration::from_secs(5));
        let solver = Arc::new(solver(site.clone(), browser.clone()));

        let tasks: Vec<_> = (0..10)
            .map(|i| {
                let solver = solver.clone();
                tokio::spawn(async move {
                    solver
                        .solve(get(&format!("http://cf.test/{i}")), None)
                        .await
                })
            })
            .collect();
        let mut tiers = Vec::new();
        for task in tasks {
            tiers.push(task.await.unwrap().unwrap().tier);
        }
        assert_eq!(browser.solves(), 1);
        assert_eq!(tiers.iter().filter(|t| **t == Tier::Browser).count(), 1);
        assert_eq!(tiers.iter().filter(|t| **t == Tier::Cached).count(), 9);
    }

    #[tokio::test(start_paused = true)]
    async fn different_domains_solve_separately() {
        let site = ProtectedSite::new();
        let browser = MockBrowser::new(Some(site.clone()), Duration::from_secs(1));
        let solver = solver(site.clone(), browser.clone());
        solver.solve(get("http://one.test/"), None).await.unwrap();
        solver.solve(get("http://two.test/"), None).await.unwrap();
        assert_eq!(browser.solves(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_is_reported() {
        let site = ProtectedSite::new();
        let browser = MockBrowser::new(Some(site.clone()), Duration::from_secs(120));
        let solver = solver(site, browser);
        let mut req = get("http://cf.test/");
        req.timeout = Duration::from_secs(10);
        assert_eq!(solver.solve(req, None).await.unwrap_err(), Error::Timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_plain_fetch_falls_through_to_browser_within_max_timeout() {
        struct HangingFetcher;
        #[async_trait]
        impl Fetcher for HangingFetcher {
            async fn fetch(&self, _req: &Request) -> Result<Page> {
                std::future::pending().await
            }
        }
        let browser = MockBrowser::new(None, Duration::from_secs(2));
        let solver = solver(Arc::new(HangingFetcher), browser.clone());
        let mut req = get("http://cf.test/");
        req.timeout = Duration::from_secs(10);
        let solved = solver.solve(req, None).await.unwrap();
        assert_eq!(solved.tier, Tier::Browser);
        assert_eq!(browser.solves(), 1);
    }

    #[tokio::test]
    async fn browser_and_replay_use_the_same_proxy() {
        let site = ProtectedSite::new();
        let browser = MockBrowser::new(Some(site.clone()), Duration::ZERO);
        let default_proxy = Proxy {
            url: "http://default:1".into(),
            username: None,
            password: None,
        };
        let solver = Solver::new(
            site.clone(),
            browser.clone(),
            CookieCache::new(Duration::from_secs(3600), None),
            Some(default_proxy.clone()),
        );
        let custom = Proxy {
            url: "socks5://custom:2".into(),
            username: None,
            password: None,
        };

        let mut req = get("http://cf.test/");
        req.proxy = Some(custom.clone());
        solver.solve(req.clone(), None).await.unwrap();
        assert_eq!(solver.solve(req, None).await.unwrap().tier, Tier::Cached);
        assert_eq!(
            browser.requests.lock().unwrap()[0].proxy,
            Some(custom.clone())
        );
        assert!(site
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.proxy == Some(custom.clone())));

        // Another exit IP: the clearance of the custom proxy must not be used.
        let other = solver.solve(get("http://cf.test/"), None).await.unwrap();
        assert_eq!(other.tier, Tier::Browser);
        assert_eq!(
            browser.requests.lock().unwrap()[1].proxy,
            Some(default_proxy)
        );
    }

    #[tokio::test]
    async fn unsolved_challenge_is_an_error() {
        struct StuckBrowser;
        #[async_trait]
        impl BrowserBackend for StuckBrowser {
            async fn solve(&self, _req: &Request) -> Result<Page> {
                Ok(challenge_page())
            }
            async fn shutdown(&self) {}
        }
        let solver = Solver::new(
            ProtectedSite::new(),
            Arc::new(StuckBrowser),
            CookieCache::new(Duration::from_secs(3600), None),
            None,
        );
        let err = solver
            .solve(get("http://cf.test/"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Browser(_)));
    }

    #[tokio::test]
    async fn sessions_keep_cookies_between_requests() {
        struct SetsCookie(Mutex<Vec<Request>>);
        #[async_trait]
        impl Fetcher for SetsCookie {
            async fn fetch(&self, req: &Request) -> Result<Page> {
                self.0.lock().unwrap().push(req.clone());
                let mut page = content_page(DEFAULT_USER_AGENT);
                page.cookies = vec![Cookie::new("sid", "1")];
                Ok(page)
            }
        }
        let site = Arc::new(SetsCookie(Mutex::default()));
        let solver = solver(site.clone(), MockBrowser::new(None, Duration::ZERO));

        let (id, created) = solver.create_session(Some("s1".into()), None);
        assert_eq!((id.as_str(), created), ("s1", true));
        assert!(!solver.create_session(Some("s1".into()), None).1);
        assert_eq!(solver.create_session(None, None).0.len(), 32);
        assert_eq!(solver.list_sessions().len(), 2);

        let session = Some(("s1", None));
        solver.solve(get("http://x.test/"), session).await.unwrap();
        solver.solve(get("http://x.test/"), session).await.unwrap();
        solver.solve(get("http://x.test/"), None).await.unwrap();
        let requests = site.0.lock().unwrap();
        assert!(requests[0].cookies.is_empty());
        assert_eq!(requests[1].cookies, vec![Cookie::new("sid", "1")]);
        assert!(requests[2].cookies.is_empty(), "no session, no cookies");

        assert!(solver.destroy_session("s1"));
        assert!(!solver.destroy_session("s1"));
    }

    #[tokio::test]
    async fn session_cookies_do_not_leak_into_shared_cache() {
        struct ContextBrowser(Arc<ProtectedSite>);
        #[async_trait]
        impl BrowserBackend for ContextBrowser {
            async fn solve(&self, req: &Request) -> Result<Page> {
                let token = self.0.accepted.lock().unwrap().clone();
                let mut page = content_page(BROWSER_UA);
                page.cookies = merge_cookies(&req.cookies, &[Cookie::new("cf_clearance", token)]);
                Ok(page)
            }
            async fn shutdown(&self) {}
        }
        let site = ProtectedSite::new();
        let solver = Solver::new(
            site.clone(),
            Arc::new(ContextBrowser(site.clone())),
            CookieCache::new(Duration::from_secs(3600), None),
            None,
        );

        let mut req1 = get("http://cf.test/");
        req1.cookies = vec![Cookie::new("auth", "alice")];
        let first = solver.solve(req1, Some(("s1", None))).await.unwrap();
        assert_eq!(first.tier, Tier::Browser);
        assert!(first.page.cookies.iter().any(|c| c.name == "auth"));

        let second = solver.solve(get("http://cf.test/"), None).await.unwrap();
        assert_eq!(second.tier, Tier::Cached);
        assert!(!second.page.cookies.iter().any(|c| c.name == "auth"));
        assert!(second.page.cookies.iter().any(|c| c.name == "cf_clearance"));
    }

    #[test]
    fn invalid_url_is_rejected() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let solver = solver(Arc::new(OpenSite), MockBrowser::new(None, Duration::ZERO));
        for bad in ["not a url", "file://localhost/etc/passwd", "ftp://x.test/"] {
            let err = rt.block_on(solver.solve(get(bad), None)).unwrap_err();
            assert!(matches!(err, Error::Invalid(_)), "{bad}: {err:?}");
        }
    }
}
