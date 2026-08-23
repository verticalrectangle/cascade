//! Markdown parser — splits a transcript body into structured [`Block`]s
//! with inline [`Run`]s. Tags map onto GtkTextTag names styled per theme.

// ── structured AST (comprehensive renderer) ─────────────────────────
// Tag vocabulary (theme-styled): "assistant" (plain), "md-bold", "md-italic",
// "md-bold-italic", "md-inline-code", "md-link", "md-strike", "md-highlight",
// "md-list-marker", "md-quote", "diff-add", "diff-remove".

/// One styled inline run. `link` is set for `md-link` runs (markdown or
/// bare-URL links) so the renderer can make them clickable.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub text: String,
    pub tag: &'static str,
    pub link: Option<String>,
}

impl Run {
    pub fn plain(text: impl Into<String>, tag: &'static str) -> Self {
        Self {
            text: text.into(),
            tag,
            link: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ListKind {
    Bullet,
    /// Ordered list, author's own number preserved.
    Numbered(u64),
    Task { checked: bool },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// A rendered block of a transcript entry.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// Paragraph of inline runs.
    Prose(Vec<Run>),
    /// `#`–`####`; level 1..=4.
    Heading { level: u8, runs: Vec<Run> },
    /// One list item; `level` = nesting depth (0-based).
    ListItem {
        level: u8,
        kind: ListKind,
        runs: Vec<Run>,
    },
    /// Consecutive `> ` lines, joined.
    Quote { runs: Vec<Run> },
    /// `---` / `***` / `___`.
    Rule,
    /// GFM pipe table; cells carry inline runs.
    Table {
        header: Vec<Vec<Run>>,
        aligns: Vec<Align>,
        rows: Vec<Vec<Vec<Run>>>,
    },
    /// Fenced code block: language hint + raw code.
    Code { lang: String, code: String },
    /// `<advisory severity guidance>…</advisory>` callout; body re-parses.
    Advisory {
        severity: Option<String>,
        guidance: Option<String>,
        body: String,
    },
    /// `![alt](target)`; target is a local path or URL.
    Image { alt: String, target: String },
}

/// Parse a transcript body into structured blocks.
///
/// Streaming-safe: unterminated fences/advisories consume the remainder;
/// unpaired inline markers stay literal; never panics on partial input.
/// CRLF line endings are tolerated.
pub fn parse_blocks(body: &str) -> Vec<Block> {
    if body.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = body
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();

    let mut out: Vec<Block> = Vec::new();
    let mut prose: Vec<Run> = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.starts_with("```") {
            flush_prose(&mut out, &mut prose);
            let lang = trimmed[3..].trim().to_string();
            i += 1;
            let mut body_lines: Vec<&str> = Vec::new();
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                body_lines.push(lines[i]);
                i += 1;
            }
            if i < lines.len() {
                i += 1; // closing fence
            }
            out.push(Block::Code {
                lang,
                code: body_lines.join("\n"),
            });
            continue;
        }

        if let Some(adv) = try_advisory(&lines, &mut i) {
            flush_prose(&mut out, &mut prose);
            if let Some(prefix) = adv.prefix {
                let runs = parse_inline(&prefix, "assistant");
                if runs.iter().any(|r| !r.text.trim().is_empty()) {
                    out.push(Block::Prose(runs));
                }
            }
            out.push(Block::Advisory {
                severity: adv.severity,
                guidance: adv.guidance,
                body: adv.body,
            });
            if let Some(suffix) = adv.suffix {
                append_prose_line(&mut prose, parse_inline(&suffix, "assistant"));
            }
            continue;
        }

        if trimmed.is_empty() {
            flush_prose(&mut out, &mut prose);
            i += 1;
            continue;
        }

        if let Some((alt, target)) = image_only_line(line) {
            flush_prose(&mut out, &mut prose);
            out.push(Block::Image { alt, target });
            i += 1;
            continue;
        }

        if is_rule_line(trimmed) {
            flush_prose(&mut out, &mut prose);
            out.push(Block::Rule);
            i += 1;
            continue;
        }

        if let Some((level, content)) = parse_heading(line) {
            flush_prose(&mut out, &mut prose);
            out.push(Block::Heading {
                level,
                runs: parse_inline(content, "assistant"),
            });
            i += 1;
            continue;
        }

        if let Some((level, kind, content)) = parse_list_item(line) {
            flush_prose(&mut out, &mut prose);
            out.push(Block::ListItem {
                level,
                kind,
                runs: parse_inline(content, "assistant"),
            });
            i += 1;
            continue;
        }

        if is_quote_line(line) {
            flush_prose(&mut out, &mut prose);
            let mut chunks: Vec<&str> = Vec::new();
            while i < lines.len() && is_quote_line(lines[i]) {
                chunks.push(quote_content(lines[i]));
                i += 1;
            }
            out.push(Block::Quote {
                runs: parse_inline(&chunks.join("\n"), "md-quote"),
            });
            continue;
        }

        if let Some(table) = try_table(&lines, i) {
            flush_prose(&mut out, &mut prose);
            i = table.end;
            out.push(Block::Table {
                header: table.header,
                aligns: table.aligns,
                rows: table.rows,
            });
            continue;
        }

        if let Some(tag) = diff_tag_for(line) {
            append_prose_line(&mut prose, vec![Run::plain(line, tag)]);
            i += 1;
            continue;
        }

        append_prose_line(&mut prose, parse_inline(line, "assistant"));
        i += 1;
    }

    flush_prose(&mut out, &mut prose);
    out
}

fn flush_prose(out: &mut Vec<Block>, prose: &mut Vec<Run>) {
    if prose.iter().any(|r| !r.text.trim().is_empty()) {
        out.push(Block::Prose(std::mem::take(prose)));
    } else {
        prose.clear();
    }
}

fn append_prose_line(prose: &mut Vec<Run>, runs: Vec<Run>) {
    if runs.is_empty() {
        return;
    }
    if !prose.is_empty() {
        push_run(prose, "\n", "assistant", None);
    }
    for r in runs {
        push_run(prose, &r.text, r.tag, r.link);
    }
}

fn push_run(segs: &mut Vec<Run>, text: &str, tag: &'static str, link: Option<String>) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = segs.last_mut() {
        if last.tag == tag && last.link == link {
            last.text.push_str(text);
            return;
        }
    }
    segs.push(Run {
        text: text.to_string(),
        tag,
        link,
    });
}

// ── block detectors ──────────────────────────────────────────────────

fn parse_heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 4 {
        return None;
    }
    let rest = &line[hashes..];
    if !(rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    Some((hashes as u8, rest.trim_start()))
}

fn parse_list_item(line: &str) -> Option<(u8, ListKind, &str)> {
    let mut indent = 0u32;
    let mut idx = 0usize;
    let bytes = line.as_bytes();
    while idx < bytes.len() {
        match bytes[idx] {
            b' ' => {
                indent += 1;
                idx += 1;
            }
            b'\t' => {
                indent += 2;
                idx += 1;
            }
            _ => break,
        }
    }
    let rest = &line[idx..];
    let level = (indent / 2) as u8;

    if let Some(body) = strip_task(rest) {
        if body.is_empty() {
            return None;
        }
        let checked = rest.starts_with("- [x]") || rest.starts_with("- [X]");
        return Some((level, ListKind::Task { checked }, body));
    }

    for prefix in ["- ", "* ", "• "] {
        if let Some(body) = rest.strip_prefix(prefix) {
            if body.is_empty() {
                return None;
            }
            return Some((level, ListKind::Bullet, body));
        }
    }

    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..=3).contains(&digits) {
        let n: u64 = rest[..digits].parse().ok()?;
        let tail = &rest[digits..];
        if let Some(body) = tail
            .strip_prefix(". ")
            .or_else(|| tail.strip_prefix(") "))
        {
            if body.is_empty() {
                return None;
            }
            return Some((level, ListKind::Numbered(n), body));
        }
    }
    None
}

/// `- [ ]` / `- [x]` / `- [X]` plus a following space (or end-of-line).
fn strip_task(rest: &str) -> Option<&str> {
    for marker in ["- [ ]", "- [x]", "- [X]"] {
        if let Some(after) = rest.strip_prefix(marker) {
            if after.is_empty() {
                return Some("");
            }
            return after.strip_prefix(' ');
        }
    }
    None
}

fn is_quote_line(line: &str) -> bool {
    line.trim_start_matches([' ', '\t']).starts_with('>')
}

fn quote_content(line: &str) -> &str {
    let rest = line.trim_start_matches([' ', '\t']);
    let rest = rest.strip_prefix('>').unwrap_or(rest);
    rest.strip_prefix(' ').unwrap_or(rest)
}

fn is_rule_line(trimmed: &str) -> bool {
    let mut marker: Option<char> = None;
    let mut count = 0usize;
    for c in trimmed.chars() {
        if c == ' ' || c == '\t' {
            continue;
        }
        if c != '-' && c != '*' && c != '_' {
            return false;
        }
        match marker {
            None => marker = Some(c),
            Some(m) if m != c => return false,
            Some(_) => {}
        }
        count += 1;
    }
    count >= 3
}

fn image_only_line(line: &str) -> Option<(String, String)> {
    let s = line.trim();
    let rest = s.strip_prefix("![")?;
    let close_alt = rest.find("](")?;
    let alt = rest[..close_alt].to_string();
    let after = &rest[close_alt + 2..];
    let close_tgt = after.find(')')?;
    if close_tgt + 1 != after.len() {
        return None;
    }
    Some((alt, after[..close_tgt].to_string()))
}

fn diff_tag_for(line: &str) -> Option<&'static str> {
    if line.len() < 2 {
        return None;
    }
    match line.as_bytes()[0] {
        b'+' => Some("diff-add"),
        b'-' => Some("diff-remove"),
        _ => None,
    }
}

// ── GFM tables ───────────────────────────────────────────────────────

struct ParsedTable {
    header: Vec<Vec<Run>>,
    aligns: Vec<Align>,
    rows: Vec<Vec<Vec<Run>>>,
    end: usize,
}

fn try_table(lines: &[&str], i: usize) -> Option<ParsedTable> {
    if !lines[i].contains('|') || i + 1 >= lines.len() {
        return None;
    }
    let aligns_raw = parse_delim_row(lines[i + 1])?;
    let header_raw = split_cells(lines[i]);
    if header_raw.is_empty() {
        return None;
    }
    let width = header_raw.len();
    let header: Vec<Vec<Run>> = header_raw
        .into_iter()
        .map(|c| parse_inline(&c, "assistant"))
        .collect();
    let mut aligns = aligns_raw;
    aligns.truncate(width);
    while aligns.len() < width {
        aligns.push(Align::Left);
    }
    let mut rows = Vec::new();
    let mut j = i + 2;
    while j < lines.len() && is_table_row_line(lines[j]) {
        rows.push(fit_row(
            split_cells(lines[j])
                .into_iter()
                .map(|c| parse_inline(&c, "assistant"))
                .collect(),
            width,
        ));
        j += 1;
    }
    Some(ParsedTable {
        header,
        aligns,
        rows,
        end: j,
    })
}

fn is_table_row_line(line: &str) -> bool {
    line.contains('|')
}

fn split_cells(line: &str) -> Vec<String> {
    let mut t = line.trim();
    if t.starts_with('|') {
        t = &t[1..];
    }
    if t.ends_with('|') {
        t = &t[..t.len() - 1];
    }
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn parse_delim_row(line: &str) -> Option<Vec<Align>> {
    let cells = split_cells(line);
    if cells.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(cells.len());
    for c in cells {
        out.push(cell_align(&c)?);
    }
    Some(out)
}

fn cell_align(cell: &str) -> Option<Align> {
    let c = cell.trim();
    if c.is_empty() {
        return None;
    }
    let start_colon = c.starts_with(':');
    let end_colon = c.ends_with(':');
    let mid = &c[usize::from(start_colon)..c.len() - usize::from(end_colon)];
    if mid.is_empty() || !mid.chars().all(|ch| ch == '-') {
        return None;
    }
    Some(if start_colon && end_colon {
        Align::Center
    } else if end_colon {
        Align::Right
    } else {
        Align::Left
    })
}

fn fit_row(mut row: Vec<Vec<Run>>, width: usize) -> Vec<Vec<Run>> {
    row.truncate(width);
    while row.len() < width {
        row.push(Vec::new());
    }
    row
}

// ── advisory (iOS markdownBlocks extraction, lines 197–252) ──────────

struct AdvisoryParts {
    prefix: Option<String>,
    suffix: Option<String>,
    severity: Option<String>,
    guidance: Option<String>,
    body: String,
}

fn try_advisory(lines: &[&str], i: &mut usize) -> Option<AdvisoryParts> {
    let trimmed = lines[*i].trim();
    let start = advisory_start(trimmed)?;
    let tag_end = advisory_tag_end(trimmed, start)?;
    let prefix_raw = &trimmed[..start];
    let prefix = if prefix_raw.trim().is_empty() {
        None
    } else {
        Some(prefix_raw.to_string())
    };
    let opener = &trimmed[start..tag_end];
    let (severity, guidance) = advisory_attrs(opener);
    let rest = &trimmed[tag_end..];

    if let Some((cs, ce)) = advisory_closer_range(rest) {
        let piece = &rest[..cs];
        let mut parts: Vec<&str> = Vec::new();
        if !piece.is_empty() {
            parts.push(piece);
        }
        let suffix_raw = &rest[ce..];
        let suffix = if suffix_raw.trim().is_empty() {
            None
        } else {
            Some(suffix_raw.to_string())
        };
        *i += 1;
        return Some(AdvisoryParts {
            prefix,
            suffix,
            severity,
            guidance,
            body: decode_entities(&parts.join("\n")),
        });
    }

    let mut parts: Vec<String> = Vec::new();
    if !rest.is_empty() {
        parts.push(rest.to_string());
    }
    *i += 1;
    while *i < lines.len() && advisory_closer_range(lines[*i]).is_none() {
        parts.push(lines[*i].to_string());
        *i += 1;
    }
    let mut suffix = None;
    if *i < lines.len() {
        if let Some((cs, ce)) = advisory_closer_range(lines[*i]) {
            let piece = &lines[*i][..cs];
            if !piece.is_empty() {
                parts.push(piece.to_string());
            }
            let suffix_raw = &lines[*i][ce..];
            if !suffix_raw.trim().is_empty() {
                suffix = Some(suffix_raw.to_string());
            }
        }
        *i += 1;
    }
    Some(AdvisoryParts {
        prefix,
        suffix,
        severity,
        guidance,
        body: decode_entities(&parts.join("\n")),
    })
}

fn advisory_start(line: &str) -> Option<usize> {
    if let Some(p) = line.find("<advisory") {
        return Some(p);
    }
    line.find("&lt;advisory")
}

fn advisory_tag_end(line: &str, start: usize) -> Option<usize> {
    let suffix = &line[start..];
    let lit = suffix.find('>').map(|p| start + p + 1);
    let ent = suffix.find("&gt;").map(|p| start + p + 4);
    match (lit, ent) {
        (Some(l), Some(e)) => Some(l.min(e)),
        (a, b) => a.or(b),
    }
}

fn advisory_closer_range(line: &str) -> Option<(usize, usize)> {
    if let Some(p) = line.find("</advisory>") {
        return Some((p, p + "</advisory>".len()));
    }
    if let Some(p) = line.find("&lt;/advisory&gt;") {
        return Some((p, p + "&lt;/advisory&gt;".len()));
    }
    None
}

fn advisory_attrs(opener: &str) -> (Option<String>, Option<String>) {
    (quoted_attr(opener, "severity"), quoted_attr(opener, "guidance"))
}

fn quoted_attr(opener: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let start = opener.find(&key)? + key.len();
    let rest = &opener[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

// ── inline span parsing ──────────────────────────────────────────────

fn is_boundary(c: char) -> bool {
    c.is_whitespace() || c.is_ascii_punctuation()
}

/// Scan a line for code, links, strike, highlight, emphasis, and bare URLs.
/// Unpaired markers stay literal. Backslash escapes ASCII punctuation.
fn parse_inline(line: &str, default_tag: &'static str) -> Vec<Run> {
    let chars: Vec<char> = line.chars().collect();
    let mut segs: Vec<Run> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        if c == '\\' && i + 1 < chars.len() && chars[i + 1].is_ascii_punctuation() {
            plain.push(chars[i + 1]);
            i += 2;
            continue;
        }

        if c == '`' {
            if let Some(close) = find_char(&chars, i + 1, '`') {
                if close > i + 1 {
                    push_run(&mut segs, &plain, default_tag, None);
                    plain.clear();
                    let content: String = chars[i + 1..close].iter().collect();
                    push_run(&mut segs, &content, "md-inline-code", None);
                    i = close + 1;
                    continue;
                }
            }
            plain.push(c);
            i += 1;
            continue;
        }

        if c == '!' && chars.get(i + 1) == Some(&'[') {
            if let Some(end) = match_inline_image(&chars, i) {
                for ch in &chars[i..end] {
                    plain.push(*ch);
                }
                i = end;
                continue;
            }
        }

        if c == '[' {
            if let Some((label, url, past)) = match_md_link(&chars, i) {
                push_run(&mut segs, &plain, default_tag, None);
                plain.clear();
                push_run(&mut segs, &label, "md-link", Some(url));
                i = past;
                continue;
            }
            plain.push(c);
            i += 1;
            continue;
        }

        if c == '~' && chars.get(i + 1) == Some(&'~') {
            if let Some((content, past)) = delimited_span(&chars, i, '~', 2) {
                push_run(&mut segs, &plain, default_tag, None);
                plain.clear();
                push_run(&mut segs, &content, "md-strike", None);
                i = past;
                continue;
            }
        }

        if c == '=' && chars.get(i + 1) == Some(&'=') {
            if let Some((content, past)) = delimited_span(&chars, i, '=', 2) {
                push_run(&mut segs, &plain, default_tag, None);
                plain.clear();
                push_run(&mut segs, &content, "md-highlight", None);
                i = past;
                continue;
            }
        }

        if c == '*' || c == '_' {
            let run_end = {
                let mut j = i;
                while j < chars.len() && chars[j] == c {
                    j += 1;
                }
                j
            };
            let len = (run_end - i).min(3);
            let opens = i == 0 || is_boundary(chars[i - 1]);
            if opens {
                if let Some((content, past)) = emphasis_span(&chars, c, len, run_end) {
                    let tag = match len {
                        3 => "md-bold-italic",
                        2 => "md-bold",
                        _ => "md-italic",
                    };
                    push_run(&mut segs, &plain, default_tag, None);
                    plain.clear();
                    push_run(&mut segs, &content, tag, None);
                    i = past;
                    continue;
                }
            }
            for _ in 0..(run_end - i) {
                plain.push(c);
            }
            i = run_end;
            continue;
        }

        if let Some((url, past)) = match_bare_url(&chars, i) {
            push_run(&mut segs, &plain, default_tag, None);
            plain.clear();
            push_run(&mut segs, &url, "md-link", Some(url.clone()));
            i = past;
            continue;
        }

        plain.push(c);
        i += 1;
    }
    push_run(&mut segs, &plain, default_tag, None);
    segs
}

fn find_char(chars: &[char], from: usize, want: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == want)
}

fn delimited_span(chars: &[char], i: usize, mark: char, n: usize) -> Option<(String, usize)> {
    if i + n >= chars.len() {
        return None;
    }
    let content_start = i + n;
    let mut j = content_start;
    while j + n <= chars.len() {
        if (0..n).all(|k| chars[j + k] == mark) {
            if j == content_start {
                return None;
            }
            let content: String = chars[content_start..j].iter().collect();
            return Some((content, j + n));
        }
        j += 1;
    }
    None
}

fn match_md_link(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    let close_bracket = find_char(chars, i + 1, ']')?;
    if close_bracket <= i + 1 {
        return None;
    }
    if chars.get(close_bracket + 1) != Some(&'(') {
        return None;
    }
    let close_paren = find_char(chars, close_bracket + 2, ')')?;
    let label: String = chars[i + 1..close_bracket].iter().collect();
    let url: String = chars[close_bracket + 2..close_paren].iter().collect();
    Some((label, url, close_paren + 1))
}

fn match_inline_image(chars: &[char], i: usize) -> Option<usize> {
    // ![alt](target) starting at i
    if chars.get(i) != Some(&'!') || chars.get(i + 1) != Some(&'[') {
        return None;
    }
    let close_bracket = find_char(chars, i + 2, ']')?;
    if chars.get(close_bracket + 1) != Some(&'(') {
        return None;
    }
    let close_paren = find_char(chars, close_bracket + 2, ')')?;
    Some(close_paren + 1)
}

fn match_bare_url(chars: &[char], i: usize) -> Option<(String, usize)> {
    if i > 0 && !is_boundary(chars[i - 1]) {
        return None;
    }
    let https = ['h', 't', 't', 'p', 's', ':', '/', '/'];
    let http = ['h', 't', 't', 'p', ':', '/', '/'];
    let scheme_len = if chars[i..].starts_with(&https) {
        8
    } else if chars[i..].starts_with(&http) {
        7
    } else {
        return None;
    };
    let after = i + scheme_len;
    if after >= chars.len() || !is_url_char(chars[after]) {
        return None;
    }
    let mut j = after;
    while j < chars.len() && is_url_char(chars[j]) {
        j += 1;
    }
    while j > after {
        let last = chars[j - 1];
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?') {
            j -= 1;
            continue;
        }
        if last == ')' {
            let opens = chars[i..j].iter().filter(|c| **c == '(').count();
            let closes = chars[i..j].iter().filter(|c| **c == ')').count();
            if closes > opens {
                j -= 1;
                continue;
            }
        }
        break;
    }
    if j == after {
        return None;
    }
    let url: String = chars[i..j].iter().collect();
    Some((url, j))
}

fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// Try an emphasis span: run of exactly `len` `c` at `start..run_end`,
/// non-empty content, valid closer. Returns (content, index past closer).
fn emphasis_span(
    chars: &[char],
    c: char,
    len: usize,
    run_end: usize,
) -> Option<(String, usize)> {
    if run_end >= chars.len() || chars[run_end].is_whitespace() {
        return None;
    }
    let mut j = run_end;
    let mut in_code = false;
    while j < chars.len() {
        let ch = chars[j];
        if ch == '`' {
            in_code = !in_code;
            j += 1;
            continue;
        }
        if !in_code && ch == c {
            let mut k = j;
            while k < chars.len() && chars[k] == c {
                k += 1;
            }
            if k - j == len && !chars[j - 1].is_whitespace() {
                let closes = k == chars.len() || is_boundary(chars[k]);
                if closes {
                    let content: String = chars[run_end..j].iter().collect();
                    if content.is_empty() {
                        return None;
                    }
                    return Some((content, k));
                }
            }
            j = k;
            continue;
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(body: &str) -> Vec<&'static str> {
        parse_blocks(body)
            .iter()
            .map(|b| match b {
                Block::Prose(_) => "prose",
                Block::Heading { .. } => "heading",
                Block::ListItem { .. } => "list",
                Block::Quote { .. } => "quote",
                Block::Rule => "rule",
                Block::Table { .. } => "table",
                Block::Code { .. } => "code",
                Block::Advisory { .. } => "advisory",
                Block::Image { .. } => "image",
            })
            .collect()
    }

    fn text_of(runs: &[Run]) -> String {
        runs.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn empty_and_blank_inputs() {
        assert!(parse_blocks("").is_empty());
        assert!(parse_blocks("   ").is_empty());
        assert!(parse_blocks("\n\n").is_empty());
        assert!(parse_blocks("\r\n\r\n").is_empty());
        assert!(parse_blocks(" \n \t \n ").is_empty());
    }

    #[test]
    fn headings_one_through_four() {
        for (src, level, title) in [
            ("# One", 1u8, "One"),
            ("## Two", 2, "Two"),
            ("### Three", 3, "Three"),
            ("#### Four", 4, "Four"),
            ("#\tTab", 1, "Tab"),
        ] {
            match &parse_blocks(src)[..] {
                [Block::Heading { level: l, runs }] => {
                    assert_eq!(*l, level, "{src}");
                    assert_eq!(text_of(runs), title, "{src}");
                }
                other => panic!("{src}: {other:?}"),
            }
        }
        assert_eq!(kinds("##### Five"), ["prose"]);
        assert_eq!(kinds("#NoSpace"), ["prose"]);
        assert_eq!(kinds("#"), ["prose"]);
        match &parse_blocks("# ")[..] {
            [Block::Heading { level: 1, runs }] => assert!(runs.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn list_nesting_levels() {
        let src = "- a\n  - b\n    - c\n\t- tab";
        let blocks = parse_blocks(src);
        match &blocks[..] {
            [Block::ListItem {
                level: 0,
                kind: ListKind::Bullet,
                runs: a,
            }, Block::ListItem {
                level: 1,
                kind: ListKind::Bullet,
                runs: b,
            }, Block::ListItem {
                level: 2,
                kind: ListKind::Bullet,
                runs: c,
            }, Block::ListItem {
                level: 1,
                kind: ListKind::Bullet,
                runs: tab,
            }] => {
                assert_eq!(text_of(a), "a");
                assert_eq!(text_of(b), "b");
                assert_eq!(text_of(c), "c");
                assert_eq!(text_of(tab), "tab");
            }
            other => panic!("{other:?}"),
        }
        // empty body after marker is prose
        assert_eq!(kinds("- "), ["prose"]);
        assert_eq!(kinds("*"), ["prose"]);
        match &parse_blocks("1. first\n12) twelfth")[..] {
            [Block::ListItem {
                kind: ListKind::Numbered(1),
                ..
            }, Block::ListItem {
                kind: ListKind::Numbered(12),
                ..
            }] => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(kinds("1234. too-many-digits"), ["prose"]);
        match &parse_blocks("• bullet")[..] {
            [Block::ListItem {
                kind: ListKind::Bullet,
                ..
            }] => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn task_checkboxes() {
        match &parse_blocks("- [ ] todo\n- [x] done\n- [X] also")[..] {
            [Block::ListItem {
                kind: ListKind::Task { checked: false },
                runs: a,
                ..
            }, Block::ListItem {
                kind: ListKind::Task { checked: true },
                runs: b,
                ..
            }, Block::ListItem {
                kind: ListKind::Task { checked: true },
                runs: c,
                ..
            }] => {
                assert_eq!(text_of(a), "todo");
                assert_eq!(text_of(b), "done");
                assert_eq!(text_of(c), "also");
            }
            other => panic!("{other:?}"),
        }
        // empty checkbox body → prose; star checkbox is a bullet
        assert_eq!(kinds("- [ ]"), ["prose"]);
        match &parse_blocks("* [x] no")[..] {
            [Block::ListItem {
                kind: ListKind::Bullet,
                runs,
                ..
            }] => assert_eq!(text_of(runs), "[x] no"),
            other => panic!("{other:?}"),
        }
        match &parse_blocks("  - [x] nested")[..] {
            [Block::ListItem {
                level: 1,
                kind: ListKind::Task { checked: true },
                ..
            }] => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn table_alignment_and_ragged_rows() {
        let src = "\
| Left | Center | Right |
| :--- | :----: | ----: |
| a | b | c |
| only |
| w | x | y | z |
not a row";
        match &parse_blocks(src)[..] {
            [Block::Table {
                header,
                aligns,
                rows,
            }, Block::Prose(p)] => {
                assert_eq!(header.len(), 3);
                assert_eq!(text_of(&header[0]), "Left");
                assert_eq!(text_of(&header[1]), "Center");
                assert_eq!(text_of(&header[2]), "Right");
                assert_eq!(aligns, &vec![Align::Left, Align::Center, Align::Right]);
                assert_eq!(rows.len(), 3);
                assert_eq!(rows[0].len(), 3);
                assert_eq!(text_of(&rows[0][0]), "a");
                assert_eq!(rows[1].len(), 3);
                assert_eq!(text_of(&rows[1][0]), "only");
                assert!(rows[1][1].is_empty());
                assert!(rows[1][2].is_empty());
                assert_eq!(rows[2].len(), 3);
                assert_eq!(text_of(&rows[2][0]), "w");
                assert_eq!(text_of(&rows[2][2]), "y");
                assert_eq!(text_of(p), "not a row");
            }
            other => panic!("{other:?}"),
        }
        // no delimiter → prose
        assert_eq!(kinds("| a | b |\n| c | d |"), ["prose"]);
        // default align when delimiter has no colons
        match &parse_blocks("| h |\n| --- |\n| r |")[..] {
            [Block::Table { aligns, .. }] => assert_eq!(aligns, &vec![Align::Left]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn quote_grouping() {
        match &parse_blocks("> a\n> b\n> c")[..] {
            [Block::Quote { runs }] => {
                assert_eq!(text_of(runs), "a\nb\nc");
                assert!(runs.iter().all(|r| r.tag == "md-quote"));
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks(">\n> x")[..] {
            [Block::Quote { runs }] => assert_eq!(text_of(runs), "\nx"),
            other => panic!("{other:?}"),
        }
        assert_eq!(kinds("> q\nplain"), ["quote", "prose"]);
        match &parse_blocks("> **bold**")[..] {
            [Block::Quote { runs }] => {
                assert_eq!(runs.len(), 1);
                assert_eq!(runs[0].tag, "md-bold");
                assert_eq!(runs[0].text, "bold");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rules() {
        for src in ["---", "***", "___", "- - -", "* * *", "  ___  "] {
            assert_eq!(kinds(src), ["rule"], "{src}");
        }
        assert_eq!(kinds("--"), ["prose"]);
        assert_eq!(kinds("--- not"), ["prose"]);
        assert_eq!(kinds("*-*-*"), ["prose"]);
        assert_eq!(kinds("a\n---\nb"), ["prose", "rule", "prose"]);
    }

    #[test]
    fn advisory_attributes_and_unterminated() {
        match &parse_blocks(
            r#"<advisory severity="error" guidance="fix it">hello</advisory>"#,
        )[..]
        {
            [Block::Advisory {
                severity,
                guidance,
                body,
            }] => {
                assert_eq!(severity.as_deref(), Some("error"));
                assert_eq!(guidance.as_deref(), Some("fix it"));
                assert_eq!(body, "hello");
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks("<advisory>body</advisory>")[..] {
            [Block::Advisory {
                severity: None,
                guidance: None,
                body,
            }] => assert_eq!(body, "body"),
            other => panic!("{other:?}"),
        }
        match &parse_blocks("<advisory severity=\"info\">\nstill going")[..] {
            [Block::Advisory { severity, body, .. }] => {
                assert_eq!(severity.as_deref(), Some("info"));
                assert_eq!(body, "still going");
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks("&lt;advisory severity=\"x\"&gt;hi&lt;/advisory&gt;")[..] {
            [Block::Advisory {
                severity,
                body,
                ..
            }] => {
                assert_eq!(severity.as_deref(), Some("x"));
                assert_eq!(body, "hi");
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks("hello <advisory severity=\"e\">x</advisory> bye")[..] {
            [Block::Prose(pre), Block::Advisory { body, .. }, Block::Prose(post)] => {
                assert_eq!(text_of(pre), "hello ");
                assert_eq!(body, "x");
                assert_eq!(text_of(post), " bye");
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks("<advisory severity=\"e\">a&amp;b</advisory>")[..] {
            [Block::Advisory { body, .. }] => assert_eq!(body, "a&b"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn images() {
        match &parse_blocks("![alt](./x.png)")[..] {
            [Block::Image { alt, target }] => {
                assert_eq!(alt, "alt");
                assert_eq!(target, "./x.png");
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks("  ![cat](https://ex.com/c.png)  ")[..] {
            [Block::Image { alt, target }] => {
                assert_eq!(alt, "cat");
                assert_eq!(target, "https://ex.com/c.png");
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks("see ![alt](x.png) here")[..] {
            [Block::Prose(runs)] => {
                assert_eq!(text_of(runs), "see ![alt](x.png) here");
                assert!(runs.iter().all(|r| r.tag == "assistant"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(kinds("![]()"), ["image"]);
    }

    #[test]
    fn escapes_and_inline_styles() {
        match &parse_blocks(r#"\*not bold\* and \`tick\`"#)[..] {
            [Block::Prose(runs)] => {
                assert_eq!(text_of(runs), "*not bold* and `tick`");
                assert!(runs.iter().all(|r| r.tag == "assistant"));
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks("a **b** c *d* e ***f*** g `h`")[..] {
            [Block::Prose(runs)] => {
                let tags: Vec<_> = runs.iter().map(|r| (r.tag, r.text.as_str())).collect();
                assert_eq!(
                    tags,
                    vec![
                        ("assistant", "a "),
                        ("md-bold", "b"),
                        ("assistant", " c "),
                        ("md-italic", "d"),
                        ("assistant", " e "),
                        ("md-bold-italic", "f"),
                        ("assistant", " g "),
                        ("md-inline-code", "h"),
                    ]
                );
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks("~~gone~~ ==mark==")[..] {
            [Block::Prose(runs)] => {
                assert_eq!(runs[0].tag, "md-strike");
                assert_eq!(runs[0].text, "gone");
                assert_eq!(runs[2].tag, "md-highlight");
                assert_eq!(runs[2].text, "mark");
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks("[hi](https://x.com)")[..] {
            [Block::Prose(runs)] => {
                assert_eq!(runs.len(), 1);
                assert_eq!(runs[0].tag, "md-link");
                assert_eq!(runs[0].text, "hi");
                assert_eq!(runs[0].link.as_deref(), Some("https://x.com"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bare_urls() {
        match &parse_blocks("see https://example.com/path.")[..] {
            [Block::Prose(runs)] => {
                assert_eq!(runs[0].text, "see ");
                assert_eq!(runs[1].tag, "md-link");
                assert_eq!(runs[1].text, "https://example.com/path");
                assert_eq!(runs[1].link.as_deref(), Some("https://example.com/path"));
                assert_eq!(runs[2].text, ".");
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks("http://localhost:8080/a")[..] {
            [Block::Prose(runs)] => {
                assert_eq!(runs[0].tag, "md-link");
                assert_eq!(runs[0].text, "http://localhost:8080/a");
            }
            other => panic!("{other:?}"),
        }
        // incomplete scheme stays literal
        match &parse_blocks("http://")[..] {
            [Block::Prose(runs)] => assert_eq!(runs[0].tag, "assistant"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn crlf_tolerated() {
        let blocks = parse_blocks("# Title\r\n\r\n- item\r\n> q\r\n");
        assert_eq!(kinds_of(&blocks), ["heading", "list", "quote"]);
        match &blocks[0] {
            Block::Heading { level: 1, runs } => assert_eq!(text_of(runs), "Title"),
            other => panic!("{other:?}"),
        }
    }

    fn kinds_of(blocks: &[Block]) -> Vec<&'static str> {
        blocks
            .iter()
            .map(|b| match b {
                Block::Prose(_) => "prose",
                Block::Heading { .. } => "heading",
                Block::ListItem { .. } => "list",
                Block::Quote { .. } => "quote",
                Block::Rule => "rule",
                Block::Table { .. } => "table",
                Block::Code { .. } => "code",
                Block::Advisory { .. } => "advisory",
                Block::Image { .. } => "image",
            })
            .collect()
    }

    #[test]
    fn unclosed_fence() {
        match &parse_blocks("```python\nprint(1)")[..] {
            [Block::Code { lang, code }] => {
                assert_eq!(lang, "python");
                assert_eq!(code, "print(1)");
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks("```\nfoo\nbar\n```")[..] {
            [Block::Code { lang, code }] => {
                assert!(lang.is_empty());
                assert_eq!(code, "foo\nbar");
            }
            other => panic!("{other:?}"),
        }
        match &parse_blocks("```rust\n")[..] {
            [Block::Code { lang, code }] => {
                assert_eq!(lang, "rust");
                assert_eq!(code, "");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn partial_inline_markers_stay_literal() {
        for src in [
            "hello *world",
            "hello **world",
            "hello ***world",
            "hello `code",
            "hello ~~strike",
            "hello ==mark",
            "hello [link](nope",
            "a ** b **",
        ] {
            match &parse_blocks(src)[..] {
                [Block::Prose(runs)] => {
                    assert_eq!(text_of(runs), src, "{src}");
                    assert!(
                        runs.iter().all(|r| r.tag == "assistant"),
                        "{src}: {runs:?}"
                    );
                }
                other => panic!("{src}: {other:?}"),
            }
        }
    }

    #[test]
    fn diff_line_tagging() {
        match &parse_blocks("+added\n-removed")[..] {
            [Block::Prose(runs)] => {
                assert_eq!(runs[0].tag, "diff-add");
                assert_eq!(runs[0].text, "+added");
                assert_eq!(runs[2].tag, "diff-remove");
                assert_eq!(runs[2].text, "-removed");
            }
            other => panic!("{other:?}"),
        }
        // list, not diff
        match &parse_blocks("- item")[..] {
            [Block::ListItem { .. }] => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn prose_paragraphs_and_inline_in_heading() {
        assert_eq!(kinds("a\n\nb"), ["prose", "prose"]);
        match &parse_blocks("hello\nworld")[..] {
            [Block::Prose(runs)] => assert_eq!(text_of(runs), "hello\nworld"),
            other => panic!("{other:?}"),
        }
        match &parse_blocks("# **Hi**")[..] {
            [Block::Heading { level: 1, runs }] => {
                assert_eq!(runs[0].tag, "md-bold");
                assert_eq!(runs[0].text, "Hi");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn never_panics_on_partial_fragments() {
        for src in [
            "`",
            "*",
            "**",
            "~~",
            "==",
            "[",
            "](",
            "![",
            "```",
            "<advisory",
            "<advisory severity=\"",
            "|",
            "| --",
            ">",
            "#",
            "-",
            "+",
            "\\",
            "http://",
            "https://",
            "&lt;advisory",
        ] {
            let _ = parse_blocks(src);
        }
    }
}
