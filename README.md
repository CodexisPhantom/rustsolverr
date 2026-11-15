# rustsolverr

A self-hosted challenge-solving proxy that speaks the [FlareSolverr](https://github.com/FlareSolverr/FlareSolverr)
v1 API. It is a drop-in replacement for FlareSolverr or Byparr in an *arr stack
(Prowlarr, Jackett): change the URL, nothing else.

rustsolverr does not contain any anti-detection technique of its own. The
challenge is solved by an existing stealth browser ([Camoufox](https://camoufox.com)).
rustsolverr is the orchestrator around it, and its goal is to start that
browser as rarely as possible:

- A small Rust binary serves the API. No browser runs while it is idle.
- The browser starts on the first request that needs it and stops after an idle period.
- Cookies and the user-agent of a solve are cached. Later requests for the same
  site are answered by a plain HTTP client that has the browser's TLS and HTTP/2
  fingerprint.
- The browser runs in its own process group, which is killed on shutdown and on a crash.

## Quick start

```sh
docker compose up -d --build     # rustsolverr on :8191, next to Prowlarr
curl http://localhost:8191/health
```

`docker-compose.yml` is an example with Prowlarr. To run only rustsolverr:

```sh
docker build -t rustsolverr .
docker run -d --name rustsolverr -p 8191:8191 --shm-size 1g -v rustsolverr-config:/config rustsolverr
```

Give the container `--shm-size 1g` (or `shm_size: 1gb`): Firefox needs more
shared memory than Docker's 64 MB default.

**Prowlarr:** Settings > Indexers > Add (Indexer Proxies) > FlareSolverr. Host
`http://rustsolverr:8191`. Give the proxy and the indexer the same tag.

**Jackett:** set "FlareSolverr API URL" to `http://rustsolverr:8191`.

A request looks the same as with FlareSolverr:

```sh
curl -X POST http://localhost:8191/v1 -H 'Content-Type: application/json' \
  -d '{"cmd": "request.get", "url": "https://example.com/", "maxTimeout": 60000}'
```

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `PORT` | `8191` | Listen port. |
| `HOST` | `0.0.0.0` | Listen address. |
| `LOG_LEVEL` | `info` | `trace`, `debug`, `info`, `warn`, `error`, or a `tracing` filter such as `rustsolverr_solver=debug`. |
| `MAX_CONCURRENT_SOLVES` | `2` | Browser solves that may run at the same time. More requests wait. |
| `BROWSER_IDLE_TIMEOUT` | `300` | Seconds without a solve after which the browser is stopped. |
| `COOKIE_CACHE_TTL` | `86400` | Seconds a cached clearance is kept. A clearance that stops working is dropped earlier, on its first challenged replay. |
| `CACHE_FILE` | unset (image: `/config/cookies.json`) | JSON file that persists the cookie cache. Unset: memory only. |
| `PROXY_URL` | unset | Default proxy for requests without a `proxy` parameter. `http://`, `socks4://` or `socks5://`. |
| `PROXY_USERNAME`, `PROXY_PASSWORD` | unset | Credentials for `PROXY_URL`. |
| `BROWSER_BACKEND` | `camoufox` | `camoufox` or `chromium`. |
| `HEADLESS` | `true` (image: `virtual`) | `true`, `false`, or `virtual` (Camoufox with a window inside Xvfb; Linux only). |
| `PYTHON` | `python3` | Python interpreter that has the `camoufox` package. |
| `CHROME_PATH` | auto-detect | Chromium executable for the `chromium` backend. |
| `CHROME_ARGS` | unset | Extra Chromium flags, separated by spaces, for example `--no-sandbox`. |

The cache file contains clearance cookies. Treat it as a secret.

## API compatibility

Checked against the FlareSolverr source (`dtos.py`, `flaresolverr.py`,
`flaresolverr_service.py`) and against the clients: Prowlarr's
`FlareSolverr.cs` and FlareSolverrSharp (used by Jackett).

| Endpoint | Behaviour |
|---|---|
| `GET /` | `{"msg": "FlareSolverr is ready!", "version": ..., "userAgent": ...}` |
| `GET /health` | `{"status": "ok"}` |
| `POST /v1` | Commands `request.get`, `request.post`, `sessions.create`, `sessions.list`, `sessions.destroy`. |

- Parameters: `url`, `maxTimeout` (default 60000; a numeric string is accepted),
  `session`, `session_ttl_minutes`, `cookies`, `returnOnlyCookies`, `proxy`
  (`url`, `username`, `password`), `postData`, `waitInSeconds`.
- Response: `status`, `message`, `startTimestamp`, `endTimestamp` (milliseconds),
  `version`, and `solution` with `url`, `status`, `headers`, `response`,
  `cookies`, `userAgent`. Each cookie has `name`, `value`, `domain`, `path`,
  `expires`, `size`, `httpOnly`, `secure`, `session`, `sameSite`, with the JSON
  types the two clients deserialize.
- Errors are HTTP 500 with `"status": "error"` and FlareSolverr's messages, for
  example `Error: Error solving the challenge. Timeout after 60.0 seconds.`
  Both clients accept HTTP 200 and 500 only.
- `message` is `Challenge solved!` when the browser was used, and
  `Challenge not detected!` when the HTTP client was enough.

Differences from FlareSolverr:

- `version` is the rustsolverr version.
- `solution.status` and `solution.headers` are the real values of the final
  document. FlareSolverr always reports `200` and `{}`. (The `chromium`
  backend also reports `200` and `{}`.)
- A session is a named cookie jar with an optional proxy, not a browser that
  stays open. Cookies persist between requests of a session; other browser
  state (local storage, open page) does not.
- `returnScreenshot`, `disableMedia` and `tabs_till_verify` are accepted and
  ignored. `solution.turnstile_token` and `solution.screenshot` are never set.
- `request.post` without `url` is an error.
- Parameters that FlareSolverr v2 removed (`headers`, `userAgent`, `download`,
  `returnRawHtml`) are ignored, as in FlareSolverr.

## How the tiers work

Each `request.get` / `request.post` goes through up to three tiers, cheapest first.

1. **Cached clearance.** If the cache holds cookies and a user-agent for the
   site, the request is replayed by the HTTP client with those cookies and that
   exact user-agent. The client's TLS and HTTP/2 profile is chosen from the
   user-agent (a Firefox 135 user-agent gets the Firefox 135 profile). No
   browser starts. If the reply is a challenge, the cache entry is invalidated
   and the request falls through to tier 3.
2. **Plain fetch.** Without a cache entry, the HTTP client fetches the page
   directly. Many pages have no challenge at all.
3. **Browser solve.** Only when a challenge page is detected. The browser loads
   the page in a fresh context, waits until the challenge is gone, and returns
   the page, the cookies and its user-agent. These go into the cache.

**Challenge detection.** For an HTTP response: the `cf-mitigated: challenge`
header, the titles FlareSolverr uses (`Just a moment...`, `DDoS-Guard`), or a
403/429/503 status together with a marker of the Cloudflare interstitial
(`_cf_chl_opt`, `/cdn-cgi/challenge-platform/`, ...). In the browser:
FlareSolverr's title and CSS selector lists, tested against the live DOM. A
Cloudflare "Access denied" page in the browser is an error (`Cloudflare has
blocked this request...`).

**Cache key.** Host name plus proxy. Clearance is bound to the exit IP, so the
browser solve and the replay always use the same proxy: the request's `proxy`,
else the session's, else `PROXY_URL`. A clearance earned through one proxy is
never replayed through another.

**One solve per site.** A lock per cache key makes simultaneous requests for
one site wait for a single browser solve; the others then replay from the cache.

**Timeouts.** `maxTimeout` covers the whole request, including the wait for a
free browser slot. On a timeout the browser context of that request is closed
and the FlareSolverr timeout error is returned.

## Architecture

A Cargo workspace; each crate is one module boundary.

| Crate | Content |
|---|---|
| `crates/common` | Shared types: `Cookie`, `Proxy`, `Request`, `Page`, `Error`. |
| `crates/config` | Environment variables to `Config`. |
| `crates/challenge` | Challenge detection: marker lists and `detect()`. |
| `crates/cache` | Clearance cache with TTL and JSON persistence. |
| `crates/fetch` | `Fetcher` trait and `WreqFetcher`, the HTTP client with browser TLS/HTTP2 profiles. |
| `crates/browser` | `BrowserBackend` trait, `Managed` (concurrency limit, idle shutdown), `CamoufoxBackend`, `ChromiumBackend`. |
| `crates/solver` | The tiers, the per-site locks and the sessions. |
| `crates/api` | FlareSolverr wire types and the axum router. |
| `crates/server` | The `rustsolverr` binary: wiring and signal handling. |

### Why Camoufox is driven by a sidecar

Camoufox is a patched Firefox. Its automation interface is Playwright's
Juggler protocol, and its launcher (fingerprint generation, browser download,
virtual display) is a Python package. There were three ways to drive it from Rust:

1. **Its remote server mode** (`camoufox server`, a Playwright websocket) with
   a Rust Playwright client. Rejected: the Camoufox documentation calls this
   mode experimental and a workaround on undocumented Playwright methods; the
   websocket protocol is Playwright's internal one and must match the client
   version exactly; and Python is still needed to start it.
2. **Speaking Juggler directly from Rust.** Rejected: it would re-implement
   the launcher and a private protocol.
3. **A small Python sidecar** on the official `camoufox` Python API. Chosen.

The sidecar (`crates/browser/src/sidecar.py`, about 150 lines, embedded in the
binary) reads one JSON command per line on stdin and writes one JSON reply per
line on stdout. It uses only the documented API, so a Camoufox update needs a
`pip install -U`, not a protocol port. The cost is a Python process, but only
while the browser runs: after `BROWSER_IDLE_TIMEOUT` the sidecar and the
browser are both stopped, and the idle footprint is the Rust binary alone.

Lifecycle rules, all enforced on the Rust side:

- Lazy start; one sidecar, one browser; a fresh browser context per solve.
- The sidecar leads its own process group. On shutdown it is asked to exit,
  then the group gets `SIGKILL`. Dropping the handle (panic, runtime shutdown)
  kills the group too.
- If rustsolverr itself is killed, the sidecar sees stdin close, closes the
  browser and exits.
- If the sidecar or the browser crashes, waiting solves fail with an error and
  the next solve starts a new one.
- In the image, `tini` is PID 1 and reaps zombie processes.

### The Chromium backend

`BROWSER_BACKEND=chromium` drives a Chromium over the DevTools protocol with
[chromiumoxide](https://crates.io/crates/chromiumoxide). It proves the trait is
swappable and is useful for sites that only need JavaScript. A stock Chromium
has no stealth patches, so expect it to fail real Cloudflare challenges. The
Docker image does not include Chromium.

## Crate choices

Checked on crates.io in November 2026.

| Crate | Version | Why |
|---|---|---|
| `wreq` + `wreq-util` | 0.16.3 / 0.2.1 | The maintained fork of reqwest with BoringSSL and browser TLS/HTTP2 profiles (Firefox up to 151, Chrome up to 149). `rquest`, its former name, is yanked from crates.io. The 6.0.0-rc line on crates.io is older than 0.16. Needs Rust 1.98 or newer and cmake, perl and libclang to build. |
| `chromiumoxide` | 0.9.1 | Maintained CDP client on tokio. Optional (`chromium` feature of the binary, on by default). |
| `axum` | 0.8 | HTTP server. |
| `tokio` | 1 | Runtime, process and signal handling. |
| `serde`, `serde_json` | 1 | Wire types. |
| `tracing`, `tracing-subscriber` | 0.1 / 0.3 | Logs. |
| `thiserror`, `anyhow` | 2 / 1 | Errors. |

No Rust Playwright client is used; see the sidecar section.

## Build and test

`wreq` builds BoringSSL. On Debian or Ubuntu:

```sh
sudo apt-get install build-essential cmake perl pkg-config libclang-dev
cargo test --workspace
cargo run -p rustsolverr        # needs: pip install camoufox && python -m camoufox fetch
```

Without those tools (for example on Windows), every crate except the binary
still builds and tests, because only the binary enables the real HTTP client:

```sh
cargo test -p rustsolverr-common -p rustsolverr-config -p rustsolverr-challenge -p rustsolverr-cache -p rustsolverr-fetch -p rustsolverr-browser -p rustsolverr-solver -p rustsolverr-api
```

The whole workspace can be tested in the toolchain stage of the Dockerfile:

```sh
docker build --target toolchain -t rustsolverr-toolchain .
docker run --rm -v "$PWD:/src" -v rustsolverr-target:/src/target rustsolverr-toolchain cargo test --workspace
```

Tests:

- Unit tests: API (de)serialization with the request shapes of Prowlarr and
  FlareSolverrSharp, cache expiry, challenge detection, and the tier
  fall-through logic with a mocked browser and a mocked site.
- `crates/server/tests/integration.rs`: the real API and HTTP client against a
  local site that serves a JavaScript challenge which sets a cookie. One test
  uses a simulated browser. A second, `#[ignore]`d test uses the real Camoufox
  (`cargo test -p rustsolverr -- --ignored`).
- `scripts/smoke.py`: the same scenario against a running instance, for the
  Docker image: `docker exec rustsolverr python3 /usr/local/share/rustsolverr/smoke.py`.

## Limitations

- **Success against real protection depends on Camoufox**, on the exit IP and
  on the site. rustsolverr adds nothing to it.
- **A clearance may not transfer to the client.** Prowlarr and Jackett take the
  cookies and the user-agent and repeat the request with their own HTTP stack.
  If a site also binds the clearance to the TLS fingerprint, that repeat is
  challenged again. FlareSolverr has the same limit.
- **Empty cookies.** Both clients treat a solution without cookies as an error.
  A page that needs no challenge and sets no cookie gives that result, as with
  FlareSolverr.
- **Interactive challenges.** For a Turnstile checkbox the sidecar clicks the
  widget (best effort). Captchas that need a human are not solved; there is no
  captcha-service integration.
- **Proxy.** Camoufox does not get a timezone or locale that matches the proxy
  exit (Camoufox's `geoip` option is not used). Proxy credentials are passed
  to the browser context and to the HTTP client; SOCKS proxies with
  credentials are untested.
- **Sessions** keep cookies only (see above).
- **Fingerprint.** Camoufox generates one fingerprint per browser start. It
  changes when the browser restarts after an idle stop. Cached clearances keep
  the user-agent they were earned with, so they stay valid.
- **TLS profile match is by browser version.** The replay uses the `wreq`
  profile closest to the browser's Firefox version (the newest one not above
  it). It is not byte-identical to Camoufox's handshake. Camoufox 0.5.9
  ships Firefox 158, while `wreq-util` 0.2.1 ends at Firefox 151, so the
  replay sends the Firefox 158 user-agent with the Firefox 151 TLS profile.
- **The cache key is the exact host name**, as in Prowlarr and FlareSolverrSharp.
  `www.example.com` and `example.com` are solved separately.
- **Windows:** child processes of a crashed sidecar are killed with `taskkill`;
  the process-group guarantees are for Linux, the deployment target.
- No request-body types other than `application/x-www-form-urlencoded` for
  `request.post`, as in FlareSolverr.
