//! FlareSolverr v1 wire types.
//!
//! Field names and types follow FlareSolverr's `dtos.py` and the classes
//! Prowlarr and FlareSolverrSharp (Jackett) deserialize into.

use std::collections::BTreeMap;

use rustsolverr_common::{Cookie, Proxy};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// A number, or a string that holds a number (FlareSolverr applies `int()`).
/// Anything else counts as absent.
fn lenient_number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

/// Body of `POST /v1`. Unknown fields are ignored: clients still send
/// parameters that FlareSolverr v2 removed (`headers`, `userAgent`, `download`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct V1Request {
    pub cmd: Option<String>,
    pub url: Option<String>,
    #[serde(rename = "maxTimeout", deserialize_with = "lenient_number")]
    pub max_timeout: Option<f64>,
    pub session: Option<String>,
    #[serde(deserialize_with = "lenient_number")]
    pub session_ttl_minutes: Option<f64>,
    pub cookies: Option<Vec<Cookie>>,
    #[serde(rename = "returnOnlyCookies")]
    pub return_only_cookies: Option<bool>,
    pub proxy: Option<ProxyParam>,
    #[serde(rename = "postData")]
    pub post_data: Option<String>,
    #[serde(rename = "waitInSeconds", deserialize_with = "lenient_number")]
    pub wait_in_seconds: Option<f64>,
}

/// Prowlarr always sends the object, with `url: null` when it has no proxy.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ProxyParam {
    pub url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ProxyParam {
    pub fn into_proxy(self) -> Option<Proxy> {
        let not_empty = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        Some(
            Proxy {
                url: not_empty(self.url)?,
                username: not_empty(self.username),
                password: not_empty(self.password),
            }
            .normalized(),
        )
    }
}

/// Reply to `POST /v1`. FlareSolverr omits the fields a command does not set.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct V1Response {
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solution: Option<Solution>,
    /// Milliseconds since the Unix epoch.
    pub start_timestamp: u64,
    pub end_timestamp: u64,
    pub version: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Solution {
    pub url: String,
    pub status: u16,
    /// Omitted, like `response`, with `returnOnlyCookies`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    pub cookies: Vec<CookieOut>,
    pub user_agent: String,
}

/// Cookie as the clients read it: `expires` a number, `size` an integer,
/// three booleans, `sameSite` a string.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CookieOut {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires: f64,
    pub size: usize,
    pub http_only: bool,
    pub secure: bool,
    pub session: bool,
    pub same_site: String,
}

impl From<Cookie> for CookieOut {
    fn from(c: Cookie) -> Self {
        CookieOut {
            size: c.name.len() + c.value.len(),
            session: c.is_session(),
            name: c.name,
            value: c.value,
            domain: c.domain,
            path: c.path,
            expires: c.expires,
            http_only: c.http_only,
            secure: c.secure,
            same_site: c.same_site,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexResponse {
    pub msg: &'static str,
    pub version: &'static str,
    pub user_agent: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_prowlarr_get() {
        // Prowlarr sends every property, null included, and always a proxy object.
        let req: V1Request = serde_json::from_value(json!({
            "cmd": "request.get",
            "url": "https://example.com/",
            "cookies": null,
            "proxy": {"url": null, "username": null, "password": null},
            "headers": null,
            "maxTimeout": 60000
        }))
        .unwrap();
        assert_eq!(req.cmd.as_deref(), Some("request.get"));
        assert_eq!(req.max_timeout, Some(60000.0));
        assert!(req.cookies.is_none());
        assert!(req.proxy.unwrap().into_proxy().is_none());
    }

    #[test]
    fn parses_prowlarr_post_with_removed_headers_param() {
        let req: V1Request = serde_json::from_value(json!({
            "cmd": "request.post",
            "url": "https://example.com/login",
            "postData": "a=b&c=d",
            "headers": {"contentType": "application/x-www-form-urlencoded", "contentLength": null},
            "maxTimeout": 30000,
            "proxy": {"url": "socks5://10.0.0.1:1080", "username": "u", "password": "p"}
        }))
        .unwrap();
        assert_eq!(req.post_data.as_deref(), Some("a=b&c=d"));
        let proxy = req.proxy.unwrap().into_proxy().unwrap();
        assert_eq!(proxy.url, "socks5://10.0.0.1:1080");
        assert_eq!(proxy.username.as_deref(), Some("u"));
        assert_eq!(proxy.password.as_deref(), Some("p"));
    }

    #[test]
    fn parses_flaresolverrsharp_request() {
        // FlareSolverrSharp omits null fields.
        let req: V1Request = serde_json::from_value(json!({
            "cmd": "request.get", "url": "https://example.com/", "maxTimeout": 55000
        }))
        .unwrap();
        assert!(req.proxy.is_none() && req.session.is_none());
    }

    #[test]
    fn parses_all_documented_parameters() {
        let req: V1Request = serde_json::from_value(json!({
            "cmd": "request.get",
            "url": "https://example.com/",
            "maxTimeout": "45000",
            "session": "abc",
            "session_ttl_minutes": 10,
            "cookies": [{"name": "c1", "value": "v1"}],
            "returnOnlyCookies": true,
            "waitInSeconds": 2,
            "returnScreenshot": false,
            "userAgent": "ignored"
        }))
        .unwrap();
        assert_eq!(
            req.max_timeout,
            Some(45000.0),
            "numeric string, as int() allows"
        );
        assert_eq!(req.session.as_deref(), Some("abc"));
        assert_eq!(req.session_ttl_minutes, Some(10.0));
        assert_eq!(req.cookies.unwrap()[0].name, "c1");
        assert_eq!(req.return_only_cookies, Some(true));
        assert_eq!(req.wait_in_seconds, Some(2.0));
    }

    #[test]
    fn bad_max_timeout_counts_as_absent() {
        let req: V1Request = serde_json::from_value(json!({"maxTimeout": "soon"})).unwrap();
        assert_eq!(req.max_timeout, None);
        let req: V1Request = serde_json::from_value(json!({"maxTimeout": null})).unwrap();
        assert_eq!(req.max_timeout, None);
    }

    #[test]
    fn serializes_solution_response() {
        let cookie = Cookie {
            domain: ".example.com".into(),
            expires: 1700000000.5,
            http_only: true,
            secure: true,
            ..Cookie::new("cf_clearance", "abc")
        };
        let res = V1Response {
            status: "ok".into(),
            message: "Challenge solved!".into(),
            solution: Some(Solution {
                url: "https://example.com/".into(),
                status: 200,
                headers: Some(BTreeMap::from([(
                    "content-type".into(),
                    "text/html".into(),
                )])),
                response: Some("<html></html>".into()),
                cookies: vec![cookie.into(), Cookie::new("s", "1").into()],
                user_agent: "UA".into(),
            }),
            start_timestamp: 1594872947467,
            end_timestamp: 1594872949617,
            version: "0.1.0".into(),
            ..V1Response::default()
        };
        assert_eq!(
            serde_json::to_value(&res).unwrap(),
            json!({
                "status": "ok",
                "message": "Challenge solved!",
                "solution": {
                    "url": "https://example.com/",
                    "status": 200,
                    "headers": {"content-type": "text/html"},
                    "response": "<html></html>",
                    "cookies": [
                        {"name": "cf_clearance", "value": "abc", "domain": ".example.com",
                         "path": "/", "expires": 1700000000.5, "size": 15, "httpOnly": true,
                         "secure": true, "session": false, "sameSite": "None"},
                        {"name": "s", "value": "1", "domain": "", "path": "/", "expires": -1.0,
                         "size": 2, "httpOnly": false, "secure": false, "session": true,
                         "sameSite": "None"}
                    ],
                    "userAgent": "UA"
                },
                "startTimestamp": 1594872947467u64,
                "endTimestamp": 1594872949617u64,
                "version": "0.1.0"
            })
        );
    }

    #[test]
    fn error_response_has_no_solution_or_session_keys() {
        let res = V1Response {
            status: "error".into(),
            message: "Error: boom".into(),
            ..V1Response::default()
        };
        let value = serde_json::to_value(&res).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "endTimestamp",
                "message",
                "startTimestamp",
                "status",
                "version"
            ]
        );
    }
}
