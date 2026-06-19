//! DuckDuckGo scraping backend (best-effort, keyless).
//!
//! Scrapes the `lite.duckduckgo.com` and `html.duckduckgo.com` HTML endpoints.
//! These endpoints aggressively challenge automated clients with CAPTCHAs —
//! often returning HTTP 200/202 with a "select all squares containing a duck"
//! page and zero result links. When that happens we surface an honest,
//! actionable error instead of a misleading "No results found". This backend is
//! retained as an opt-in fallback for users who want keyless search and have a
//! clean egress IP, but it is no longer the default.

use super::{format_results, SearchProvider, SearchResult, MOZILLA_UA};
use async_trait::async_trait;

pub(crate) struct DdgProvider;

/// One backend attempt. `Ok` means the HTTP layer succeeded (2xx); an empty
/// `results` vec with a 2xx status is the tell-tale signature of a
/// rate-limited / blocked DuckDuckGo CAPTCHA page.
#[derive(Debug)]
struct SearchAttempt {
    source: &'static str,
    status: u16,
    results: Vec<SearchResult>,
    body_snippet: String,
}

#[async_trait]
impl SearchProvider for DdgProvider {
    fn name(&self) -> &'static str {
        "DuckDuckGo"
    }

    async fn search(&self, client: &reqwest::Client, query: &str) -> Result<String, String> {
        let lite = search_ddg_lite(client, query).await;
        if let Ok(a) = &lite {
            if !a.results.is_empty() {
                return Ok(format_results(query, a.source, a.results.clone()));
            }
        }
        let html = search_ddg_html(client, query).await;
        if let Ok(a) = &html {
            if !a.results.is_empty() {
                return Ok(format_results(query, a.source, a.results.clone()));
            }
        }
        Err(compose_ddg_failure(query, &lite, &html))
    }
}

/// Headers that make a reqwest request resemble a real browser performing a
/// top-level navigation. Reduces (does NOT eliminate) DuckDuckGo's
/// bot-challenge rate by matching the browser fingerprint. Cannot defeat
/// challenges driven by IP reputation.
fn browser_headers(origin: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut h = HeaderMap::with_capacity(9);
    let put = |h: &mut HeaderMap, name: &'static str, val: &'static str| {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(val),
        ) {
            h.insert(n, v);
        }
    };
    put(
        &mut h,
        "accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
    );
    put(&mut h, "accept-language", "en-US,en;q=0.9");
    put(&mut h, "sec-fetch-dest", "document");
    put(&mut h, "sec-fetch-mode", "navigate");
    put(&mut h, "sec-fetch-site", "none");
    put(&mut h, "sec-fetch-user", "?1");
    put(&mut h, "upgrade-insecure-requests", "1");
    if let Ok(v) = HeaderValue::from_str(&format!("{origin}/")) {
        h.insert(reqwest::header::REFERER, v);
    }
    if let Ok(v) = HeaderValue::from_str(origin) {
        h.insert(reqwest::header::ORIGIN, v);
    }
    h
}

/// A short, whitespace-collapsed excerpt of a response body for diagnostics.
fn body_snippet(html: &str) -> String {
    crate::tools::html_to_text(html).chars().take(300).collect()
}

/// Parse DuckDuckGo HTML results. Tolerant to markup variations.
fn parse_ddg_results(html: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    for piece in html.split("result__a") {
        if results.len() >= 10 {
            break;
        }
        let Some(href_start) = piece.find("href=\"") else {
            continue;
        };
        let rest = &piece[href_start + 6..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let raw_url = &rest[..end];
        let url = decode_ddg_redirect(raw_url);
        if url.is_empty() || !url.starts_with("http") {
            continue;
        }
        let title_rest = &rest[end..];
        let title = extract_until(title_start_after(title_rest), '<');
        let snippet = extract_snippet(piece);
        if title.trim().is_empty() {
            continue;
        }
        results.push(SearchResult {
            title: decode_entities(&title),
            url,
            snippet: decode_entities(&snippet),
        });
    }
    results
}

/// Parse DuckDuckGo Lite results.
fn parse_ddg_lite_results(html: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    for piece in html.split("result-link") {
        if results.len() >= 10 {
            break;
        }
        let Some(href_start) = piece.find("href=\"") else {
            continue;
        };
        let rest = &piece[href_start + 6..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let raw_url = &rest[..end];
        let url = decode_ddg_redirect(raw_url);
        if url.is_empty() || !url.starts_with("http") {
            continue;
        }
        let title = extract_until(title_start_after(&rest[end..]), '<');
        if title.trim().is_empty() {
            continue;
        }
        let snippet = extract_lite_snippet(piece);
        results.push(SearchResult {
            title: decode_entities(&title),
            url,
            snippet: decode_entities(&snippet),
        });
    }
    results
}

fn title_start_after(rest: &str) -> &str {
    rest.find('>').map(|idx| &rest[idx + 1..]).unwrap_or("")
}

fn extract_until(text: &str, terminator: char) -> String {
    text.find(terminator)
        .map(|idx| text[..idx].to_string())
        .unwrap_or_else(|| text.to_string())
}

fn extract_snippet(piece: &str) -> String {
    if let Some(idx) = piece.find("result__snippet") {
        let rest = &piece[idx..];
        if let Some(gt) = rest.find('>') {
            return extract_until(&rest[gt + 1..], '<');
        }
    }
    String::new()
}

fn extract_lite_snippet(piece: &str) -> String {
    if let Some(idx) = piece.find("result-snippet") {
        let rest = &piece[idx..];
        if let Some(gt) = rest.find('>') {
            return extract_until(&rest[gt + 1..], '<');
        }
    }
    String::new()
}

fn decode_ddg_redirect(raw: &str) -> String {
    if let Some(stripped) = raw.strip_prefix("//duckduckgo.com/l/?uddg=") {
        let encoded = stripped.split('&').next().unwrap_or("");
        if let Ok(decoded) = url_decode(encoded) {
            return decoded;
        }
    }
    raw.to_string()
}

fn url_decode(value: &str) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|e| e.to_string())?;
                let byte = u8::from_str_radix(hex, 16).map_err(|e| e.to_string())?;
                out.push(byte as char);
                i += 2;
            }
            c => out.push(c as char),
        }
        i += 1;
    }
    Ok(out)
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Build an honest, actionable error when DuckDuckGo returned no parseable
/// results. Pure function so it can be unit-tested without network access.
fn compose_ddg_failure(
    query: &str,
    lite: &Result<SearchAttempt, String>,
    html: &Result<SearchAttempt, String>,
) -> String {
    use std::fmt::Write;
    let mut msg = String::new();
    let _ = writeln!(
        msg,
        "Web search failed for {:?}: DuckDuckGo returned no parseable results.",
        query
    );
    let _ = writeln!(
        msg,
        "This almost always means DuckDuckGo rate-limited/blocked the request, \
         or a firewall/proxy returned a non-results page (both still answer HTTP \
         200, which previously masqueraded as \"No results found\")."
    );
    let _ = writeln!(
        msg,
        "Fix it in ~/.config/neenee/config.toml under [websearch]:"
    );
    let _ = writeln!(
        msg,
        "  - default reliable backend: provider = \"exa\" (anonymous, no key)"
    );
    let _ = writeln!(
        msg,
        "  - self-hosted/keyless: provider = \"searxng\", searxng_url = \"http://localhost:8080/search\""
    );
    let _ = writeln!(
        msg,
        "  - hosted API: provider = \"tavily\", tavily_api_key = \"tvly-...\""
    );
    let _ = writeln!(
        msg,
        "  - or route around the block: proxy = \"socks5h://127.0.0.1:1080\""
    );
    let _ = writeln!(msg, "\nDiagnostics:");
    for (label, attempt) in [("lite", lite), ("html", html)] {
        match attempt {
            Ok(a) => {
                let _ = writeln!(
                    msg,
                    "  {}: HTTP {}, {} result(s), body excerpt: {:?}",
                    label,
                    a.status,
                    a.results.len(),
                    a.body_snippet
                );
            }
            Err(e) => {
                let _ = writeln!(msg, "  {}: error: {}", label, e);
            }
        }
    }
    msg
}

async fn search_ddg_lite(client: &reqwest::Client, query: &str) -> Result<SearchAttempt, String> {
    let endpoint = "https://lite.duckduckgo.com/lite/";
    let response = client
        .post(endpoint)
        .header(reqwest::header::USER_AGENT, MOZILLA_UA)
        .headers(browser_headers("https://lite.duckduckgo.com"))
        .form(&[("q", query), ("kl", "us-en")])
        .send()
        .await
        .map_err(|e| format!("DuckDuckGo Lite request failed: {}", e))?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        return Err(format!("DuckDuckGo Lite returned HTTP {}", status));
    }
    let html = response
        .text()
        .await
        .map_err(|e| format!("Failed to read DuckDuckGo Lite response: {}", e))?;
    let results = parse_ddg_lite_results(&html);
    let snippet = body_snippet(&html);
    Ok(SearchAttempt {
        source: "DuckDuckGo Lite",
        status,
        results,
        body_snippet: snippet,
    })
}

async fn search_ddg_html(client: &reqwest::Client, query: &str) -> Result<SearchAttempt, String> {
    let endpoint = "https://html.duckduckgo.com/html/";
    let response = client
        .post(endpoint)
        .header(reqwest::header::USER_AGENT, MOZILLA_UA)
        .headers(browser_headers("https://html.duckduckgo.com"))
        .form(&[("q", query), ("kl", "us-en")])
        .send()
        .await
        .map_err(|e| format!("DuckDuckGo HTML request failed: {}", e))?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        return Err(format!("DuckDuckGo HTML returned HTTP {}", status));
    }
    let html = response
        .text()
        .await
        .map_err(|e| format!("Failed to read DuckDuckGo HTML response: {}", e))?;
    let results = parse_ddg_results(&html);
    let snippet = body_snippet(&html);
    Ok(SearchAttempt {
        source: "DuckDuckGo HTML",
        status,
        results,
        body_snippet: snippet,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ddg_results_extracts_title_url_and_snippet() {
        let html = r#"
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fnews">AI News</a>
            <a class="result__snippet">Latest artificial intelligence headlines.</a>
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org">Research</a>
            <a class="result__snippet">Research papers on AI.</a>
        "#;
        let results = parse_ddg_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "AI News");
        assert_eq!(results[0].url, "https://example.com/news");
        assert_eq!(
            results[0].snippet,
            "Latest artificial intelligence headlines."
        );
    }

    #[test]
    fn parse_ddg_lite_results_extracts_title_url_and_snippet() {
        let html = r#"
            <a class="result-link" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Flite.example.com%2Fone">Lite Result One</a>
            <td class="result-snippet">A snippet from the lite endpoint.</td>
            <a class="result-link" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Flite.example.com%2Ftwo">Lite Result Two</a>
            <td class="result-snippet">Another lite snippet.</td>
        "#;
        let results = parse_ddg_lite_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Lite Result One");
        assert_eq!(results[0].url, "https://lite.example.com/one");
        assert_eq!(results[0].snippet, "A snippet from the lite endpoint.");
    }

    #[test]
    fn parse_ddg_results_skips_invalid_redirects() {
        let html = r#"
            <a class="result__a" href="/not-a-redirect">Bad Link</a>
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fvalid.example.com">Good Link</a>
        "#;
        let results = parse_ddg_results(html);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Good Link");
    }

    #[test]
    fn compose_ddg_failure_reports_block_not_no_results() {
        let lite = Ok(SearchAttempt {
            source: "DuckDuckGo Lite",
            status: 200,
            results: Vec::new(),
            body_snippet: "If this error persists, please let us know.".to_string(),
        });
        let html = Ok(SearchAttempt {
            source: "DuckDuckGo HTML",
            status: 200,
            results: Vec::new(),
            body_snippet: "".to_string(),
        });
        let msg = compose_ddg_failure("fable 5", &lite, &html);
        assert!(
            !msg.contains("tried DuckDuckGo Lite and HTML endpoints"),
            "must not repeat the old misleading phrasing: {msg}"
        );
        assert!(msg.contains("failed"), "should signal failure: {msg}");
        assert!(msg.contains("blocked"), "should mention blocking: {msg}");
        assert!(msg.contains("exa"), "should suggest exa: {msg}");
        assert!(msg.contains("HTTP 200"), "should include status: {msg}");
    }

    #[test]
    fn compose_ddg_failure_includes_transport_errors() {
        let lite: Result<SearchAttempt, String> =
            Err("DuckDuckGo Lite request failed: dns error".to_string());
        let html: Result<SearchAttempt, String> =
            Err("DuckDuckGo HTML returned HTTP 429".to_string());
        let msg = compose_ddg_failure("rust async", &lite, &html);
        assert!(msg.contains("dns error"), "{msg}");
        assert!(msg.contains("429"), "{msg}");
    }
}
