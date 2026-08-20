//! Markdown-lite renderer — Rust port of the omperator MarkdownRenderer.swift.
//! Splits a transcript body into blocks: prose (flat `(text, tag)` segment
//! runs, markers stripped) and fenced code blocks. Tags map onto GtkTextTag
//! names styled per theme by the caller.

/// One styled run of rendered transcript text.
pub type Segment = (String, &'static str);

/// A rendered block of a transcript entry.
pub enum Block {
    /// Prose lines; each segment carries its tag (md-h1, md-bold, …).
    Prose(Vec<Segment>),
    /// Fenced code block: language hint + raw code.
    Code { lang: String, code: String },
}

fn is_fence_line(line: &str) -> bool {
    line.starts_with("```")
}

/// Parse a transcript body into blocks. Assistant/other roles get markdown
/// parsing; the caller handles user messages verbatim.
pub fn parse_blocks(body: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut prose: Vec<Segment> = Vec::new();
    let mut code: Option<(String, String)> = None;

    for line in body.split_inclusive('\n') {
        let text = line.strip_suffix('\n').unwrap_or(line);
        let has_nl = line.ends_with('\n');
        if is_fence_line(text) {
            match code.take() {
                // Closing fence: flush the code block. The fence line itself
                // keeps its line slot as a blank prose line (Swift parity).
                Some((lang, body_text)) => {
                    flush_prose(&mut blocks, &mut prose);
                    blocks.push(Block::Code {
                        lang,
                        code: body_text,
                    });
                    push_segment(&mut prose, "\n", "assistant");
                }
                None => {
                    flush_prose(&mut blocks, &mut prose);
                    let lang = text.trim_start_matches('`').trim().to_string();
                    code = Some((lang, String::new()));
                    push_segment(&mut prose, "\n", "assistant");
                }
            }
            continue;
        }
        if let Some((_, body_text)) = code.as_mut() {
            body_text.push_str(line);
            continue;
        }
        let mut segs = prose_line_segments(text);
        if has_nl {
            append_newline(&mut segs);
        }
        prose.extend(segs);
    }
    if let Some((lang, body_text)) = code.take() {
        flush_prose(&mut blocks, &mut prose);
        blocks.push(Block::Code {
            lang,
            code: body_text,
        });
    }
    flush_prose(&mut blocks, &mut prose);
    blocks
}

fn flush_prose(blocks: &mut Vec<Block>, prose: &mut Vec<Segment>) {
    // Keep blank spacer lines out of their own block.
    if prose.iter().any(|(t, _)| !t.trim().is_empty()) {
        blocks.push(Block::Prose(std::mem::take(prose)));
    } else {
        prose.clear();
    }
}

fn push_segment(segs: &mut Vec<Segment>, text: &str, tag: &'static str) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = segs.last_mut() {
        if last.1 == tag {
            last.0.push_str(text);
            return;
        }
    }
    segs.push((text.to_string(), tag));
}

fn append_newline(segs: &mut Vec<Segment>) {
    push_segment(segs, "\n", "assistant");
}

// ── line-level (block) parsing ───────────────────────────────────────

/// `#` / `##` / `###` at line start followed by space/tab or EOL.
fn heading_content(line: &str) -> Option<(&str, &'static str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 3 {
        return None;
    }
    let rest = &line[hashes..];
    if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    let tag = match hashes {
        1 => "md-h1",
        2 => "md-h2",
        _ => "md-h3",
    };
    Some((rest.trim_start(), tag))
}

/// A diff line: `+`/`-` first char with content after it.
fn diff_tag_for(line: &str) -> Option<&'static str> {
    if line.len() < 2 {
        return None;
    }
    if line.starts_with("- ") {
        return None; // list item, not diff
    }
    match line.as_bytes()[0] {
        b'+' => Some("diff-add"),
        b'-' => Some("diff-remove"),
        _ => None,
    }
}

/// Index just past the list marker (indent + bullet/number + space) if any.
fn list_prefix_end(line: &str) -> Option<usize> {
    let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
    let rest = &line[indent..];
    let after_marker = if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("• ")
    {
        2
    } else {
        // ordered: digits then '.' or ')' then space
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && digits <= 3 {
            let tail = &rest[digits..];
            if (tail.starts_with(". ") || tail.starts_with(") ")) && digits > 0 {
                digits + 2
            } else {
                return None;
            }
        } else {
            return None;
        }
    };
    if rest[after_marker..].is_empty() {
        return None;
    }
    Some(indent + after_marker)
}

fn prose_line_segments(line: &str) -> Vec<Segment> {
    if let Some((content, tag)) = heading_content(line) {
        if content.is_empty() {
            return Vec::new();
        }
        return vec![(content.to_string(), tag)];
    }
    if let Some(tag) = diff_tag_for(line) {
        return vec![(line.to_string(), tag)];
    }
    if let Some(rest) = line.strip_prefix("> ") {
        return inline_segments(rest, "md-quote");
    }
    if line == ">" {
        return Vec::new();
    }
    if let Some(end) = list_prefix_end(line) {
        let (prefix, body) = line.split_at(end);
        let mut segs = vec![(prefix.to_string(), "md-list")];
        segs.extend(inline_segments(body, "md-list"));
        return segs;
    }
    inline_segments(line, "assistant")
}

// ── inline span parsing ──────────────────────────────────────────────

fn is_boundary(c: char) -> bool {
    c.is_whitespace() || c.is_ascii_punctuation()
}

/// Scan a line for `` `code` ``, `***bi***`, `**b**`/`__b__`, `*i*`/`_i_`,
/// `[label](url)`. Unpaired markers stay literal.
fn inline_segments(line: &str, default_tag: &'static str) -> Vec<Segment> {
    let chars: Vec<char> = line.chars().collect();
    let mut segs: Vec<Segment> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // inline code span
        if c == '`' {
            if let Some(close) = find_char(&chars, i + 1, '`') {
                if close > i + 1 {
                    push_segment(&mut segs, &plain, default_tag);
                    plain.clear();
                    let content: String = chars[i + 1..close].iter().collect();
                    push_segment(&mut segs, &content, "md-inline-code");
                    i = close + 1;
                    continue;
                }
            }
            plain.push(c);
            i += 1;
            continue;
        }
        // links: [label](url)
        if c == '[' {
            if let Some(close_bracket) = find_char(&chars, i + 1, ']') {
                if close_bracket > i + 1
                    && chars.get(close_bracket + 1) == Some(&'(')
                {
                    if let Some(close_paren) = find_char(&chars, close_bracket + 2, ')') {
                        let label: String = chars[i + 1..close_bracket].iter().collect();
                        push_segment(&mut segs, &plain, default_tag);
                        plain.clear();
                        push_segment(&mut segs, &label, "md-link");
                        i = close_paren + 1;
                        continue;
                    }
                }
            }
            plain.push(c);
            i += 1;
            continue;
        }
        // emphasis runs of * or _
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
                    push_segment(&mut segs, &plain, default_tag);
                    plain.clear();
                    push_segment(&mut segs, &content, tag);
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
        plain.push(c);
        i += 1;
    }
    push_segment(&mut segs, &plain, default_tag);
    segs
}

fn find_char(chars: &[char], from: usize, want: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == want)
}

/// Try an emphasis span: run of exactly `len` `c` at `start..run_end`,
/// non-empty content, valid closer. Returns (content, index past closer).
fn emphasis_span(
    chars: &[char],
    c: char,
    len: usize,
    run_end: usize,
) -> Option<(String, usize)> {
    // content must start with non-whitespace
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
            // candidate closer: exactly `len` chars
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
