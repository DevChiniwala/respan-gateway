//! Firecrawl (<https://firecrawl.dev>): pages as markdown, asynchronous
//! crawls, site maps and web search through `https://api.firecrawl.dev/v2`,
//! with a bearer key. A crawl returns its id; `get_crawl` reads its status
//! and one page of results without following authenticated URLs elsewhere.

use async_trait::async_trait;
use respan_gateway_tool_vendor_kit::args::{
    object, optional_bool, optional_enum, optional_int, optional_str, optional_str_list,
    required_str,
};
use respan_gateway_tool_vendor_kit::reqwest::Url;
use respan_gateway_tool_vendor_kit::{
    Api, KeyIn, ToolError, ToolOutput, ToolSpec, ToolVendor, Tools,
};
use serde_json::{Map, Value, json};

/// Firecrawl.
pub const VENDOR: ToolVendor = ToolVendor {
    id: "firecrawl",
    key_env: &["FIRECRAWL_API_KEY"],
    key_in: KeyIn::Bearer,
    base_url: "https://api.firecrawl.dev/v2",
    tools: &FirecrawlTools,
};

/// Firecrawl's `scrape`, `crawl`, `get_crawl`, `map_site` and `search` tools.
#[derive(Debug, Clone, Copy)]
pub struct FirecrawlTools;

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "scrape",
        description: "Scrape one web page with Firecrawl and get its markdown and metadata, \
            including its source URL and HTTP status. Billed to the Firecrawl account.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "url": {"type": "string", "description": "The HTTP or HTTPS page URL to scrape."},
    "only_main_content": {"type": "boolean", "default": true, "description": "Remove navigation, headers and footers."},
    "wait_for": {"type": "integer", "minimum": 0, "maximum": 60000, "description": "Additional milliseconds to wait for JavaScript to render."}
  },
  "required": ["url"]
}"#,
    },
    ToolSpec {
        name: "crawl",
        description: "Start a Firecrawl crawl that follows a site's links and scrapes its pages \
            as markdown. Returns a job id immediately: use `get_crawl` to read its status and \
            pages. Billed to the Firecrawl account for pages scraped; limited to 10 pages \
            unless `limit` says otherwise. Prefer `map_site` to only discover URLs.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "url": {"type": "string", "description": "The HTTP or HTTPS URL to start from."},
    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 10, "description": "At most this many pages to scrape."},
    "max_depth": {"type": "integer", "minimum": 0, "maximum": 100, "description": "Maximum link-discovery depth from the root and sitemap pages."},
    "include_paths": {"type": "array", "items": {"type": "string"}, "description": "Only paths matching one of these regular expressions."},
    "exclude_paths": {"type": "array", "items": {"type": "string"}, "description": "Exclude paths matching these regular expressions."},
    "allow_external_links": {"type": "boolean", "default": false, "description": "Also follow links to other domains."}
  },
  "required": ["url"]
}"#,
    },
    ToolSpec {
        name: "get_crawl",
        description: "Read a Firecrawl crawl's status and one page of scraped documents. \
            A job can still be scraping: poll again until completed. When `next_skip` is \
            present, pass it as `skip` to read more pages of the same job. `creditsUsed` is \
            the job's cumulative cost, not an additional charge for reading it.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "crawl_id": {"type": "string", "description": "The id returned by `crawl`."},
    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 10, "description": "At most this many documents to read at once."},
    "skip": {"type": "integer", "minimum": 0, "maximum": 4294967295, "default": 0, "description": "Documents already read, from the previous result's next_skip."}
  },
  "required": ["crawl_id"]
}"#,
    },
    ToolSpec {
        name: "map_site",
        description: "Discover a website's URLs with Firecrawl, without scraping their content. \
            Returns URLs with titles and descriptions when available. Billed to the \
            Firecrawl account.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "url": {"type": "string", "description": "The HTTP or HTTPS website URL to map."},
    "search": {"type": "string", "description": "Rank URLs by relevance to this search."},
    "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 100, "description": "At most this many URLs to return."},
    "include_subdomains": {"type": "boolean", "default": true, "description": "Also discover subdomains."},
    "sitemap": {"type": "string", "enum": ["include", "skip", "only"], "default": "include", "description": "Whether to use the site's sitemap as well as other URL discovery."}
  },
  "required": ["url"]
}"#,
    },
    ToolSpec {
        name: "search",
        description: "Search the web with Firecrawl and get ranked pages with URLs, titles and \
            descriptions. Set `scrape` to also get each page's markdown. Billed to the \
            Firecrawl account; scraping search results costs additional credits.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "query": {"type": "string", "minLength": 1, "maxLength": 500, "description": "The web search query."},
    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 5, "description": "At most this many search results."},
    "scrape": {"type": "boolean", "default": false, "description": "Also scrape each result as markdown."}
  },
  "required": ["query"]
}"#,
    },
];

#[async_trait]
impl Tools for FirecrawlTools {
    fn list(&self) -> &'static [ToolSpec] {
        TOOLS
    }

    async fn call(&self, api: &Api, name: &str, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args = object(&arguments)?;
        let response = match name {
            "scrape" => api.post_json("scrape", &scrape_body(args)?).await?,
            "crawl" => api.post_json("crawl", &crawl_body(args)?).await?,
            "get_crawl" => {
                let id = required_str(args, "crawl_id")?;
                if id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return Err(ToolError::new("`crawl_id` must be a crawl job id"));
                }
                let limit = optional_int(args, "limit", 1, 100)?.unwrap_or(10);
                let skip = optional_int(args, "skip", 0, u32::MAX.into())?.unwrap_or(0);
                api.get_json(
                    &format!("crawl/{id}"),
                    &[("skip", skip.to_string()), ("limit", limit.to_string())],
                )
                .await?
            }
            "map_site" => api.post_json("map", &map_body(args)?).await?,
            "search" => api.post_json("search", &search_body(args)?).await?,
            _ => return Err(ToolError::new(format!("no tool named {name:?}"))),
        };
        render(name, response)
    }
}

fn url(args: &Map<String, Value>) -> Result<&str, ToolError> {
    let url = required_str(args, "url")?;
    let valid = Url::parse(url)
        .ok()
        .is_some_and(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some());
    match valid {
        true => Ok(url),
        false => Err(ToolError::new("`url` must be an HTTP or HTTPS URL")),
    }
}

fn scrape_body(args: &Map<String, Value>) -> Result<Value, ToolError> {
    let mut body = json!({"url": url(args)?, "formats": ["markdown"]});
    if let Some(value) = optional_bool(args, "only_main_content")? {
        body["onlyMainContent"] = value.into();
    }
    if let Some(value) = optional_int(args, "wait_for", 0, 60_000)? {
        body["waitFor"] = value.into();
    }
    Ok(body)
}

fn crawl_body(args: &Map<String, Value>) -> Result<Value, ToolError> {
    let mut body = json!({
        "url": url(args)?,
        "limit": optional_int(args, "limit", 1, 100)?.unwrap_or(10),
        "scrapeOptions": {"formats": ["markdown"]}
    });
    if let Some(value) = optional_int(args, "max_depth", 0, 100)? {
        body["maxDiscoveryDepth"] = value.into();
    }
    for (from, to) in [
        ("include_paths", "includePaths"),
        ("exclude_paths", "excludePaths"),
    ] {
        if let Some(value) = optional_str_list(args, from)? {
            body[to] = json!(value);
        }
    }
    if let Some(value) = optional_bool(args, "allow_external_links")? {
        body["allowExternalLinks"] = value.into();
    }
    Ok(body)
}

fn map_body(args: &Map<String, Value>) -> Result<Value, ToolError> {
    let mut body = json!({
        "url": url(args)?,
        "limit": optional_int(args, "limit", 1, 1000)?.unwrap_or(100)
    });
    if let Some(value) = optional_str(args, "search")? {
        body["search"] = value.into();
    }
    if let Some(value) = optional_bool(args, "include_subdomains")? {
        body["includeSubdomains"] = value.into();
    }
    if let Some(value) = optional_enum(args, "sitemap", &["include", "skip", "only"])? {
        body["sitemap"] = value.into();
    }
    Ok(body)
}

fn search_body(args: &Map<String, Value>) -> Result<Value, ToolError> {
    let query = required_str(args, "query")?;
    if query.chars().count() > 500 {
        return Err(ToolError::new("`query` must be at most 500 characters"));
    }
    let mut body = json!({
        "query": query,
        "limit": optional_int(args, "limit", 1, 100)?.unwrap_or(5),
        "sources": ["web"]
    });
    if optional_bool(args, "scrape")? == Some(true) {
        body["scrapeOptions"] = json!({"formats": ["markdown"]});
    }
    Ok(body)
}

fn render(name: &str, mut response: Value) -> Result<ToolOutput, ToolError> {
    // A crawl's status answer carries no `success`: only `false` is a refusal.
    if response.get("success").and_then(Value::as_bool) == Some(false) {
        let error = response.get("error").and_then(Value::as_str);
        return Err(ToolError::new(format!(
            "Firecrawl: {}",
            error.unwrap_or("the API answered without a successful result")
        )));
    }
    let valid = match name {
        "scrape" => response.get("data").is_some_and(Value::is_object),
        "search" => response.pointer("/data/web").is_some_and(Value::is_array),
        "crawl" => response
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty()),
        "map_site" => response.get("links").is_some_and(Value::is_array),
        "get_crawl" => {
            let status = response.get("status").and_then(Value::as_str);
            if let Some(status @ ("failed" | "cancelled")) = status {
                // Not the whole answer: its pages can run to megabytes.
                let count = |key: &str| response.get(key).and_then(Value::as_u64).unwrap_or(0);
                return Err(ToolError::new(format!(
                    "Firecrawl crawl {status} after {} of {} pages",
                    count("completed"),
                    count("total")
                )));
            }
            let has_status = status.is_some_and(|status| !status.is_empty());
            // Only reuse the offset; authenticated requests always use
            // `crawl/{id}` under the configured API base, never this URL.
            let next = response.get("next").and_then(Value::as_str);
            let skip = next
                .and_then(|next| Url::parse(next).ok())
                .and_then(|next| {
                    next.query_pairs()
                        .find(|(key, _)| key == "skip")
                        .and_then(|(_, value)| value.parse::<u32>().ok())
                });
            if let Some(skip) = skip {
                response["next_skip"] = skip.into();
            }
            has_status && response.get("data").is_some_and(Value::is_array)
        }
        _ => false,
    };
    if !valid {
        return Err(ToolError::new(
            "Firecrawl answered without the tool's result",
        ));
    }
    let credits = match name {
        "scrape" => response.pointer("/data/metadata/creditsUsed"),
        "search" => response.get("creditsUsed"),
        // A crawl's `creditsUsed` is cumulative for the entire job.
        // Reading it repeatedly must not charge that total repeatedly.
        _ => None,
    }
    .and_then(Value::as_f64)
    .filter(|credits| credits.is_finite() && *credits >= 0.0);
    let output = ToolOutput::from(response.to_string());
    Ok(match credits {
        Some(credits) => output.with_usage("credits", credits),
        None => output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_tool_vendor_kit::Content;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    fn text(output: &ToolOutput) -> Value {
        let Content::Text(text) = &output.content[0] else {
            panic!("text output")
        };
        serde_json::from_str(text).expect("JSON output")
    }

    #[test]
    fn crawls_have_an_explicit_page_limit_and_markdown() {
        let body = crawl_body(&args(json!({"url": "https://example.com"}))).expect("crawl");
        assert_eq!(body["limit"], 10);
        assert_eq!(body["scrapeOptions"], json!({"formats": ["markdown"]}));
        let body = crawl_body(&args(json!({
            "url": "https://example.com", "limit": 2, "max_depth": 0,
            "include_paths": ["/docs/.*"], "exclude_paths": ["/private/.*"],
            "allow_external_links": false
        })))
        .expect("crawl options");
        assert_eq!(body["maxDiscoveryDepth"], 0);
        assert_eq!(body["includePaths"], json!(["/docs/.*"]));
        assert_eq!(body["excludePaths"], json!(["/private/.*"]));
        assert_eq!(body["allowExternalLinks"], false);
    }

    #[test]
    fn invalid_arguments_are_refused_before_sending() {
        for input in [
            json!({}),
            json!({"url": "file:///etc/passwd"}),
            json!({"url": "https://example.com", "only_main_content": "true"}),
            json!({"url": "https://example.com", "wait_for": 60001}),
        ] {
            assert!(scrape_body(&args(input)).is_err());
        }
        for limit in [0, 101] {
            assert!(
                crawl_body(&args(json!({"url": "https://example.com", "limit": limit}))).is_err()
            );
            assert!(search_body(&args(json!({"query": "rust", "limit": limit}))).is_err());
        }
        assert!(
            crawl_body(&args(
                json!({"url": "https://example.com", "include_paths": [1]})
            ))
            .is_err()
        );
        assert!(
            map_body(&args(
                json!({"url": "https://example.com", "sitemap": "never"})
            ))
            .is_err()
        );
        assert!(search_body(&args(json!({"query": " "}))).is_err());
        assert!(search_body(&args(json!({"query": "x".repeat(501)}))).is_err());
        assert!(search_body(&args(json!({"query": "rust", "scrape": 1}))).is_err());
    }

    #[test]
    fn keeps_page_outcomes_warnings_and_reported_usage() {
        let page = json!({
            "success": true,
            "data": {"markdown": "Not found", "metadata": {
                "sourceURL": "https://example.com/missing", "statusCode": 404,
                "error": "Not found", "creditsUsed": 1
            }},
            "warning": "Partial content"
        });
        let output = render("scrape", page.clone()).expect("scrape");
        assert_eq!(text(&output), page);
        assert_eq!(output.usage.get("credits"), Some(&1.0));
        let output = render(
            "search",
            json!({
                "success": true, "data": {"web": []}, "creditsUsed": 2, "warning": "No results"
            }),
        )
        .expect("search");
        assert_eq!(output.usage.get("credits"), Some(&2.0));
        assert_eq!(text(&output)["warning"], "No results");
        for credits in [Value::Null, json!(-1)] {
            let output = render(
                "search",
                json!({
                    "success": true, "data": {"web": []}, "creditsUsed": credits
                }),
            )
            .expect("search");
            assert!(output.usage.is_empty());
        }
    }

    #[test]
    fn reads_crawl_pages_without_charging_the_job_total_again() {
        for status in ["scraping", "completed"] {
            let output = render("get_crawl", json!({
                "success": true, "status": status, "total": 2, "completed": 2,
                "creditsUsed": 2, "expiresAt": "2026-10-26T00:00:00Z",
                "next": "https://api.firecrawl.dev/v2/crawl/id?skip=1&limit=1",
                "data": [{"markdown": "Hello", "metadata": {"sourceURL": "https://example.com"}}]
            })).expect("crawl status");
            assert_eq!(text(&output)["next_skip"], 1);
            assert_eq!(text(&output)["status"], status);
            assert!(output.usage.is_empty());
        }
        for status in ["failed", "cancelled"] {
            let error = render(
                "get_crawl",
                json!({
                    "success": true, "status": status, "total": 3, "completed": 1,
                    "data": [{"markdown": "x".repeat(10_000)}]
                }),
            )
            .expect_err("crawl ended early");
            assert_eq!(
                error.0,
                format!("Firecrawl crawl {status} after 1 of 3 pages")
            );
        }
    }

    #[test]
    fn api_failures_and_missing_results_are_tool_errors() {
        let error = render(
            "crawl",
            json!({"success": false, "error": "Insufficient credits"}),
        )
        .expect_err("API failure");
        assert!(error.0.contains("Insufficient credits"));
        for name in ["scrape", "crawl", "get_crawl", "map_site", "search"] {
            assert!(render(name, json!({"success": true})).is_err(), "{name}");
        }
        assert!(
            render(
                "scrape",
                json!({"success": false, "data": {"markdown": "x"}})
            )
            .is_err()
        );
        assert!(render("get_crawl", json!({"status": "scraping", "data": []})).is_ok());
        assert!(render("search", json!({"success": true, "data": {}})).is_err());
    }
}
