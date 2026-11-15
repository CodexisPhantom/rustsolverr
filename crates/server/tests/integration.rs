//! Runs the real API and the real HTTP client against a local mock site that
//! simulates a JavaScript challenge.
//!
//! The site serves a "Just a moment..." page whose script sets the
//! `cf_clearance` cookie from `navigator.userAgent` and reloads. Like
//! Cloudflare, it accepts the cookie only together with the user-agent that
//! earned it.
//!
//! `challenge_is_solved_once_then_replayed` uses a simulated browser.
//! `camoufox_solves_the_challenge` uses the real Camoufox; it is ignored
//! unless Python with the `camoufox` package is installed:
//! `cargo test -p rustsolverr -- --ignored` (set `PYTHON` if needed).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use rustsolverr_browser::{BrowserBackend, CamoufoxBackend, Managed};
use rustsolverr_cache::CookieCache;
use rustsolverr_common::{Cookie, Page, Request, Result};
use rustsolverr_fetch::{Fetcher, WreqFetcher};
use rustsolverr_solver::Solver;
use serde_json::{json, Value};

const CHALLENGE_HTML: &str = r#"<!DOCTYPE html>
<html><head><title>Just a moment...</title></head>
<body><div id="cf-challenge-running">Checking your browser</div>
<script>
setTimeout(function () {
  document.cookie = "cf_clearance=ok-" + navigator.userAgent.replace(/[^a-zA-Z0-9]/g, "") + "; path=/";
  location.reload();
}, 1500);
</script></body></html>"#;

/// The cookie value the challenge script computes for a user-agent.
fn clearance_for(user_agent: &str) -> String {
    let alnum: String = user_agent
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    format!("ok-{alnum}")
}

#[derive(Default)]
struct SiteStats {
    challenges: AtomicUsize,
    contents: AtomicUsize,
}

fn cleared(headers: &HeaderMap) -> bool {
    let text = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    };
    let wanted = format!("cf_clearance={}", clearance_for(text(header::USER_AGENT)));
    text(header::COOKIE).split("; ").any(|c| c == wanted)
}

async fn protected(
    State(stats): State<Arc<SiteStats>>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, Html<String>) {
    if cleared(&headers) {
        stats.contents.fetch_add(1, Ordering::SeqCst);
        let page = format!(
            "<html><head><title>Site</title></head><body>secret content {body}</body></html>"
        );
        (StatusCode::OK, Html(page))
    } else {
        stats.challenges.fetch_add(1, Ordering::SeqCst);
        (StatusCode::SERVICE_UNAVAILABLE, Html(CHALLENGE_HTML.into()))
    }
}

async fn open() -> Html<&'static str> {
    Html("<html><head><title>Open</title></head><body>open content</body></html>")
}

/// Serves `app` on a free local port; returns the port.
async fn serve(app: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

async fn start_site() -> (u16, Arc<SiteStats>) {
    let stats = Arc::new(SiteStats::default());
    let app = Router::new()
        .route("/", get(protected).post(protected))
        .route("/open", get(open))
        .with_state(stats.clone());
    (serve(app).await, stats)
}

async fn start_api(browser: Arc<dyn BrowserBackend>) -> u16 {
    let solver = Solver::new(
        Arc::new(WreqFetcher::new().unwrap()),
        browser,
        CookieCache::new(Duration::from_secs(600), None),
        None,
    );
    serve(rustsolverr_api::router(Arc::new(solver))).await
}

/// Posts a command to the API the way Prowlarr does; returns status and body.
async fn v1(api: u16, body: Value) -> (u16, Value) {
    let mut req = Request::get(format!("http://127.0.0.1:{api}/v1"));
    req.method = rustsolverr_common::Method::Post;
    req.post_data = Some(body.to_string());
    let page = WreqFetcher::new().unwrap().fetch(&req).await.unwrap();
    (page.status, serde_json::from_str(&page.body).unwrap())
}

const FAKE_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:135.0) Gecko/20100101 Firefox/135.0";

/// Does what a browser does on the mock site: loads the page, "runs" the
/// challenge script, and loads the page again with the cookie.
struct FakeBrowser {
    fetcher: WreqFetcher,
    solves: AtomicUsize,
}

#[async_trait]
impl BrowserBackend for FakeBrowser {
    async fn solve(&self, req: &Request) -> Result<Page> {
        self.solves.fetch_add(1, Ordering::SeqCst);
        let mut req = req.clone();
        req.user_agent = Some(FAKE_UA.into());
        let first = self.fetcher.fetch(&req).await?;
        assert!(
            first.body.contains("document.cookie"),
            "expected the challenge"
        );
        let cookie = Cookie {
            domain: "127.0.0.1".into(),
            ..Cookie::new("cf_clearance", clearance_for(FAKE_UA))
        };
        req.cookies.push(cookie.clone());
        let mut page = self.fetcher.fetch(&req).await?;
        page.cookies = vec![cookie];
        Ok(page)
    }

    async fn shutdown(&self) {}
}

#[tokio::test]
async fn challenge_is_solved_once_then_replayed() {
    let (site, stats) = start_site().await;
    let browser = Arc::new(FakeBrowser {
        fetcher: WreqFetcher::new().unwrap(),
        solves: AtomicUsize::new(0),
    });
    let api = start_api(browser.clone()).await;
    let url = format!("http://127.0.0.1:{site}/");

    // No cache: the plain fetch meets the challenge, the browser solves it.
    let (status, body) = v1(
        api,
        json!({"cmd": "request.get", "url": url, "maxTimeout": 30000}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["message"], "Challenge solved!");
    let solution = &body["solution"];
    assert_eq!(solution["status"], 200);
    assert!(solution["response"]
        .as_str()
        .unwrap()
        .contains("secret content"));
    assert_eq!(solution["userAgent"], FAKE_UA);
    assert_eq!(solution["cookies"][0]["name"], "cf_clearance");
    assert_eq!(solution["cookies"][0]["value"], clearance_for(FAKE_UA));
    assert_eq!(browser.solves.load(Ordering::SeqCst), 1);

    // Cached clearance: replayed by the HTTP client with the browser's exact
    // user-agent. The site would show the challenge for any other one.
    let challenges = stats.challenges.load(Ordering::SeqCst);
    let (status, body) = v1(api, json!({"cmd": "request.get", "url": url})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["message"], "Challenge not detected!");
    assert!(body["solution"]["response"]
        .as_str()
        .unwrap()
        .contains("secret content"));
    assert_eq!(body["solution"]["userAgent"], FAKE_UA);
    assert_eq!(
        body["solution"]["cookies"][0]["value"],
        clearance_for(FAKE_UA)
    );
    assert_eq!(browser.solves.load(Ordering::SeqCst), 1, "no second solve");
    assert_eq!(
        stats.challenges.load(Ordering::SeqCst),
        challenges,
        "no new challenge"
    );

    // A POST through the cached clearance reaches the site with its body.
    let (status, body) = v1(
        api,
        json!({"cmd": "request.post", "url": url, "postData": "a=b&c=d"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body["solution"]["response"]
        .as_str()
        .unwrap()
        .contains("secret content a=b&c=d"));
    assert_eq!(browser.solves.load(Ordering::SeqCst), 1);

    // Another host (cache key) without a challenge: plain fetch only.
    let (status, body) = v1(
        api,
        json!({"cmd": "request.get", "url": format!("http://localhost:{site}/open")}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["message"], "Challenge not detected!");
    assert!(body["solution"]["response"]
        .as_str()
        .unwrap()
        .contains("open content"));
    assert_eq!(browser.solves.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[ignore = "needs Python with the camoufox package and a fetched browser"]
async fn camoufox_solves_the_challenge() {
    let (site, stats) = start_site().await;
    let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".into());
    let headless = std::env::var("HEADLESS").unwrap_or_else(|_| "true".into());
    let browser = Managed::new(Arc::new(CamoufoxBackend::new(python, headless)), 1);
    let api = start_api(browser.clone()).await;
    let url = format!("http://127.0.0.1:{site}/");

    let (status, body) = v1(
        api,
        json!({"cmd": "request.get", "url": url, "maxTimeout": 120000}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["message"], "Challenge solved!");
    let solution = &body["solution"];
    let user_agent = solution["userAgent"].as_str().unwrap().to_string();
    assert!(user_agent.contains("Firefox/"), "{user_agent}");
    assert!(solution["response"]
        .as_str()
        .unwrap()
        .contains("secret content"));
    let cookies = solution["cookies"].as_array().unwrap();
    let clearance = cookies
        .iter()
        .find(|c| c["name"] == "cf_clearance")
        .unwrap();
    assert_eq!(clearance["value"], clearance_for(&user_agent));

    // The replay must pass with the cookies and user-agent of that browser.
    let challenges = stats.challenges.load(Ordering::SeqCst);
    let (status, body) = v1(api, json!({"cmd": "request.get", "url": url})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["message"], "Challenge not detected!");
    assert_eq!(body["solution"]["userAgent"], user_agent.as_str());
    assert_eq!(stats.challenges.load(Ordering::SeqCst), challenges);

    // A solve that cannot finish in time gives the FlareSolverr timeout error.
    let (status, body) = v1(
        api,
        json!({"cmd": "request.get", "url": format!("http://localhost:{site}/"), "maxTimeout": 1000}),
    )
    .await;
    assert_eq!(status, 500, "{body}");
    assert_eq!(
        body["message"],
        "Error: Error solving the challenge. Timeout after 1.0 seconds."
    );

    browser.shutdown().await;
}
