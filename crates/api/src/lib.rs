//! FlareSolverr v1 compatible HTTP API.
//!
//! Messages and status codes follow FlareSolverr's `flaresolverr_service.py`:
//! every failure of `POST /v1` is HTTP 500 with `status: "error"` and a
//! message that starts with `Error: `.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::routing::{get, post};
use axum::{Json, Router};
use rustsolverr_common::{Error, Method, Request};
use rustsolverr_solver::{Solver, Tier};
use serde_json::{json, Value};

pub mod types;
use types::{IndexResponse, Solution, V1Request, V1Response};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_MAX_TIMEOUT_MS: f64 = 60000.0;

pub fn router(solver: Arc<Solver>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/v1", post(v1))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(solver)
}

async fn index(State(solver): State<Arc<Solver>>) -> Json<IndexResponse> {
    Json(IndexResponse {
        msg: "FlareSolverr is ready!",
        version: VERSION,
        user_agent: solver.user_agent(),
    })
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

/// Same body shape as FlareSolverr's error handler.
fn http_error(status: StatusCode, uri: &Uri) -> (StatusCode, Json<Value>) {
    let reason = status.canonical_reason().unwrap_or("Error");
    let body = json!({
        "error": format!("{reason}: '{}'", uri.path()),
        "status_code": status.as_u16(),
    });
    (status, Json(body))
}

async fn not_found(uri: Uri) -> (StatusCode, Json<Value>) {
    http_error(StatusCode::NOT_FOUND, &uri)
}

async fn method_not_allowed(uri: Uri) -> (StatusCode, Json<Value>) {
    http_error(StatusCode::METHOD_NOT_ALLOWED, &uri)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The body is read as raw bytes: FlareSolverr does not demand a
/// `Content-Type: application/json` header.
async fn v1(State(solver): State<Arc<Solver>>, body: Bytes) -> (StatusCode, Json<V1Response>) {
    let start = now_ms();
    let result = match serde_json::from_slice::<V1Request>(&body) {
        Ok(req) => handle(&solver, req).await,
        Err(e) => Err(format!("Invalid JSON body: {e}")),
    };
    let (code, mut res) = match result {
        Ok(res) => (StatusCode::OK, res),
        Err(message) => {
            tracing::error!("{message}");
            let res = V1Response {
                status: "error".into(),
                message: format!("Error: {message}"),
                ..V1Response::default()
            };
            (StatusCode::INTERNAL_SERVER_ERROR, res)
        }
    };
    res.start_timestamp = start;
    res.end_timestamp = now_ms();
    res.version = VERSION.into();
    (code, Json(res))
}

fn ok(message: &str) -> V1Response {
    V1Response {
        status: "ok".into(),
        message: message.into(),
        ..V1Response::default()
    }
}

async fn handle(solver: &Solver, req: V1Request) -> Result<V1Response, String> {
    let cmd = req
        .cmd
        .clone()
        .ok_or("Request parameter 'cmd' is mandatory.")?;
    match cmd.as_str() {
        "sessions.create" => {
            let proxy = req.proxy.and_then(|p| p.into_proxy());
            let (id, created) = solver.create_session(req.session, proxy);
            let mut res = ok(if created {
                "Session created successfully."
            } else {
                "Session already exists."
            });
            res.session = Some(id);
            Ok(res)
        }
        "sessions.list" => {
            let mut res = ok("");
            res.sessions = Some(solver.list_sessions());
            Ok(res)
        }
        "sessions.destroy" => {
            if !req.session.is_some_and(|id| solver.destroy_session(&id)) {
                return Err("The session doesn't exist.".into());
            }
            Ok(ok("The session has been removed."))
        }
        "request.get" => {
            if req.post_data.is_some() {
                return Err("Cannot use 'postBody' when sending a GET request.".into());
            }
            request(solver, req, Method::Get, "request.get").await
        }
        "request.post" => {
            if req.post_data.is_none() {
                return Err(
                    "Request parameter 'postData' is mandatory in 'request.post' command.".into(),
                );
            }
            request(solver, req, Method::Post, "request.post").await
        }
        other => Err(format!("Request parameter 'cmd' = '{other}' is invalid.")),
    }
}

async fn request(
    solver: &Solver,
    req: V1Request,
    method: Method,
    cmd: &str,
) -> Result<V1Response, String> {
    let url = req
        .url
        .ok_or_else(|| format!("Request parameter 'url' is mandatory in '{cmd}' command."))?;
    let timeout_ms = req
        .max_timeout
        .filter(|ms| *ms >= 1.0)
        .unwrap_or(DEFAULT_MAX_TIMEOUT_MS);
    let seconds = |v: f64| Duration::try_from_secs_f64(v).unwrap_or(Duration::ZERO);
    let page_request = Request {
        url,
        method,
        post_data: req.post_data,
        cookies: req.cookies.unwrap_or_default(),
        user_agent: None,
        proxy: req.proxy.and_then(|p| p.into_proxy()),
        timeout: seconds(timeout_ms / 1000.0),
        wait: seconds(req.wait_in_seconds.unwrap_or(0.0)),
    };
    let session_ttl = req
        .session_ttl_minutes
        .filter(|minutes| *minutes > 0.0)
        .map(|minutes| seconds(minutes * 60.0));
    let session = req.session.as_deref().map(|id| (id, session_ttl));

    let solved = solver.solve(page_request, session).await.map_err(|e| {
        let detail = match e {
            // `{:?}` prints 60.0 like Python does.
            Error::Timeout => format!("Timeout after {:?} seconds.", timeout_ms / 1000.0),
            Error::Blocked => "Cloudflare has blocked this request. Probably your IP is \
                                   banned for this site, check in your web browser."
                .into(),
            other => other.to_string().replace('\n', "\\n"),
        };
        format!("Error solving the challenge. {detail}")
    })?;

    let mut res = ok(match solved.tier {
        Tier::Browser => "Challenge solved!",
        Tier::Cached | Tier::Plain => "Challenge not detected!",
    });
    let page = solved.page;
    let only_cookies = req.return_only_cookies.unwrap_or(false);
    res.solution = Some(Solution {
        url: page.url,
        status: page.status,
        headers: (!only_cookies).then_some(page.headers),
        response: (!only_cookies).then_some(page.body),
        cookies: page.cookies.into_iter().map(Into::into).collect(),
        user_agent: page.user_agent,
    });
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use rustsolverr_browser::BrowserBackend;
    use rustsolverr_cache::CookieCache;
    use rustsolverr_common::{Cookie, Page};
    use rustsolverr_fetch::{Fetcher, DEFAULT_USER_AGENT};
    use tower::ServiceExt;

    /// `/challenge` shows a challenge; every other path is a normal page.
    struct Site;

    #[async_trait]
    impl Fetcher for Site {
        async fn fetch(&self, req: &Request) -> rustsolverr_common::Result<Page> {
            let challenge = req.url.contains("/challenge");
            Ok(Page {
                url: req.url.clone(),
                status: if challenge { 403 } else { 200 },
                headers: [("content-type".to_string(), "text/html".to_string())].into(),
                body: if challenge {
                    "<title>Just a moment...</title>".into()
                } else {
                    format!(
                        "<title>ok</title>{}",
                        req.post_data.as_deref().unwrap_or("")
                    )
                },
                cookies: vec![Cookie::new("sid", "1")],
                user_agent: DEFAULT_USER_AGENT.into(),
            })
        }
    }

    /// A browser that never finishes.
    struct SlowBrowser;

    #[async_trait]
    impl BrowserBackend for SlowBrowser {
        async fn solve(&self, _req: &Request) -> rustsolverr_common::Result<Page> {
            std::future::pending().await
        }
        async fn shutdown(&self) {}
    }

    fn app() -> Router {
        let cache = CookieCache::new(Duration::from_secs(60), None);
        router(Arc::new(Solver::new(
            Arc::new(Site),
            Arc::new(SlowBrowser),
            cache,
            None,
        )))
    }

    async fn call(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let request = HttpRequest::builder()
            .method(method)
            .uri(path)
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn v1(app: &Router, body: Value) -> (StatusCode, Value) {
        call(app, "POST", "/v1", body).await
    }

    #[tokio::test]
    async fn index_and_health() {
        let app = app();
        let (status, body) = call(&app, "GET", "/", json!(null)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["msg"], "FlareSolverr is ready!");
        assert_eq!(body["version"], VERSION);
        assert_eq!(body["userAgent"], DEFAULT_USER_AGENT);

        let (status, body) = call(&app, "GET", "/health", json!(null)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"status": "ok"}));
    }

    #[tokio::test]
    async fn unknown_route_and_method() {
        let app = app();
        let (status, body) = call(&app, "GET", "/nope", json!(null)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["status_code"], 404);
        let (status, body) = call(&app, "GET", "/v1", json!(null)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(body["status_code"], 405);
    }

    /// The call Prowlarr's "Test" button makes: a `request.get` with every
    /// property present. Prowlarr wants HTTP 200 and a body that parses.
    #[tokio::test]
    async fn prowlarr_test_call() {
        let (status, body) = v1(
            &app(),
            json!({
                "cmd": "request.get",
                "url": "https://prowlarr.servarr.com/v1/ping",
                "cookies": null,
                "proxy": {"url": null, "username": null, "password": null},
                "headers": null,
                "maxTimeout": 60000
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["message"], "Challenge not detected!");
        assert!(body["startTimestamp"].is_u64() && body["endTimestamp"].is_u64());
        assert_eq!(body["version"], VERSION);
        let solution = &body["solution"];
        assert_eq!(solution["url"], "https://prowlarr.servarr.com/v1/ping");
        assert_eq!(solution["status"], 200);
        assert_eq!(solution["headers"]["content-type"], "text/html");
        assert_eq!(solution["response"], "<title>ok</title>");
        assert_eq!(solution["userAgent"], DEFAULT_USER_AGENT);
        let cookie = &solution["cookies"][0];
        assert_eq!(cookie["name"], "sid");
        assert!(cookie["expires"].is_number() && cookie["size"].is_u64());
        assert!(cookie["httpOnly"].is_boolean() && cookie["session"].is_boolean());
        assert!(cookie["sameSite"].is_string());
    }

    #[tokio::test]
    async fn post_sends_post_data_and_return_only_cookies_drops_the_page() {
        let app = app();
        let (status, body) = v1(
            &app,
            json!({"cmd": "request.post", "url": "http://x.test/", "postData": "a=b"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["solution"]["response"], "<title>ok</title>a=b");

        let (_, body) = v1(
            &app,
            json!({"cmd": "request.get", "url": "http://x.test/", "returnOnlyCookies": true}),
        )
        .await;
        let solution = body["solution"].as_object().unwrap();
        assert!(!solution.contains_key("response") && !solution.contains_key("headers"));
        assert_eq!(solution["cookies"][0]["name"], "sid");
    }

    #[tokio::test]
    async fn validation_errors_are_http_500_with_flaresolverr_messages() {
        let app = app();
        let cases = [
            (json!({}), "Error: Request parameter 'cmd' is mandatory."),
            (
                json!({"cmd": "request.put"}),
                "Error: Request parameter 'cmd' = 'request.put' is invalid.",
            ),
            (
                json!({"cmd": "request.get"}),
                "Error: Request parameter 'url' is mandatory in 'request.get' command.",
            ),
            (
                json!({"cmd": "request.get", "url": "http://x.test/", "postData": "a=b"}),
                "Error: Cannot use 'postBody' when sending a GET request.",
            ),
            (
                json!({"cmd": "request.post", "url": "http://x.test/"}),
                "Error: Request parameter 'postData' is mandatory in 'request.post' command.",
            ),
            (
                json!({"cmd": "sessions.destroy", "session": "missing"}),
                "Error: The session doesn't exist.",
            ),
        ];
        for (request, message) in cases {
            let (status, body) = v1(&app, request).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(body["status"], "error");
            assert_eq!(body["message"], message);
            assert!(body.get("solution").is_none());
            assert!(body["startTimestamp"].is_u64());
        }
    }

    #[tokio::test]
    async fn invalid_json_is_an_error_response() {
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/v1")
            .body(Body::from("{not json"))
            .unwrap();
        let response = app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn timeout_gives_the_flaresolverr_timeout_error() {
        let (status, body) = v1(
            &app(),
            json!({"cmd": "request.get", "url": "http://x.test/challenge", "maxTimeout": 50}),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body["message"],
            "Error: Error solving the challenge. Timeout after 0.05 seconds."
        );
    }

    #[tokio::test]
    async fn session_commands() {
        let app = app();
        let (status, body) = v1(&app, json!({"cmd": "sessions.create", "session": "s1"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["message"], "Session created successfully.");
        assert_eq!(body["session"], "s1");

        let (_, body) = v1(&app, json!({"cmd": "sessions.create", "session": "s1"})).await;
        assert_eq!(body["message"], "Session already exists.");

        let (_, body) = v1(&app, json!({"cmd": "sessions.create"})).await;
        let generated = body["session"].as_str().unwrap().to_string();
        assert!(!generated.is_empty());

        let (_, body) = v1(&app, json!({"cmd": "sessions.list"})).await;
        assert_eq!(body["sessions"].as_array().unwrap().len(), 2);

        let (status, body) = v1(&app, json!({"cmd": "sessions.destroy", "session": "s1"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["message"], "The session has been removed.");
        let (_, body) = v1(&app, json!({"cmd": "sessions.list"})).await;
        assert_eq!(body["sessions"], json!([generated]));
    }
}
