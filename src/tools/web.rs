//! Web search + fetch tools.
//!
//! - `WebSearchTool` — DuckDuckGo HTML search, no API key. Returns a
//!   list of `{title, url, snippet}` results.
//! - `WebFetchTool` — fetches a URL and returns the stripped text
//!   content. Useful for fetching a specific page after a search.
//!
//! Both are read-only. They make HTTPS requests through the shared
//! `reqwest::Client`, which is created lazily and reused.
//!
//! HTML extraction is intentionally simple — we strip tags with a
//! regex. This isn't perfect (it leaves script/style bodies in,
//! decodes entities imperfectly) but it's good enough for
//! search-result pages and doesn't pull in a heavyweight HTML parser.

use super::{Tool, ToolContext, ToolOutput, truncate_tail};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::debug;

// Compile the regexes once. `regex` is a transitive dep via `reqwest`
// or — if it's not — we'd need to add it. Check first.
//
// Fallback: write a small manual tag-stripper if regex isn't available.

static TAG_RE: Lazy<Regex> = Lazy::new(|| {
    // Match <...> tags. Not perfect (won't handle attributes with `>` inside)
    // but fine for stripping search-result HTML.
    Regex::new(r"<[^>]+>").expect("tag regex compiles")
});

static WS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\s+").expect("whitespace regex compiles"));

// Shared HTTP client (connection-pooled). Created lazily on first use.
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("graph-centric-agent/0.1 (+https://github.com/wang546673478/Graph-Centric)")
        .build()
        .expect("reqwest client builds")
});

// ---------------------------------------------------------------------------
// WebSearchTool
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchInput {
    pub query: String,
    #[serde(default = "default_max_results")]
    pub max_results: usize,
}

fn default_max_results() -> usize {
    5
}

/// v2 spec §5.1: in-memory cache for web search results.
/// `HashMap<query, (timestamp, result)>` with TTL eviction.
#[derive(Debug, Default)]
pub struct WebSearchCache {
    entries: std::collections::HashMap<String, (std::time::Instant, String)>,
    ttl: Option<std::time::Duration>,
}

impl WebSearchCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_ttl(ttl: std::time::Duration) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            ttl: Some(ttl),
        }
    }

    /// Returns Some(cached) on hit and not expired; None on miss.
    pub fn get(&self, query: &str) -> Option<String> {
        let entry = self.entries.get(query)?;
        if let Some(ttl) = self.ttl {
            if entry.0.elapsed() > ttl {
                return None;
            }
        }
        Some(entry.1.clone())
    }

    pub fn put(&mut self, query: String, result: String) {
        self.entries
            .insert(query, (std::time::Instant::now(), result));
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

pub struct WebSearchTool {
    cache: std::sync::Mutex<WebSearchCache>,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self {
            cache: std::sync::Mutex::new(WebSearchCache::with_ttl(
                std::time::Duration::from_secs(3600), // spec default 1h
            )),
        }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the public web via DuckDuckGo HTML. Returns a list of \
         {title, url, snippet} for the top results. No API key needed. \
         Use when the model needs to find current information, links, \
         references, or to discover candidate pages to fetch next."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query (DuckDuckGo syntax; quotes for exact match)."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of results to return. Default 5, max 20.",
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn call(&self, input: serde_json::Value, _ctx: &ToolContext) -> super::Result<ToolOutput> {
        let parsed: WebSearchInput = serde_json::from_value(input)
            .map_err(|e| crate::error::HarnessError::domain(format!("web_search: bad input: {e}")))?;
        if parsed.query.trim().is_empty() {
            return Ok(ToolOutput::ok("(empty query — no results)", None));
        }
        // v2 spec §5.1: in-memory cache. Same query within TTL
        // (default 1h) returns the cached result without
        // hitting the network.
        if let Some(cached) = self.cache.lock().unwrap().get(&parsed.query) {
            debug!(query = %parsed.query, "web_search: cache hit");
            return Ok(ToolOutput {
                content: format!("[cached]\n{cached}"),
                structured: None,
                truncated: false,
                exit_code: Some(0),
                interrupted: false,
                duration_ms: 0,
            });
        }
        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            urlencoding_minimal(&parsed.query)
        );
        debug!(query = %parsed.query, "web_search: GET {url}");
        let resp = HTTP
            .get(&url)
            .send()
            .await
            .map_err(|e| crate::error::HarnessError::domain(format!("web_search: request failed: {e}")))?;
        let body = resp
            .text()
            .await
            .map_err(|e| crate::error::HarnessError::domain(format!("web_search: read body: {e}")))?;
        let results = parse_duckduckgo(&body, parsed.max_results);
        let formatted = if results.is_empty() {
            "(no results)".to_string()
        } else {
            let mut s = String::new();
            for (i, r) in results.iter().enumerate() {
                s.push_str(&format!(
                    "{}. {}\n   {}\n   {}\n",
                    i + 1,
                    r.title,
                    r.url,
                    r.snippet,
                ));
            }
            s
        };
        // Store in cache for next time.
        self.cache.lock().unwrap().put(parsed.query.clone(), formatted.clone());
        let structured = serde_json::to_value(&results).ok();
        Ok(ToolOutput {
            content: formatted,
            structured,
            truncated: false,
            exit_code: Some(0),
            interrupted: false,
            duration_ms: 0,
        })
    }
}

fn parse_duckduckgo(html: &str, max: usize) -> Vec<SearchResult> {
    // DuckDuckGo HTML uses <a class="result__a" href="...">TITLE</a>
    // and <a class="result__snippet">SNIPPET</a>. Extract via regex.
    static TITLE_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?s)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
            .expect("title regex")
    });
    static SNIP_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?s)<a[^>]*class="result__snippet"[^>]*>(.*?)</a>"#)
            .expect("snippet regex")
    });

    let titles: Vec<(String, String)> = TITLE_RE
        .captures_iter(html)
        .filter_map(|c| {
            let url = c.get(1)?.as_str().to_string();
            let raw_title = c.get(2)?.as_str();
            Some((url, strip_tags(raw_title)))
        })
        .collect();
    let snippets: Vec<String> = SNIP_RE
        .captures_iter(html)
        .filter_map(|c| Some(strip_tags(c.get(1)?.as_str())))
        .collect();

    titles
        .into_iter()
        .take(max)
        .enumerate()
        .map(|(i, (url, title))| SearchResult {
            title,
            url,
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

fn strip_tags(s: &str) -> String {
    // Strip <script>...</script> and <style>...</style> blocks first so
    // their bodies don't leak as text. regex 1.x doesn't support
    // backreferences, so we do it as two explicit non-greedy matches.
    let lower = s.to_ascii_lowercase();
    let mut cleaned = String::with_capacity(s.len());
    let mut cursor = 0;
    while let Some(start) = lower[cursor..].find('<') {
        let abs = cursor + start;
        cleaned.push_str(&s[cursor..abs]);
        // Find which tag this is.
        if lower[abs..].starts_with("<script") {
            // Find the closing </script>
            if let Some(end) = lower[abs..].find("</script>") {
                cursor = abs + end + "</script>".len();
                continue;
            } else {
                // Unterminated; drop the rest.
                break;
            }
        } else if lower[abs..].starts_with("<style") {
            if let Some(end) = lower[abs..].find("</style>") {
                cursor = abs + end + "</style>".len();
                continue;
            } else {
                break;
            }
        } else {
            // Not a script/style — emit the `<` and continue scanning
            // from the next char so the generic tag-stripper handles
            // it next pass.
            cleaned.push('<');
            cursor = abs + 1;
        }
    }
    cleaned.push_str(&s[cursor..]);
    let no_tags = TAG_RE.replace_all(&cleaned, " ");
    let collapsed = WS_RE.replace_all(&no_tags, " ");
    collapsed.trim().to_string()
}

fn urlencoding_minimal(s: &str) -> String {
    // Don't pull in a full URL-encoding crate. Encode the chars that
    // matter for query strings: spaces, &, =, ?, #, %, +.
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b' ' => out.push('+'),
            b'&' | b'=' | b'?' | b'#' | b'%' | b'+' => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
            _ => out.push(b as char),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// WebFetchTool
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchInput {
    pub url: String,
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
}

fn default_max_chars() -> usize {
    5000
}

pub struct WebFetchTool;

impl WebFetchTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a single URL and return the page text (HTML stripped, \
         whitespace collapsed, tail-truncated to the cap). Use after \
         `web_search` to dig into a specific result. Read-only."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL to fetch (http or https)."
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Maximum characters of the returned text. Default 5000.",
                    "minimum": 100,
                    "maximum": 50000
                }
            },
            "required": ["url"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn call(&self, input: serde_json::Value, _ctx: &ToolContext) -> super::Result<ToolOutput> {
        let parsed: WebFetchInput = serde_json::from_value(input)
            .map_err(|e| crate::error::HarnessError::domain(format!("web_fetch: bad input: {e}")))?;
        // Light scheme check — don't bother with full URL parsing; the
        // request will fail loudly if the URL is malformed anyway.
        if !(parsed.url.starts_with("http://") || parsed.url.starts_with("https://")) {
            return Err(crate::error::HarnessError::domain(format!(
                "web_fetch: url must be http(s), got: {}",
                parsed.url
            )));
        }
        let max = parsed.max_chars.clamp(100, 50_000);
        debug!(url = %parsed.url, max, "web_fetch: GET");
        let resp = HTTP
            .get(&parsed.url)
            .send()
            .await
            .map_err(|e| crate::error::HarnessError::domain(format!("web_fetch: request failed: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| crate::error::HarnessError::domain(format!("web_fetch: read body: {e}")))?;
        let text = strip_tags(&body);
        let (content, truncated) = truncate_tail(&text, max);
        let structured = serde_json::json!({
            "url": parsed.url,
            "status": status.as_u16(),
            "char_count": text.chars().count(),
            "truncated": truncated,
        });
        Ok(ToolOutput {
            content,
            structured: Some(structured),
            truncated,
            exit_code: Some(0),
            interrupted: false,
            duration_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_tags_removes_basic_html() {
        let html = "<html><body><h1>Title</h1><p>Hello <b>world</b>!</p></body></html>";
        let out = strip_tags(html);
        assert!(out.contains("Title"));
        assert!(out.contains("Hello"));
        assert!(out.contains("world"));
        assert!(!out.contains("<"));
    }

    #[test]
    fn strip_tags_drops_script_and_style_bodies() {
        let html = "<div>visible<script>alert('x')</script>also <style>p{}</style>after</div>";
        let out = strip_tags(html);
        assert!(out.contains("visible"));
        assert!(out.contains("also"));
        assert!(out.contains("after"));
        assert!(!out.contains("alert"));
        assert!(!out.contains("p{}"));
    }

    #[test]
    fn urlencoding_minimal_encodes_specials() {
        assert_eq!(urlencoding_minimal("a b"), "a+b");
        assert_eq!(urlencoding_minimal("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencoding_minimal("plain"), "plain");
    }

    #[test]
    fn parse_duckduckgo_extracts_results() {
        let html = r#"<html><body>
            <a rel="nofollow" class="result__a" href="https://example.com/a">First Title</a>
            <a class="result__snippet">first snippet</a>
            <a rel="nofollow" class="result__a" href="https://example.com/b">Second <b>Title</b></a>
            <a class="result__snippet">second snippet</a>
        </body></html>"#;
        let r = parse_duckduckgo(html, 5);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].url, "https://example.com/a");
        assert_eq!(r[0].title, "First Title");
        assert_eq!(r[0].snippet, "first snippet");
        assert_eq!(r[1].url, "https://example.com/b");
        assert!(r[1].title.contains("Second"));
        assert!(r[1].title.contains("Title"));
    }

    #[test]
    fn web_search_input_default_max_results() {
        let p: WebSearchInput = serde_json::from_value(serde_json::json!({"query": "x"})).unwrap();
        assert_eq!(p.max_results, 5);
    }

    #[test]
    fn web_fetch_input_default_max_chars() {
        let p: WebFetchInput = serde_json::from_value(serde_json::json!({"url": "https://x"})).unwrap();
        assert_eq!(p.max_chars, 5000);
    }
}
