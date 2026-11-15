//! Browser backends: the tier that solves a challenge with a real browser.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rustsolverr_challenge as challenge;
use rustsolverr_common::{Page, Request, Result};
use serde_json::json;
use tokio::sync::{RwLock, Semaphore};
use tokio::time::Instant;

mod camoufox;
pub use camoufox::CamoufoxBackend;

#[cfg(feature = "chromium")]
mod chromium;
#[cfg(feature = "chromium")]
pub use chromium::ChromiumBackend;

#[async_trait]
pub trait BrowserBackend: Send + Sync {
    /// Loads the page in a fresh browser context and waits until no challenge
    /// is shown. `Page::cookies` and `Page::user_agent` come from that browser.
    /// Starts the browser when it is not running. Must close the context when
    /// the returned future is dropped.
    async fn solve(&self, req: &Request) -> Result<Page>;

    /// Stops the browser process. A later `solve` starts it again.
    async fn shutdown(&self);
}

/// Adds the lifecycle rules to a backend: at most `max_concurrent` solves at
/// a time, and shutdown after an idle period.
pub struct Managed {
    inner: Arc<dyn BrowserBackend>,
    permits: Semaphore,
    /// Solves hold it shared; shutdown holds it exclusive.
    gate: RwLock<()>,
    last_used: Mutex<Instant>,
}

/// Records the end of a solve, also when the solve future is dropped.
struct Touch<'a>(&'a Mutex<Instant>);

impl Drop for Touch<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = Instant::now();
    }
}

impl Managed {
    pub fn new(inner: Arc<dyn BrowserBackend>, max_concurrent: usize) -> Arc<Self> {
        Arc::new(Managed {
            inner,
            permits: Semaphore::new(max_concurrent),
            gate: RwLock::new(()),
            last_used: Mutex::new(Instant::now()),
        })
    }

    /// Shuts the browser down when no solve ran for `idle`.
    pub fn spawn_idle_reaper(self: &Arc<Self>, idle: Duration) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        let tick = (idle / 4).max(Duration::from_secs(1));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                // try_write fails while a solve runs.
                if let Ok(_gate) = this.gate.try_write() {
                    let last = *this.last_used.lock().unwrap();
                    if last.elapsed() >= idle {
                        this.inner.shutdown().await;
                    }
                }
            }
        })
    }
}

#[async_trait]
impl BrowserBackend for Managed {
    async fn solve(&self, req: &Request) -> Result<Page> {
        let _permit = self
            .permits
            .acquire()
            .await
            .expect("semaphore never closes");
        let _gate = self.gate.read().await;
        let _touch = Touch(&self.last_used);
        self.inner.solve(req).await
    }

    async fn shutdown(&self) {
        let _gate = self.gate.write().await;
        self.inner.shutdown().await;
    }
}

/// JavaScript expression that classifies the current document as
/// `"denied"`, `"challenge"` or `"clear"`, with FlareSolverr's rules.
pub(crate) fn classifier_js() -> String {
    format!(
        r#"(() => {{
  const title = document.title || "";
  const has = (selectors) => selectors.some((s) => document.querySelector(s));
  if ({denied_titles}.some((t) => title.startsWith(t)) || has({denied_selectors})) return "denied";
  if ({titles}.some((t) => t.toLowerCase() === title.toLowerCase()) || has({selectors})) return "challenge";
  if (location.href === "about:blank" || document.readyState === "loading") return "challenge";
  return "clear";
}})()"#,
        denied_titles = json!(challenge::ACCESS_DENIED_TITLES),
        denied_selectors = json!(challenge::ACCESS_DENIED_SELECTORS),
        titles = json!(challenge::CHALLENGE_TITLES),
        selectors = json!(challenge::CHALLENGE_SELECTORS),
    )
}

/// JavaScript expression that submits `post_data` (form-urlencoded) to `url`
/// as a form POST, so the browser itself navigates to the result.
pub(crate) fn post_script(url: &str, post_data: &str) -> String {
    let fields: Vec<(String, String)> = url::form_urlencoded::parse(post_data.as_bytes())
        .into_owned()
        .collect();
    format!(
        r#"(() => {{
  const form = document.createElement("form");
  form.method = "POST";
  form.action = {url};
  for (const [name, value] of {fields}) {{
    const input = document.createElement("input");
    input.type = "hidden";
    input.name = name;
    input.value = value;
    form.appendChild(input);
  }}
  document.documentElement.appendChild(form);
  HTMLFormElement.prototype.submit.call(form);
}})()"#,
        url = json!(url),
        fields = json!(fields),
    )
}

/// Local loopback SOCKS5 forwarder that adds RFC 1929 username/password
/// authentication on the upstream leg (neither Playwright Firefox nor Chromium
/// supports SOCKS5 authentication natively).
pub(crate) struct Socks5Bridge {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Socks5Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn relay_socks5(
    mut client: tokio::net::TcpStream,
    host: String,
    port: u16,
    username: String,
    password: String,
) -> std::io::Result<()> {
    use std::io::{Error as IoError, ErrorKind};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut greeting = [0u8; 2];
    client.read_exact(&mut greeting).await?;
    if greeting[0] != 0x05 {
        return Err(IoError::new(ErrorKind::InvalidData, "not socks5"));
    }
    let mut methods = vec![0u8; greeting[1] as usize];
    client.read_exact(&mut methods).await?;

    let mut upstream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
    upstream.write_all(&[0x05, 0x02, 0x00, 0x02]).await?;
    let mut choice = [0u8; 2];
    upstream.read_exact(&mut choice).await?;
    if choice[0] != 0x05 {
        return Err(IoError::new(ErrorKind::InvalidData, "bad socks5 reply"));
    }
    match choice[1] {
        0x00 => {}
        0x02 => {
            let u = username.as_bytes();
            let p = password.as_bytes();
            if u.len() > 255 || p.len() > 255 {
                return Err(IoError::new(
                    ErrorKind::InvalidInput,
                    "socks5 credentials exceed 255 bytes",
                ));
            }
            let mut auth = Vec::with_capacity(3 + u.len() + p.len());
            auth.push(0x01);
            auth.push(u.len() as u8);
            auth.extend_from_slice(u);
            auth.push(p.len() as u8);
            auth.extend_from_slice(p);
            upstream.write_all(&auth).await?;
            let mut status = [0u8; 2];
            upstream.read_exact(&mut status).await?;
            if status[1] != 0x00 {
                return Err(IoError::new(
                    ErrorKind::PermissionDenied,
                    "socks5 authentication failed",
                ));
            }
        }
        _ => {
            return Err(IoError::new(
                ErrorKind::PermissionDenied,
                "unsupported socks5 auth method",
            ))
        }
    }

    client.write_all(&[0x05, 0x00]).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

/// Normalizes `req.proxy` and, when it is a `socks5://` proxy with credentials,
/// starts a local unauthenticated SOCKS5 bridge for the browser.
pub(crate) async fn bridge_proxy(req: &Request) -> Result<(Request, Option<Socks5Bridge>)> {
    use rustsolverr_common::{Error, Proxy};

    let mut out = req.clone();
    out.proxy = out.proxy.map(Proxy::normalized);
    let Some(proxy) = &out.proxy else {
        return Ok((out, None));
    };
    let Some(username) = proxy.username.clone() else {
        return Ok((out, None));
    };
    let parsed = url::Url::parse(&proxy.url)
        .map_err(|e| Error::Invalid(format!("invalid proxy url {}: {e}", proxy.url)))?;
    if !matches!(parsed.scheme(), "socks5" | "socks5h") {
        return Ok((out, None));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::Invalid(format!("invalid proxy url: {}", proxy.url)))?
        .to_string();
    let port = parsed.port().unwrap_or(1080);
    let password = proxy.password.clone().unwrap_or_default();

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| Error::Browser(format!("cannot bind local socks5 bridge: {e}")))?;
    let local_port = listener
        .local_addr()
        .map_err(|e| Error::Browser(e.to_string()))?
        .port();
    let task = tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let (h, u, p) = (host.clone(), username.clone(), password.clone());
            tokio::spawn(async move {
                let _ = relay_socks5(client, h, port, u, p).await;
            });
        }
    });
    out.proxy = Some(Proxy {
        url: format!("socks5://127.0.0.1:{local_port}"),
        username: None,
        password: None,
    });
    Ok((out, Some(Socks5Bridge { task })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counting {
        running: AtomicUsize,
        max_running: AtomicUsize,
        shutdowns: AtomicUsize,
    }

    #[async_trait]
    impl BrowserBackend for Counting {
        async fn solve(&self, _req: &Request) -> Result<Page> {
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_running.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.running.fetch_sub(1, Ordering::SeqCst);
            Ok(Page::default())
        }

        async fn shutdown(&self) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn limits_concurrent_solves() {
        let backend = Arc::new(Counting::default());
        let managed = Managed::new(backend.clone(), 2);
        let tasks: Vec<_> = (0..6)
            .map(|_| {
                let managed = managed.clone();
                tokio::spawn(async move { managed.solve(&Request::get("http://x")).await })
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(backend.max_running.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_reaper_waits_for_idle_period() {
        let backend = Arc::new(Counting::default());
        let managed = Managed::new(backend.clone(), 1);
        managed.spawn_idle_reaper(Duration::from_secs(60));

        tokio::time::sleep(Duration::from_secs(40)).await;
        managed.solve(&Request::get("http://x")).await.unwrap();
        tokio::time::sleep(Duration::from_secs(40)).await;
        assert_eq!(
            backend.shutdowns.load(Ordering::SeqCst),
            0,
            "used 40 s ago: not idle"
        );
        tokio::time::sleep(Duration::from_secs(40)).await;
        assert!(backend.shutdowns.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn scripts_embed_values_as_json() {
        let js = classifier_js();
        assert!(js.contains(r#""Just a moment...""#));
        assert!(js.contains(r##""#cf-challenge-running""##));
        let js = post_script("http://x/?a=\"1\"", "user=a%20b&q=%3C");
        assert!(js.contains(r#"form.action = "http://x/?a=\"1\"""#));
        assert!(js.contains(r#"[["user","a b"],["q","<"]]"#));
    }

    #[tokio::test]
    async fn socks5_bridge_authenticates_with_upstream() {
        use rustsolverr_common::Proxy;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let upstream = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut s, _) = upstream.accept().await.unwrap();
            let mut g = [0u8; 4];
            s.read_exact(&mut g).await.unwrap();
            assert_eq!(g, [0x05, 0x02, 0x00, 0x02]);
            s.write_all(&[0x05, 0x02]).await.unwrap();
            // RFC 1929: 0x01, ulen=3, "bob", plen=6, "secret"
            let mut auth = [0u8; 12];
            s.read_exact(&mut auth).await.unwrap();
            assert_eq!(&auth, b"\x01\x03bob\x06secret");
            s.write_all(&[0x01, 0x00]).await.unwrap();
            let mut payload = [0u8; 4];
            s.read_exact(&mut payload).await.unwrap();
            assert_eq!(&payload, b"ping");
            s.write_all(b"pong").await.unwrap();
        });

        let mut req = Request::get("http://example.com/");
        req.proxy = Some(Proxy {
            url: format!("socks5://bob:secret@127.0.0.1:{upstream_port}"),
            username: None,
            password: None,
        });
        let (bridged, _guard) = bridge_proxy(&req).await.unwrap();
        let proxy = bridged.proxy.unwrap();
        assert!(proxy.username.is_none() && proxy.password.is_none());
        let addr = proxy.url.strip_prefix("socks5://").unwrap();

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0x00]);
        client.write_all(b"ping").await.unwrap();
        let mut pong = [0u8; 4];
        client.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"pong");
        server.await.unwrap();
    }
}
