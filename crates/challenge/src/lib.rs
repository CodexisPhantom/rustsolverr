//! Challenge page detection.
//!
//! The title and selector lists are the ones FlareSolverr uses
//! (`flaresolverr_service.py`). The browser backends receive them and test
//! them against the live DOM. [`detect`] classifies a plain HTTP response,
//! where no DOM exists, from status, headers and page markers.

use rustsolverr_common::Page;

pub const ACCESS_DENIED_TITLES: &[&str] = &["Access denied", "Attention Required! | Cloudflare"];
pub const ACCESS_DENIED_SELECTORS: &[&str] = &[
    "div.cf-error-title span.cf-code-label span",
    "#cf-error-details div.cf-error-overview h1",
];
pub const CHALLENGE_TITLES: &[&str] = &["Just a moment...", "DDoS-Guard"];
pub const CHALLENGE_SELECTORS: &[&str] = &[
    "#cf-challenge-running",
    ".ray_id",
    ".attack-box",
    "#cf-please-wait",
    "#challenge-spinner",
    "#trk_jschal_js",
    "#turnstile-wrapper",
    ".lds-ring",
    "td.info #js_info",
    "div.vc div.text-box h2",
];

/// Body markers of a challenge interstitial. Only trusted together with a
/// challenge status code: normal Cloudflare pages also load scripts from
/// `/cdn-cgi/challenge-platform/`.
const CHALLENGE_BODY_MARKERS: &[&str] = &[
    "_cf_chl_opt",
    "/cdn-cgi/challenge-platform/",
    "cf-challenge-running",
    "challenge-spinner",
    "cf-please-wait",
    "trk_jschal_js",
    "turnstile-wrapper",
    "challenge-error-text",
];
const ACCESS_DENIED_BODY_MARKERS: &[&str] = &["cf-error-details", "cf-error-title"];
const CHALLENGE_STATUSES: &[u16] = &[403, 429, 503];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A normal page.
    Clear,
    /// A challenge that a browser can solve.
    Challenge,
    /// The site refused the request.
    Blocked,
}

/// Text of the first `<title>` element.
pub fn title(body: &str) -> Option<&str> {
    // ASCII lower-casing keeps byte offsets equal to those of `body`.
    let lower = body.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    Some(body[start..end].trim())
}

/// Classifies a plain HTTP response.
pub fn detect(page: &Page) -> Verdict {
    let header = |name: &str| page.headers.get(name).map(|v| v.to_ascii_lowercase());
    let title = title(&page.body).unwrap_or("");
    let challenge_status = CHALLENGE_STATUSES.contains(&page.status);

    if header("cf-mitigated").as_deref() == Some("challenge")
        || CHALLENGE_TITLES
            .iter()
            .any(|t| t.eq_ignore_ascii_case(title))
        || (challenge_status && CHALLENGE_BODY_MARKERS.iter().any(|m| page.body.contains(m)))
    {
        return Verdict::Challenge;
    }

    let cloudflare = header("server").is_some_and(|s| s.contains("cloudflare"))
        || ACCESS_DENIED_BODY_MARKERS
            .iter()
            .any(|m| page.body.contains(m));
    if cloudflare && ACCESS_DENIED_TITLES.iter().any(|t| title.starts_with(t)) {
        return Verdict::Blocked;
    }
    Verdict::Clear
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(status: u16, headers: &[(&str, &str)], body: &str) -> Page {
        Page {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.into(),
            ..Page::default()
        }
    }

    #[test]
    fn extracts_title() {
        assert_eq!(title("<html><TITLE lang=en> Hi </TITLE>"), Some("Hi"));
        assert_eq!(title("<p>no title</p>"), None);
        assert_eq!(title("<title>unterminated"), None);
    }

    #[test]
    fn normal_page_is_clear() {
        let p = page(200, &[("server", "cloudflare")], "<title>Home</title>ok");
        assert_eq!(detect(&p), Verdict::Clear);
    }

    #[test]
    fn cloudflare_title_is_challenge() {
        let p = page(403, &[], "<html><title>Just a moment...</title></html>");
        assert_eq!(detect(&p), Verdict::Challenge);
        let p = page(200, &[], "<title>just a MOMENT...</title>");
        assert_eq!(detect(&p), Verdict::Challenge);
    }

    #[test]
    fn mitigated_header_is_challenge() {
        let p = page(403, &[("cf-mitigated", "challenge")], "");
        assert_eq!(detect(&p), Verdict::Challenge);
    }

    #[test]
    fn ddos_guard_is_challenge() {
        let p = page(
            403,
            &[("server", "ddos-guard")],
            "<title>DDoS-Guard</title>",
        );
        assert_eq!(detect(&p), Verdict::Challenge);
    }

    #[test]
    fn body_marker_needs_challenge_status() {
        let body = "<title>x</title><script>window._cf_chl_opt={}</script>";
        assert_eq!(detect(&page(503, &[], body)), Verdict::Challenge);
        assert_eq!(detect(&page(200, &[], body)), Verdict::Clear);
    }

    #[test]
    fn plain_error_is_clear() {
        let p = page(503, &[], "<title>Service Unavailable</title>");
        assert_eq!(detect(&p), Verdict::Clear);
        let p = page(403, &[], "<title>Access denied</title>");
        assert_eq!(detect(&p), Verdict::Clear, "not a Cloudflare page");
    }

    #[test]
    fn access_denied_is_blocked() {
        let p = page(
            403,
            &[("server", "cloudflare")],
            "<title>Access denied | example.com used Cloudflare to restrict access</title>",
        );
        assert_eq!(detect(&p), Verdict::Blocked);
        let p = page(
            403,
            &[],
            "<title>Attention Required! | Cloudflare</title><div id=\"cf-error-details\">",
        );
        assert_eq!(detect(&p), Verdict::Blocked);
    }
}
