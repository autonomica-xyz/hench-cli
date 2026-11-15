//! Pure parsers for site_scrape seed discovery: robots.txt `Sitemap:` lines
//! and sitemap `<loc>` entries. No network, no I/O — port of the Android
//! SiteScrapeTool companion-object helpers.

/// `Sitemap:` directives of a robots.txt body; http(s) only. Case-insensitive,
/// whitespace-tolerant; `#` comment tails are harmless because URLs contain
/// no spaces and the regex stops at the first one.
pub fn parse_robots_sitemaps(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        // `^\s*sitemap\s*:` — the word, optional spaces, then the colon.
        let Some(after_word) = strip_prefix_icase(line, "sitemap") else {
            continue;
        };
        let after_word = after_word.trim_start();
        let Some(rest) = after_word.strip_prefix(':') else {
            continue;
        };
        // `\s*` after the colon, then the URL is the first non-space run
        // (stops before a `#` comment or any whitespace).
        let url = rest
            .trim_start()
            .split(|c: char| c.is_whitespace() || c == '#')
            .next()
            .unwrap_or("")
            .trim();
        if url.starts_with("http://") || url.starts_with("https://") {
            out.push(url.to_string());
        }
    }
    out
}

/// Case-insensitive `strip_prefix` for ASCII needles; None when `s` is too
/// short or the cut would split a UTF-8 char.
fn strip_prefix_icase<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// `<loc>` scan result: page URLs vs child sitemap URLs, split by the root
/// tag. Regex-scan equivalent of the Android port — a malformed sitemap just
/// yields fewer seeds and the crawl goes on.
pub struct SitemapParse {
    pub pages: Vec<String>,
    pub children: Vec<String>,
}

/// `<loc>` entries of a sitemap body, split into (page urls, child sitemap
/// urls) by the root tag (`<sitemapindex>` => all children). `&amp;` is
/// decoded; other entities are left alone (matches Android).
pub fn parse_sitemap(body: &str) -> SitemapParse {
    let mut locs = Vec::new();
    let mut rest = body;
    while let Some(start) = find_loc_open(rest) {
        rest = &rest[start..];
        let Some(close) = find_loc_close(rest) else { break };
        let inner = &rest[..close];
        // Skip past the closing tag for the next scan.
        rest = &rest[close + "</loc>".len()..];
        let url = inner
            .trim_start_matches(|c: char| c.is_whitespace())
            .trim_end()
            .replace("&amp;", "&");
        if !url.is_empty() {
            locs.push(url);
        }
    }
    if locs.is_empty() {
        return SitemapParse {
            pages: Vec::new(),
            children: Vec::new(),
        };
    }
    let is_index = contains_tag(body, "sitemapindex");
    if is_index {
        SitemapParse {
            pages: Vec::new(),
            children: locs,
        }
    } else {
        SitemapParse {
            pages: locs,
            children: Vec::new(),
        }
    }
}

/// Find the next `<loc` open tag (case-insensitive), returning the index just
/// after `>` — or None when no complete open tag remains.
fn find_loc_open(hay: &str) -> Option<usize> {
    let bytes = hay.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i] == b'<'
            && bytes[i + 1].eq_ignore_ascii_case(&b'l')
            && bytes[i + 2].eq_ignore_ascii_case(&b'o')
            && bytes[i + 3].eq_ignore_ascii_case(&b'c')
            && (bytes[i + 4] == b'>' || bytes[i + 4].is_ascii_whitespace())
        {
            let end = hay[i..].find('>')? + i;
            return Some(end + 1);
        }
        i += 1;
    }
    None
}

/// Case-insensitive index of the next `</loc>` — mirrors the Android
/// `<loc>...</loc>` regex with IGNORE_CASE.
fn find_loc_close(hay: &str) -> Option<usize> {
    let bytes = hay.as_bytes();
    let mut i = 0;
    while i + 5 < bytes.len() {
        if bytes[i] == b'<'
            && bytes[i + 1] == b'/'
            && bytes[i + 2].eq_ignore_ascii_case(&b'l')
            && bytes[i + 3].eq_ignore_ascii_case(&b'o')
            && bytes[i + 4].eq_ignore_ascii_case(&b'c')
            && bytes[i + 5] == b'>'
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Case-insensitive `<tag` followed by whitespace or `>` — mirrors the
/// Android `<sitemapindex[\s>]` probe without allocating a lowered copy.
fn contains_tag(hay: &str, tag: &str) -> bool {
    let bytes = hay.as_bytes();
    let needle = tag.as_bytes();
    let mut i = 0;
    while i + 1 + needle.len() <= bytes.len() {
        if bytes[i] == b'<'
            && bytes
                .get(i + 1..i + 1 + needle.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(needle))
        {
            let next = bytes.get(i + 1 + needle.len()).copied();
            if next.is_none_or(|b| b == b'>' || b.is_ascii_whitespace()) {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// File extensions a page render can never read; links/sitemap entries ending
/// in one are skipped instead of burning a paid page. Port of the Android
/// NON_PAGE regex (case-insensitive, anchored at end of path).
pub fn is_non_page(path: &str) -> bool {
    const EXTS: &[&str] = &[
        "pdf", "zip", "gz", "tgz", "rar", "7z", "exe", "dmg", "apk", "iso", "mp3", "mp4", "m4a",
        "mov", "avi", "mkv", "webm", "wav", "ogg", "jpg", "jpeg", "png", "gif", "webp", "svg",
        "ico", "css", "js", "json", "xml", "rss", "woff", "woff2", "ttf",
    ];
    let Some(dot) = path.rfind('.') else { return false };
    // A dot in the last path segment only — `/dir.name/file` has no extension.
    if path[dot + 1..].contains('/') {
        return false;
    }
    let ext = &path[dot + 1..];
    EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robots_parses_case_and_noise() {
        let body = "# comment\nUser-agent: *\nDisallow: /admin\n\
                    SITEMAP: https://ex.com/sitemap.xml\n\
                    sitemap:   https://ex.com/s2.xml   # trailing\n\
                    Sitemap:https://ex.com/packed.xml\n\
                    sitemap: ftp://ex.com/nope.xml\n\
                    sitemap: not-a-url\n\
                    sitemap : https://ex.com/space-before-colon.xml\n";
        assert_eq!(
            parse_robots_sitemaps(body),
            vec![
                "https://ex.com/sitemap.xml",
                "https://ex.com/s2.xml",
                "https://ex.com/packed.xml",
                "https://ex.com/space-before-colon.xml",
            ]
        );
    }

    #[test]
    fn robots_rejects_non_http() {
        assert!(parse_robots_sitemaps("sitemap: ftp://x/s.xml\nsitemap: file:///x\n").is_empty());
    }

    #[test]
    fn urlset_splits_pages() {
        let body = r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
            <url><loc>https://ex.com/a?x=1&amp;y=2</loc></url>
            <URL><LOC> https://ex.com/b </LOC></URL>
        </urlset>"#;
        let p = parse_sitemap(body);
        assert_eq!(p.pages, vec!["https://ex.com/a?x=1&y=2", "https://ex.com/b"]);
        assert!(p.children.is_empty());
    }

    #[test]
    fn sitemapindex_splits_children() {
        let body = r#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
            <sitemap><loc>https://ex.com/s1.xml</loc></sitemap>
            <sitemap><loc>https://ex.com/s2.xml.gz</loc></sitemap>
        </sitemapindex>"#;
        let p = parse_sitemap(body);
        assert!(p.pages.is_empty());
        assert_eq!(p.children, vec!["https://ex.com/s1.xml", "https://ex.com/s2.xml.gz"]);
    }

    #[test]
    fn empty_or_malformed_yields_nothing() {
        assert!(parse_sitemap("").pages.is_empty());
        assert!(parse_sitemap("<urlset><loc>unclosed").pages.is_empty());
    }

    #[test]
    fn non_page_extensions() {
        for p in [
            "/a.pdf", "/a.PDF", "/x/y/z.zip", "/img/logo.png", "/f.JPEG", "/d/file.woff2",
            "/d/file.TTF", "/feed.xml", "/sitemap.xml",
        ] {
            assert!(is_non_page(p), "{p} should be non-page");
        }
        // callers pass Url::path() — no query/fragment reaches is_non_page
        for p in ["/", "/about", "/dir.name/file", "/a.pdfish", "/page.html", "/x.pdf/"] {
            assert!(!is_non_page(p), "{p} should be a page");
        }
    }
}
