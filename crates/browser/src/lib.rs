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
}
