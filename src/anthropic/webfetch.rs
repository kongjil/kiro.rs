//! `web_fetch` 服务端工具
//!
//! Anthropic 的 `web_fetch_*` 工具由服务端抓取 URL，把正文作为 document 块返回。
//! Kiro 没有这个能力，原样转发只会得到一个没人执行的空调用，所以代理自己实现：
//! 抓取、转成纯文本，以 `server_tool_use` + `web_fetch_tool_result` 呈现给客户端，
//! 并以 `tool_result` 的形式回喂给模型。
//!
//! 抓取目标是模型给出的任意 URL，属于不可信输入，因此：
//! - 只允许 http / https；
//! - 目标主机解析后落在回环、私网、链路本地、ULA 或云元数据网段一律拒绝；
//! - 重定向手动跟随，每一跳重新校验；
//! - 响应体与正文都有上限，避免被超大文件拖死。

use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::time::Duration;

use serde_json::{Value, json};

use super::stream::CompletedToolUse;

/// 单次抓取最多读取的响应体字节数。
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
/// 回喂给模型 / 返回给客户端的正文上限（字符）。
const MAX_TEXT_CHARS: usize = 100_000;
/// 最多跟随的重定向次数。
const MAX_REDIRECTS: usize = 5;
const REQUEST_TIMEOUT_SECS: u64 = 30;
const USER_AGENT: &str = "kiro-rs-webfetch/1.0";

/// 是否是 Anthropic 原生的 web_fetch 工具（`web_fetch_20250910` 等）。
///
/// 与 web_search 一样按前缀匹配，后续版本号无需改代码。
pub(crate) fn is_native_web_fetch_tool(t: &super::types::Tool) -> bool {
    t.name == "web_fetch"
        && t.tool_type
            .as_deref()
            .is_some_and(|typ| typ.starts_with("web_fetch_"))
}

/// 抓取结果。失败不抛错——一个抓不到的页面不该让整个请求失败。
#[derive(Debug, Clone)]
pub(crate) enum FetchOutcome {
    Fetched {
        url: String,
        media_type: String,
        text: String,
        truncated: bool,
    },
    Failed {
        url: String,
        error_code: &'static str,
    },
}

impl FetchOutcome {
    /// 回喂给模型的 `tool_result` 文本。
    pub(crate) fn model_content(&self) -> String {
        match self {
            Self::Fetched {
                url,
                media_type,
                text,
                truncated,
            } => {
                let mut out = format!("Contents of {url} ({media_type}):\n\n{text}");
                if *truncated {
                    out.push_str(
                        "\n\n[content truncated: the page is longer than the fetch limit]",
                    );
                }
                out
            }
            Self::Failed { url, error_code } => format!(
                "Web fetch for {url} failed ({error_code}). \
                 Continue without it and tell the user the page could not be fetched."
            ),
        }
    }

    /// 呈现给客户端的 `web_fetch_tool_result.content`。
    pub(crate) fn client_block(&self) -> Value {
        match self {
            Self::Fetched {
                url,
                media_type,
                text,
                truncated,
            } => json!({
                "type": "web_fetch_result",
                "url": url,
                "retrieved_at": chrono::Utc::now().to_rfc3339(),
                "content": {
                    "type": "document",
                    "source": { "type": "text", "media_type": "text/plain", "data": text },
                    "title": null,
                    "citations": { "enabled": false },
                    "truncated": truncated,
                    "media_type": media_type
                }
            }),
            Self::Failed { url, error_code } => json!({
                "type": "web_fetch_result",
                "url": url,
                "content": null,
                "error_code": error_code
            }),
        }
    }

    pub(crate) fn is_error(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// 从工具调用里取 URL。
pub(crate) fn tool_url(tu: &CompletedToolUse) -> Option<String> {
    tu.input
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 执行一次抓取。
pub(crate) async fn fetch(tool_use: &CompletedToolUse) -> FetchOutcome {
    let Some(raw_url) = tool_url(tool_use) else {
        tracing::warn!(tool_use_id = %tool_use.id, "web_fetch input has no url");
        return FetchOutcome::Failed {
            url: String::new(),
            error_code: "invalid_input",
        };
    };

    match fetch_text(&raw_url).await {
        Ok((url, media_type, text, truncated)) => FetchOutcome::Fetched {
            url,
            media_type,
            text,
            truncated,
        },
        Err(code) => {
            tracing::warn!(url = %raw_url, error_code = code, "web_fetch failed");
            FetchOutcome::Failed {
                url: raw_url,
                error_code: code,
            }
        }
    }
}

async fn fetch_text(raw_url: &str) -> Result<(String, String, String, bool), &'static str> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        // 重定向自己跟随：每一跳都要重新做内网校验。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "unavailable")?;

    let mut current = normalize_url(raw_url)?;

    let mut hops = 0usize;
    let response = loop {
        let target = parse_and_check(&current)?;
        let response = client
            .get(target.clone())
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await
            .map_err(|_| "url_not_accessible")?;

        if !response.status().is_redirection() {
            break (target, response);
        }
        if hops >= MAX_REDIRECTS {
            return Err("url_not_accessible");
        }
        hops += 1;
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or("url_not_accessible")?;
        current = target
            .join(location)
            .map_err(|_| "url_not_accessible")?
            .to_string();
    };
    let (final_url, response) = response;

    if !response.status().is_success() {
        return Err("url_not_accessible");
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/html")
        .to_ascii_lowercase();
    let is_pdf = content_type.contains("pdf");
    let is_text = content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("javascript");
    if !is_text && !is_pdf {
        return Err("unsupported_content_type");
    }

    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await.map_err(|_| "url_not_accessible")? {
        body.extend_from_slice(&chunk);
        if body.len() >= MAX_BODY_BYTES {
            break;
        }
    }
    let truncated_by_size = body.len() >= MAX_BODY_BYTES;
    body.truncate(MAX_BODY_BYTES);

    let text = if is_pdf {
        super::pdf_text::extract_pdf_text(&body).ok_or("unavailable")?
    } else if content_type.contains("html") {
        html_to_text(&String::from_utf8_lossy(&body))
    } else {
        String::from_utf8_lossy(&body).into_owned()
    };

    let (text, truncated_by_chars) = truncate_chars(text, MAX_TEXT_CHARS);
    Ok((
        final_url.to_string(),
        content_type,
        text,
        truncated_by_size || truncated_by_chars,
    ))
}

fn normalize_url(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(trimmed.to_string());
    }
    if trimmed.contains("://") {
        return Err("url_not_accessible");
    }
    // 没写协议的按 https 处理
    Ok(format!("https://{trimmed}"))
}

/// 解析并校验目标：只允许 http/https，且解析结果不得落在内网。
fn parse_and_check(url: &str) -> Result<reqwest::Url, &'static str> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "url_not_accessible")?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err("url_not_accessible"),
    }
    let host = parsed.host_str().ok_or("url_not_accessible")?;

    // IP 字面量直接判定；域名解析后逐个判定（避免 DNS 指向内网）。
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_blocked_ip(ip) {
            Err("url_not_accessible")
        } else {
            Ok(parsed)
        };
    }

    let port = parsed.port_or_known_default().unwrap_or(443);
    let mut resolved = false;
    for addr in (host, port)
        .to_socket_addrs()
        .map_err(|_| "url_not_accessible")?
    {
        resolved = true;
        if is_blocked_ip(addr.ip()) {
            return Err("url_not_accessible");
        }
    }
    if !resolved {
        return Err("url_not_accessible");
    }
    Ok(parsed)
}

/// 回环、私网、链路本地、ULA、云元数据网段一律拒绝。
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_v4(mapped);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 链路本地
        }
    }
}

fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    let [a, b, _, _] = v4.octets();
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        || a == 0
        // 100.64.0.0/10：运营商级 NAT，常被用作内网跳板
        || (a == 100 && (64..128).contains(&b))
}

/// 极简 HTML 转文本：去掉脚本 / 样式 / 注释与标签，压缩空白，还原常见实体。
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut index = 0usize;

    while index < html.len() {
        let rest = &html[index..];

        if rest.starts_with('<') {
            let head = rest.get(..16).unwrap_or(rest).to_ascii_lowercase();
            // 整段跳过的内容：脚本、样式、注释
            let skip_to = ["<script", "<style", "<noscript", "<!--"]
                .iter()
                .find(|tag| head.starts_with(*tag))
                .map(|tag| match *tag {
                    "<script" => "</script",
                    "<style" => "</style",
                    "<noscript" => "</noscript",
                    _ => "-->",
                });
            if let Some(closing) = skip_to {
                let consumed = rest
                    .to_ascii_lowercase()
                    .find(closing)
                    .map(|pos| pos + closing.len())
                    .unwrap_or(rest.len());
                index += consumed;
                out.push(' ');
                continue;
            }

            let block = head.as_str();
            let is_block = [
                "<br", "<p", "<div", "<li", "<tr", "<h1", "<h2", "<h3", "<h4", "<h5", "<h6",
                "</p", "</div", "</li", "</tr", "</h1", "</h2", "</h3", "</h4", "</h5", "</h6",
            ]
            .iter()
            .any(|tag| block.starts_with(tag));
            index += rest.find('>').map(|pos| pos + 1).unwrap_or(rest.len());
            out.push(if is_block { '\n' } else { ' ' });
            continue;
        }

        // 实体
        if rest.starts_with('&')
            && let Some(end) = rest.find(';').filter(|pos| *pos <= 10)
            && let Some(ch) = decode_entity(&rest[1..end])
        {
            out.push(ch);
            index += end + 1;
            continue;
        }

        let ch = rest.chars().next().unwrap_or(' ');
        out.push(ch);
        index += ch.len_utf8();
    }

    collapse_whitespace(&out)
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some(' '),
        other => other
            .strip_prefix('#')
            .and_then(|num| match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => num.parse::<u32>().ok(),
            })
            .and_then(char::from_u32),
    }
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_lines = 0;
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank_lines += 1;
            if blank_lines > 1 {
                continue;
            }
        } else {
            blank_lines = 0;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.trim().to_string()
}

fn truncate_chars(text: String, limit: usize) -> (String, bool) {
    if text.chars().count() <= limit {
        return (text, false);
    }
    (text.chars().take(limit).collect(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(input: Value) -> CompletedToolUse {
        CompletedToolUse {
            id: "toolu_1".to_string(),
            name: "web_fetch".to_string(),
            input,
        }
    }

    fn typed(name: &str, tool_type: Option<&str>) -> super::super::types::Tool {
        super::super::types::Tool {
            tool_type: tool_type.map(str::to_string),
            name: name.to_string(),
            description: String::new(),
            input_schema: Default::default(),
            max_uses: None,
            cache_control: None,
        }
    }

    #[test]
    fn detects_only_typed_web_fetch() {
        assert!(is_native_web_fetch_tool(&typed(
            "web_fetch",
            Some("web_fetch_20250910")
        )));
        // 普通同名工具由客户端执行，不算原生工具
        assert!(!is_native_web_fetch_tool(&typed("web_fetch", None)));
        // web_search 不走这里
        assert!(!is_native_web_fetch_tool(&typed(
            "web_search",
            Some("web_search_20250305")
        )));
    }

    #[test]
    fn private_and_metadata_addresses_are_blocked() {
        for ip in [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(is_blocked_ip(ip.parse().unwrap()), "{ip} 应被拒绝");
        }
        for ip in ["93.184.216.34", "1.1.1.1", "2606:4700::1111"] {
            assert!(!is_blocked_ip(ip.parse().unwrap()), "{ip} 应被放行");
        }
    }

    #[test]
    fn ipv4_mapped_ipv6_is_checked_as_ipv4() {
        // ::ffff:127.0.0.1 等价于 127.0.0.1
        assert!(is_blocked_ip("::ffff:127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn url_without_scheme_defaults_to_https() {
        assert_eq!(
            normalize_url("example.com/a").unwrap(),
            "https://example.com/a"
        );
        assert!(normalize_url("ftp://example.com").is_err());
        assert!(normalize_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn internal_hosts_are_rejected_before_any_request() {
        assert!(parse_and_check("http://127.0.0.1:8080/").is_err());
        assert!(parse_and_check("http://localhost/").is_err());
        assert!(parse_and_check("http://169.254.169.254/latest/meta-data/").is_err());
    }

    #[test]
    fn html_strips_scripts_styles_and_keeps_text() {
        let html = r#"<html><head><style>.a{color:red}</style>
            <script>var x = "</p>";</script></head>
            <body><h1>Title</h1><p>Hello&nbsp;<b>world</b> &amp; friends</p>
            <ul><li>one</li><li>two</li></ul></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Title"), "{text}");
        assert!(text.contains("Hello world & friends"), "{text}");
        assert!(text.contains("one"), "{text}");
        assert!(!text.contains("var x"), "{text}");
        assert!(!text.contains("color:red"), "{text}");
        assert!(!text.contains('<'), "{text}");
    }

    #[test]
    fn html_handles_multibyte_text_without_panicking() {
        let html = "<p>中文标题</p><p>段落内容，包含标点。</p>";
        let text = html_to_text(html);
        assert!(text.contains("中文标题"), "{text}");
        assert!(text.contains("段落内容"), "{text}");
    }

    #[test]
    fn fetched_outcome_shape_matches_contract() {
        let outcome = FetchOutcome::Fetched {
            url: "https://example.com".to_string(),
            media_type: "text/html".to_string(),
            text: "hello".to_string(),
            truncated: false,
        };
        let block = outcome.client_block();
        assert_eq!(block["type"], "web_fetch_result");
        assert_eq!(block["content"]["type"], "document");
        assert_eq!(block["content"]["source"]["media_type"], "text/plain");
        assert_eq!(block["content"]["source"]["data"], "hello");
        assert!(outcome.model_content().contains("https://example.com"));
        assert!(!outcome.is_error());
    }

    #[test]
    fn failed_outcome_carries_error_code() {
        let outcome = FetchOutcome::Failed {
            url: "https://example.com".to_string(),
            error_code: "url_not_accessible",
        };
        assert_eq!(outcome.client_block()["error_code"], "url_not_accessible");
        assert!(outcome.model_content().contains("url_not_accessible"));
        assert!(outcome.is_error());
    }

    #[test]
    fn missing_url_is_invalid_input() {
        let outcome = futures::executor::block_on(fetch(&tool(json!({"prompt": "x"}))));
        assert!(matches!(
            outcome,
            FetchOutcome::Failed {
                error_code: "invalid_input",
                ..
            }
        ));
    }
}
