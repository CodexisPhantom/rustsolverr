//! Chromium backend over the DevTools protocol (chromiumoxide). Optional and
//! less capable than Camoufox: a stock Chromium has no stealth patches.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chromiumoxide::auth::Credentials;
use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::network::{CookieParam, SetCookiesParams};
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams,
};
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use rustsolverr_common::{Cookie, Error, Method, Page, Request, Result};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::{classifier_js, post_script, BrowserBackend};

const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

fn fail(e: impl std::fmt::Display) -> Error {
    Error::Browser(e.to_string())
}

struct Running {
    browser: Browser,
    /// Drives the DevTools connection.
    handler: JoinHandle<()>,
}

pub struct ChromiumBackend {
    chrome_path: Option<PathBuf>,
    headless: bool,
    /// Extra command line flags, for example `--no-sandbox`.
    args: Vec<String>,
    /// Solves hold it shared; launch and shutdown hold it exclusive.
    state: Arc<RwLock<Option<Running>>>,
}

impl ChromiumBackend {
    pub fn new(chrome_path: Option<PathBuf>, headless: bool, args: Vec<String>) -> Self {
        ChromiumBackend {
            chrome_path,
            headless,
            args,
            state: Arc::default(),
        }
    }

    async fn launch(&self) -> Result<Running> {
        tracing::info!("starting Chromium");
        let mut config = BrowserConfig::builder().args(self.args.clone());
        if self.args.iter().any(|a| a == "--no-sandbox") {
            config = config.no_sandbox();
        }
        config = if self.headless {
            config.new_headless_mode()
        } else {
            config.with_head()
        };
        if let Some(path) = &self.chrome_path {
            config = config.chrome_executable(path);
        }
        let (browser, mut handler) = Browser::launch(config.build().map_err(fail)?)
            .await
            .map_err(fail)?;
        let handler = tokio::spawn(async move { while handler.next().await.is_some() {} });
        Ok(Running { browser, handler })
    }
}

fn cookie_param(cookie: &Cookie, url: &str) -> Result<CookieParam> {
    let mut param = CookieParam::builder()
        .name(cookie.name.clone())
        .value(cookie.value.clone());
    param = if cookie.domain.is_empty() {
        param.url(url)
    } else {
        param
            .domain(cookie.domain.clone())
            .path(cookie.path.clone())
            .secure(cookie.secure)
            .http_only(cookie.http_only)
    };
    param.build().map_err(fail)
}

async fn solve_in(browser: &Browser, context: BrowserContextId, req: &Request) -> Result<Page> {
    let target = CreateTargetParams::builder()
        .url("about:blank")
        .browser_context_id(context)
        .build()
        .map_err(fail)?;
    let page = browser.new_page(target).await.map_err(fail)?;

    if let Some(proxy) = &req.proxy {
        if let Some(username) = &proxy.username {
            page.authenticate(Credentials {
                username: username.clone(),
                password: proxy.password.clone().unwrap_or_default(),
            })
            .await
            .map_err(fail)?;
        }
    }
    if !req.cookies.is_empty() {
        let cookies = req
            .cookies
            .iter()
            .map(|c| cookie_param(c, &req.url))
            .collect::<Result<Vec<_>>>()?;
        page.execute(SetCookiesParams::new(cookies))
            .await
            .map_err(fail)?;
    }

    match req.method {
        Method::Get => {
            page.goto(req.url.as_str()).await.map_err(fail)?;
        }
        Method::Post => {
            let script = post_script(&req.url, req.post_data.as_deref().unwrap_or(""));
            page.evaluate(script).await.map_err(fail)?;
            let _ = page.wait_for_navigation().await;
        }
    }

    let classifier = classifier_js();
    loop {
        // An evaluation fails while the page navigates: poll again.
        let state = match page.evaluate(classifier.as_str()).await {
            Ok(result) => result.into_value::<String>().unwrap_or_default(),
            Err(_) => String::new(),
        };
        match state.as_str() {
            "denied" => return Err(Error::Blocked),
            "clear" => break,
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
    if !req.wait.is_zero() {
        tokio::time::sleep(req.wait).await;
    }

    let user_agent = page
        .evaluate("navigator.userAgent")
        .await
        .map_err(fail)?
        .into_value::<String>()
        .map_err(fail)?;
    let cookies = page
        .get_cookies()
        .await
        .map_err(fail)?
        .into_iter()
        .map(|c| Cookie {
            name: c.name,
            value: c.value,
            domain: c.domain,
            path: c.path,
            expires: if c.session { -1.0 } else { c.expires },
            http_only: c.http_only,
            secure: c.secure,
            same_site: c
                .same_site
                .map(|s| format!("{s:?}"))
                .unwrap_or_else(|| "None".into()),
        })
        .collect();
    Ok(Page {
        url: page.url().await.map_err(fail)?.unwrap_or_default(),
        // The DevTools page API does not expose the document response here;
        // FlareSolverr reports a fixed 200 for the same reason.
        status: 200,
        headers: Default::default(),
        body: page.content().await.map_err(fail)?,
        cookies,
        user_agent,
    })
}

#[async_trait]
impl BrowserBackend for ChromiumBackend {
    async fn solve(&self, req: &Request) -> Result<Page> {
        let mut guard = self.state.clone().write_owned().await;
        if guard.as_ref().is_some_and(|r| r.handler.is_finished()) {
            tracing::warn!("Chromium exited unexpectedly; restarting it");
            *guard = None;
        }
        if guard.is_none() {
            *guard = Some(self.launch().await?);
        }
        let guard = guard.downgrade();
        let req = req.clone();

        // The work runs in its own task: when the caller drops this future
        // (request timeout), the task still reaches its own timeout and
        // disposes the browser context.
        let task = tokio::spawn(async move {
            let browser = &guard.as_ref().expect("launched above").browser;
            let context = browser
                .create_browser_context(CreateBrowserContextParams {
                    proxy_server: req.proxy.as_ref().map(|p| p.url.clone()),
                    ..Default::default()
                })
                .await
                .map_err(fail)?;
            let result =
                tokio::time::timeout(req.timeout, solve_in(browser, context.clone(), &req)).await;
            if let Err(e) = browser.dispose_browser_context(context).await {
                tracing::warn!("cannot dispose browser context: {e}");
            }
            result.unwrap_or(Err(Error::Timeout))
        });
        task.await.map_err(fail)?
    }

    async fn shutdown(&self) {
        let Some(mut running) = self.state.write().await.take() else {
            return;
        };
        tracing::info!("stopping Chromium");
        let _ = running.browser.close().await;
        if tokio::time::timeout(EXIT_TIMEOUT, running.browser.wait())
            .await
            .is_err()
        {
            let _ = running.browser.kill().await;
        }
        running.handler.abort();
    }
}
