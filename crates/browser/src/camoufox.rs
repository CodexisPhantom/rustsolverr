//! Camoufox backend: a Python sidecar that drives Camoufox through its
//! official Python API. See `sidecar.py` for the line protocol.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rustsolverr_common::{Cookie, Error, Method, Page, Request, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use crate::{bridge_proxy, classifier_js, post_script, BrowserBackend};

const SIDECAR: &str = include_str!("sidecar.py");
/// Time for Python and the browser to start.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(90);
/// Time for a clean exit before the process group is killed.
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct CamoufoxBackend {
    python: String,
    headless: String,
    sidecar: tokio::sync::Mutex<Option<Arc<Sidecar>>>,
    next_id: AtomicU64,
}

struct Sidecar {
    child: tokio::sync::Mutex<Child>,
    pid: Option<u32>,
    /// Lines for the sidecar's stdin. Closing it makes the sidecar exit.
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    alive: Arc<AtomicBool>,
}

/// Kills the process and every descendant.
fn kill_tree(pid: u32) {
    #[cfg(unix)]
    // SAFETY: plain syscall. The sidecar leads its own process group
    // (`process_group(0)`), so the group id equals its pid.
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Sidecar {
    async fn launch(python: &str, headless: &str) -> Result<Sidecar> {
        let mut cmd = Command::new(python);
        cmd.args(["-u", "-c", SIDECAR])
            .env("HEADLESS", headless)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Browser(format!("cannot start `{python}`: {e}")))?;
        let pid = child.id();
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let mut lines = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        // From here on, dropping `sidecar` kills the process group.
        let sidecar = Sidecar {
            child: tokio::sync::Mutex::new(child),
            pid,
            tx,
            pending: pending.clone(),
            alive: alive.clone(),
        };

        let ready = tokio::time::timeout(LAUNCH_TIMEOUT, lines.next_line()).await;
        match ready {
            Ok(Ok(Some(line))) if line.contains("\"ready\"") => {}
            Ok(Ok(Some(line))) => {
                return Err(Error::Browser(format!("unexpected sidecar output: {line}")))
            }
            Ok(_) => {
                return Err(Error::Browser(
                    "the Camoufox sidecar exited at start; see its error output above \
                     (is the `camoufox` Python package installed and `camoufox fetch` done?)"
                        .into(),
                ))
            }
            Err(_) => return Err(Error::Browser("timeout starting Camoufox".into())),
        }

        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(reply) = serde_json::from_str::<Value>(&line) else {
                    tracing::warn!("ignoring sidecar output: {line}");
                    continue;
                };
                let waiter = reply["id"]
                    .as_u64()
                    .and_then(|id| pending.lock().unwrap().remove(&id));
                if let Some(waiter) = waiter {
                    let _ = waiter.send(reply);
                }
            }
            // The process is gone. Dropping the senders fails every waiting solve.
            alive.store(false, Ordering::SeqCst);
            pending.lock().unwrap().clear();
        });
        Ok(sidecar)
    }

    /// Asks for a clean exit, then kills whatever is left.
    async fn stop(&self) {
        let _ = self.tx.send(json!({"op": "shutdown"}).to_string());
        let mut child = self.child.lock().await;
        if tokio::time::timeout(EXIT_TIMEOUT, child.wait())
            .await
            .is_err()
        {
            tracing::warn!("Camoufox sidecar did not exit; killing it");
        }
        if let Some(pid) = self.pid {
            kill_tree(pid);
        }
        let _ = child.wait().await;
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            kill_tree(pid);
        }
    }
}

/// Cancels the solve in the sidecar when the Rust future is dropped, so the
/// browser context closes on a timeout.
struct CancelGuard {
    sidecar: Arc<Sidecar>,
    id: u64,
    done: bool,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if !self.done {
            self.sidecar.pending.lock().unwrap().remove(&self.id);
            let _ = self
                .sidecar
                .tx
                .send(json!({"op": "cancel", "id": self.id}).to_string());
        }
    }
}

/// Cookie in the shape Playwright's `add_cookies` accepts.
fn playwright_cookie(cookie: &Cookie, url: &str) -> Value {
    if cookie.domain.is_empty() {
        return json!({"name": cookie.name, "value": cookie.value, "url": url});
    }
    let mut out = json!({
        "name": cookie.name,
        "value": cookie.value,
        "domain": cookie.domain,
        "path": cookie.path,
        "expires": cookie.expires,
        "httpOnly": cookie.http_only,
        "secure": cookie.secure,
    });
    if ["Strict", "Lax", "None"].contains(&cookie.same_site.as_str()) {
        out["sameSite"] = json!(cookie.same_site);
    }
    out
}

fn parse_reply(reply: Value) -> Result<Page> {
    if reply["ok"].as_bool() != Some(true) {
        return Err(match reply["kind"].as_str() {
            Some("timeout") => Error::Timeout,
            Some("blocked") => Error::Blocked,
            _ => Error::Browser(reply["error"].as_str().unwrap_or("unknown error").into()),
        });
    }
    let page = &reply["page"];
    let bad = |what: &str| Error::Browser(format!("bad sidecar reply: {what}"));
    Ok(Page {
        url: page["url"].as_str().ok_or_else(|| bad("url"))?.into(),
        status: page["status"].as_u64().unwrap_or(200) as u16,
        headers: serde_json::from_value(page["headers"].clone()).map_err(|_| bad("headers"))?,
        body: page["body"].as_str().ok_or_else(|| bad("body"))?.into(),
        cookies: serde_json::from_value(page["cookies"].clone()).map_err(|_| bad("cookies"))?,
        user_agent: page["userAgent"]
            .as_str()
            .ok_or_else(|| bad("userAgent"))?
            .into(),
    })
}

impl CamoufoxBackend {
    /// `python`: interpreter that has the `camoufox` package.
    /// `headless`: `true`, `false` or `virtual`.
    pub fn new(python: impl Into<String>, headless: impl Into<String>) -> Self {
        CamoufoxBackend {
            python: python.into(),
            headless: headless.into(),
            sidecar: tokio::sync::Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// Returns the running sidecar; starts one when none runs or it crashed.
    async fn ensure(&self) -> Result<Arc<Sidecar>> {
        let mut slot = self.sidecar.lock().await;
        if let Some(sidecar) = slot.as_ref() {
            if sidecar.alive.load(Ordering::SeqCst) {
                return Ok(sidecar.clone());
            }
            tracing::warn!("Camoufox sidecar exited unexpectedly; restarting it");
        }
        *slot = None; // drops, and so kills, a dead sidecar's leftovers
        tracing::info!("starting Camoufox");
        let sidecar = Arc::new(Sidecar::launch(&self.python, &self.headless).await?);
        *slot = Some(sidecar.clone());
        Ok(sidecar)
    }
}

#[async_trait]
impl BrowserBackend for CamoufoxBackend {
    async fn solve(&self, req: &Request) -> Result<Page> {
        let (req, _bridge) = bridge_proxy(req).await?;
        let sidecar = self.ensure().await?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        sidecar.pending.lock().unwrap().insert(id, tx);
        let mut guard = CancelGuard {
            sidecar: sidecar.clone(),
            id,
            done: false,
        };

        let post = match (req.method, &req.post_data) {
            (Method::Post, data) => Some(post_script(&req.url, data.as_deref().unwrap_or(""))),
            (Method::Get, _) => None,
        };
        let command = json!({
            "op": "solve",
            "id": id,
            "url": req.url,
            "postScript": post,
            "cookies": req.cookies.iter().map(|c| playwright_cookie(c, &req.url)).collect::<Vec<_>>(),
            "proxy": req.proxy.as_ref().map(|p| {
                let mut proxy = json!({"server": p.url});
                if let Some(username) = &p.username {
                    proxy["username"] = json!(username);
                    proxy["password"] = json!(p.password.as_deref().unwrap_or(""));
                }
                proxy
            }),
            "timeoutMs": req.timeout.as_millis() as u64,
            "waitMs": req.wait.as_millis() as u64,
            "classifier": classifier_js(),
        });
        let gone = || Error::Browser("the Camoufox process exited during the solve".into());
        sidecar.tx.send(command.to_string()).map_err(|_| gone())?;
        let reply = rx.await.map_err(|_| gone())?;
        guard.done = true;
        parse_reply(reply)
    }

    async fn shutdown(&self) {
        if let Some(sidecar) = self.sidecar.lock().await.take() {
            tracing::info!("stopping Camoufox");
            sidecar.stop().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_cookies_for_playwright() {
        let bare = playwright_cookie(&Cookie::new("a", "b"), "http://x/");
        assert_eq!(bare, json!({"name": "a", "value": "b", "url": "http://x/"}));

        let full = Cookie {
            domain: ".x.test".into(),
            same_site: "bogus".into(),
            ..Cookie::new("a", "b")
        };
        let out = playwright_cookie(&full, "http://x/");
        assert_eq!(out["domain"], ".x.test");
        assert!(out.get("url").is_none());
        assert!(out.get("sameSite").is_none(), "invalid sameSite is omitted");
    }

    #[test]
    fn parses_replies() {
        let ok = json!({"id": 1, "ok": true, "page": {
            "url": "http://x/", "status": 200, "headers": {"server": "x"}, "body": "<html>",
            "cookies": [{"name": "cf_clearance", "value": "v", "domain": "x", "path": "/",
                         "expires": -1, "httpOnly": true, "secure": false, "sameSite": "Lax"}],
            "userAgent": "UA"}});
        let page = parse_reply(ok).unwrap();
        assert_eq!(page.cookies[0].name, "cf_clearance");
        assert_eq!(page.user_agent, "UA");
        assert_eq!(page.headers["server"], "x");

        let reply = |kind: &str| json!({"id": 1, "ok": false, "kind": kind, "error": "boom"});
        assert_eq!(parse_reply(reply("timeout")), Err(Error::Timeout));
        assert_eq!(parse_reply(reply("blocked")), Err(Error::Blocked));
        assert_eq!(
            parse_reply(reply("error")),
            Err(Error::Browser("boom".into()))
        );
    }
}
