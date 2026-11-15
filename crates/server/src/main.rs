use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use rustsolverr_browser::{BrowserBackend, CamoufoxBackend, Managed};
use rustsolverr_cache::CookieCache;
use rustsolverr_config::{Backend, Config};
use rustsolverr_fetch::WreqFetcher;
use rustsolverr_solver::Solver;
use tracing_subscriber::EnvFilter;

/// Time that running requests get to finish after a stop signal.
const GRACE: Duration = Duration::from_secs(5);

fn backend(config: &Config) -> anyhow::Result<Arc<dyn BrowserBackend>> {
    Ok(match config.backend {
        Backend::Camoufox => Arc::new(CamoufoxBackend::new(&config.python, &config.headless)),
        #[cfg(feature = "chromium")]
        Backend::Chromium => Arc::new(rustsolverr_browser::ChromiumBackend::new(
            config.chrome_path.clone(),
            config.headless != "false",
            config.chrome_args.clone(),
        )),
        #[cfg(not(feature = "chromium"))]
        Backend::Chromium => anyhow::bail!("this build has no chromium backend"),
    })
}

/// Resolves on Ctrl-C or, on Unix, SIGTERM.
async fn stop_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("can listen for SIGTERM");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let filter = EnvFilter::try_new(&config.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let browser = Managed::new(backend(&config)?, config.max_concurrent_solves);
    browser.spawn_idle_reaper(config.browser_idle_timeout);
    let solver = Arc::new(Solver::new(
        Arc::new(WreqFetcher::new()?),
        browser.clone(),
        CookieCache::new(config.cookie_cache_ttl, config.cache_file.clone()),
        config.proxy.clone(),
    ));

    let listener = tokio::net::TcpListener::bind((config.host, config.port))
        .await
        .with_context(|| format!("cannot listen on {}:{}", config.host, config.port))?;
    tracing::info!(
        "rustsolverr {} listening on {}:{} ({:?} backend)",
        rustsolverr_api::VERSION,
        config.host,
        config.port,
        config.backend
    );

    let server = axum::serve(listener, rustsolverr_api::router(solver))
        .with_graceful_shutdown(stop_signal());
    tokio::select! {
        result = server => result?,
        _ = async { stop_signal().await; tokio::time::sleep(GRACE).await } => {
            tracing::warn!("requests still running after {GRACE:?}; stopping anyway");
        }
    }

    // Never leave a browser behind. If a solve still holds the browser, the
    // runtime shutdown drops it, and that kills the process group.
    if tokio::time::timeout(GRACE, browser.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("browser still busy; killing it");
    }
    tracing::info!("stopped");
    Ok(())
}
