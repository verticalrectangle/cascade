//! Syntax coloring for fenced code blocks — Rust port of the omperator
//! SyntaxHighlighter.swift. Same ordered (tag, regex) rules; comments and
//! strings come first so later rules never recolor inside them. Rust's
//! `regex` crate has no lookahead, so rules that used `(?=…)` in Swift
//! match the trailing delimiter and are trimmed back to the last word char.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use regex::Regex;

/// One colored run of source text: `(text, tag)`.
pub type Token = (String, &'static str);

static CACHE: LazyLock<Mutex<HashMap<String, Option<(Arc<Regex>, Arc<LangSpec>)>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct Rule {
    tag: &'static str,
    fragment: String,
    /// Trim the match back to the last word char (lookahead replacement).
    trim: bool,
}

struct LangSpec {
    rules: Vec<Rule>,
}

fn r(tag: &'static str, frag: impl Into<String>) -> Rule {
    Rule {
        tag,
        fragment: frag.into(),
        trim: false,
    }
}

fn rt(tag: &'static str, frag: impl Into<String>) -> Rule {
    Rule {
        tag,
        fragment: frag.into(),
        trim: true,
    }
}

mod frag {
    pub const LINE_SLASH: &str = r"//[^\n]*";
    pub const BLOCK_C: &str = r"/\*[\s\S]*?\*/";
    pub const LINE_HASH: &str = r"#[^\n]*";
    pub const HTML_COMMENT: &str = r"<!--[\s\S]*?-->";
    pub const DQ: &str = r#""(?:\\.|[^"\\])*""#;
    pub const SQ: &str = r"'(?:\\.|[^'\\])*'";
    pub const TMPL: &str = r"`(?:\\.|[^`\\])*`";
    pub const NUM: &str = r"\b0[xX][0-9a-fA-F]+\b|\b\d[\d_]*(?:\.\d+)?(?:[eE][+-]?\d+)?[fFlLuU]*\b";
    pub const FN: &str = r"[A-Za-z_$][\w$]*\s*\(";
    pub const DEC: &str = r"@\w+";
    pub const PREPROC: &str = r"(?m)^[ \t]*#[ \t]*[a-zA-Z]+\b";

    pub fn strings() -> String {
        format!("{DQ}|{SQ}")
    }
    pub fn kw(s: &str) -> String {
        format!(r"\b(?:{s})\b")
    }
}

fn spec_for(lang: &str) -> LangSpec {
    use frag::*;
    let rules: Vec<Rule> = match lang {
        "swift" => vec![
            r("syn-comment", format!("{LINE_SLASH}|{BLOCK_C}")),
            r("syn-string", strings()),
            r("syn-number", NUM),
            r("syn-attribute", DEC),
            r("syn-keyword", kw("func|let|var|if|else|guard|for|while|switch|case|default|break|continue|return|throw|throws|rethrows|try|catch|do|defer|struct|class|enum|protocol|extension|init|deinit|self|Self|super|nil|true|false|as|is|in|where|import|public|private|fileprivate|internal|open|static|final|lazy|weak|unowned|some|any|async|await|actor|associatedtype|typealias|mutating|nonmutating|override|convenience|required|inout|indirect|repeat|fallthrough")),
            r("syn-type", kw("Int|Double|Float|String|Bool|Array|Dictionary|Set|Optional|Result|Void|URL|Data|Date|Error|Any|Codable|Hashable|Equatable|Comparable|Range|UUID")),
            rt("syn-function", FN),
        ],
        "javascript" | "js" | "mjs" | "cjs" | "typescript" | "ts" | "jsx" | "tsx" => vec![
            r("syn-comment", format!("{LINE_SLASH}|{BLOCK_C}")),
            r("syn-string", format!("{}|{TMPL}", strings())),
            r("syn-number", NUM),
            r("syn-attribute", DEC),
            r("syn-keyword", kw("var|let|const|function|return|if|else|for|while|do|switch|case|break|continue|new|delete|typeof|instanceof|void|this|class|extends|super|import|export|from|default|try|catch|finally|throw|async|await|yield|null|undefined|true|false|in|of|static|get|set|public|private|protected|readonly|interface|type|enum|namespace|as|is|implements|abstract")),
            r("syn-type", kw("string|number|boolean|any|unknown|void|never|object|symbol|bigint|Promise|Array|Map|Set|Date|Error|JSON|Math|Object|console")),
            rt("syn-function", FN),
        ],
        "python" | "py" | "py3" => vec![
            r("syn-comment", LINE_HASH),
            r("syn-string", format!(r#"(?:'''|""")[\s\S]*?(?:'''|""")|{}"#, strings())),
            r("syn-number", NUM),
            r("syn-attribute", DEC),
            r("syn-keyword", kw("def|class|return|if|elif|else|for|while|break|continue|pass|raise|try|except|finally|with|as|import|from|global|nonlocal|lambda|yield|async|await|del|in|is|not|and|or|None|True|False|assert")),
            r("syn-type", kw("int|float|str|bool|list|dict|tuple|set|frozenset|object|bytes|bytearray|range|type|complex")),
            rt("syn-function", FN),
        ],
        "bash" | "sh" | "shell" | "zsh" | "fish" => vec![
            r("syn-comment", LINE_HASH),
            r("syn-string", strings()),
            r("syn-number", NUM),
            r("syn-keyword", kw("if|then|else|elif|fi|for|do|done|while|until|case|esac|in|function|return|local|export|unset|echo|read|exit|shift|break|continue|cd|set|source|alias|trap|wait|printf")),
            rt("syn-function", FN),
        ],
        "json" | "json5" => vec![
            r("syn-string", strings()),
            r("syn-number", NUM),
            r("syn-type", kw("true|false|null")),
        ],
        "rust" | "rs" => vec![
            r("syn-comment", format!("{LINE_SLASH}|{BLOCK_C}")),
            r("syn-string", strings()),
            r("syn-number", NUM),
            r("syn-attribute", r"#\[[a-zA-Z_][\w]*\]"),
            r("syn-keyword", kw("fn|let|mut|const|static|if|else|for|while|loop|match|break|continue|return|struct|enum|trait|impl|pub|use|mod|ref|self|Self|super|as|in|where|unsafe|async|await|move|dyn|crate|extern|type|true|false")),
            r("syn-type", kw("i8|i16|i32|i64|i128|usize|u8|u16|u32|u64|u128|isize|f32|f64|bool|char|str|String|Vec|Option|Result|Box|Rc|Arc|HashMap|HashSet")),
            rt("syn-function", FN),
        ],
        "go" | "golang" => vec![
            r("syn-comment", format!("{LINE_SLASH}|{BLOCK_C}")),
            r("syn-string", format!("{}|{TMPL}", strings())),
            r("syn-number", NUM),
            r("syn-keyword", kw("func|var|const|type|struct|interface|map|chan|if|else|for|range|switch|case|default|break|continue|return|defer|go|select|package|import|fallthrough|nil|true|false")),
            r("syn-type", kw("int|int8|int16|int32|int64|uint|uint8|uint16|uint32|uint64|uintptr|string|bool|byte|rune|float32|float64|complex64|complex128|error|any")),
            rt("syn-function", FN),
        ],
        "css" => vec![
            r("syn-comment", BLOCK_C),
            r("syn-string", strings()),
            r("syn-number", format!(r"{NUM}|#[0-9a-fA-F]{{3,8}}\b")),
            r("syn-attribute", r"@[\w-]+"),
            r("syn-type", kw("px|em|rem|vh|vw|auto|none|block|flex|grid|absolute|relative|fixed|solid|dashed|inherit|initial|center|left|right|top|bottom")),
        ],
        "html" | "xml" | "svg" => vec![
            r("syn-comment", HTML_COMMENT),
            r("syn-string", strings()),
            r("syn-keyword", r"</?[a-zA-Z][\w-]*"),
            rt("syn-attribute", r"[a-zA-Z-]+\s*="),
            r("syn-number", NUM),
        ],
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "cs" | "java" | "kt" => vec![
            r("syn-comment", format!("{LINE_SLASH}|{BLOCK_C}")),
            r("syn-string", strings()),
            r("syn-number", NUM),
            r("syn-attribute", format!("{DEC}|{PREPROC}")),
            r("syn-keyword", kw("auto|break|case|const|continue|default|do|else|enum|extern|for|goto|if|inline|register|restrict|return|sizeof|static|struct|switch|typedef|union|volatile|while|alignas|alignof|and|asm|catch|class|constexpr|decltype|delete|explicit|export|false|friend|mutable|namespace|new|noexcept|nullptr|operator|private|protected|public|static_assert|template|this|throw|true|try|typename|using|virtual|NULL|override|final")),
            r("syn-type", kw("void|char|short|int|long|float|double|signed|unsigned|wchar_t|size_t|ssize_t|ptrdiff_t|intptr_t|uintptr_t|int8_t|int16_t|int32_t|int64_t|uint8_t|uint16_t|uint32_t|uint64_t|string|vector|map|set|list|deque|pair|tuple|optional|variant|unique_ptr|shared_ptr|weak_ptr|FILE|bool")),
            rt("syn-function", FN),
        ],
        "yaml" | "yml" | "toml" => vec![
            r("syn-comment", LINE_HASH),
            r("syn-string", strings()),
            r("syn-number", NUM),
            rt("syn-keyword", r"[A-Za-z_][\w-]*\s*:"),
            r("syn-type", kw("true|false|null|yes|no|on|off")),
        ],
        "markdown" | "md" => vec![
            r("syn-comment", HTML_COMMENT),
            r("syn-string", r"``[\s\S]*?``|`[^`\n]*`"),
            r("syn-attribute", r"!?\[[^\]]*\]\([^)]*\)|https?://[^\s<)\]]+"),
            r("syn-keyword", r"(?m)^#{1,6}[^\n]*"),
            r("syn-attribute", r"\*\*[^*\n]+\*\*|__[^_\n]+__|\*[^*\n]+\*|_[^_\n]+_|~~[^~\n]+~~"),
            r("syn-number", NUM),
        ],
        _ => vec![
            r("syn-comment", format!("{LINE_SLASH}|{BLOCK_C}|{LINE_HASH}")),
            r("syn-string", strings()),
            r("syn-number", NUM),
        ],
    };
    LangSpec { rules }
}

fn compiled(lang: &str) -> Option<(Arc<Regex>, Arc<LangSpec>)> {
    let key = lang.to_lowercase();
    if let Some(hit) = CACHE.lock().get(&key) {
        return hit.clone();
    }
    let spec = Arc::new(spec_for(&key));
    let pattern = spec
        .rules
        .iter()
        .map(|rule| format!("({})", rule.fragment))
        .collect::<Vec<_>>()
        .join("|");
    let entry = Regex::new(&pattern).ok().map(Arc::new).map(|re| (re, spec));
    CACHE.lock().insert(key, entry.clone());
    entry
}

fn push(tokens: &mut Vec<Token>, text: &str, tag: &'static str) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = tokens.last_mut() {
        if last.1 == tag {
            last.0.push_str(text);
            return;
        }
    }
    tokens.push((text.to_string(), tag));
}

/// Trim a match back to just past its last word char (lookahead emulation).
fn trim_to_word(s: &str) -> usize {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() || c == '_' || c == '$' || c == '-' {
            end = i + c.len_utf8();
        }
    }
    end
}

fn tokenize(code: &str, lang: &str) -> Vec<Token> {
    let Some((re, spec)) = compiled(lang) else {
        return vec![(code.to_string(), "syn-plain")];
    };
    let mut tokens = Vec::new();
    let mut cursor = 0;
    for caps in re.captures_iter(code) {
        let whole = caps.get(0).unwrap();
        let group = (1..=spec.rules.len()).find(|i| caps.get(*i).is_some());
        let Some(gi) = group else { continue };
        let rule = &spec.rules[gi - 1];
        if whole.start() > cursor {
            push(&mut tokens, &code[cursor..whole.start()], "syn-plain");
        }
        let m = whole.as_str();
        if rule.trim {
            let end = trim_to_word(m);
            push(&mut tokens, &m[..end], rule.tag);
            push(&mut tokens, &m[end..], "syn-plain");
        } else {
            push(&mut tokens, m, rule.tag);
        }
        cursor = whole.end();
    }
    if cursor < code.len() {
        push(&mut tokens, &code[cursor..], "syn-plain");
    }
    tokens
}

// ── diff ─────────────────────────────────────────────────────────────

fn diff_tag(line: &str) -> &'static str {
    if line.starts_with("+++") || line.starts_with("---") {
        return "syn-plain";
    }
    if line == "+" || line == "-" {
        return "syn-plain";
    }
    if line.starts_with('+') {
        return "diff-add";
    }
    if line.starts_with('-') {
        return "diff-remove";
    }
    "syn-plain"
}

fn diff_tokens(code: &str) -> Vec<Token> {
    code.split_inclusive('\n')
        .map(|l| (l.to_string(), diff_tag(l.trim_end_matches('\n'))))
        .collect()
}

fn looks_like_unified_diff(code: &str) -> bool {
    let mut saw_hunk = false;
    let mut saw_marker = false;
    for line in code.split('\n') {
        if line.starts_with("@@") {
            saw_hunk = true;
        } else if line.starts_with("diff --git ")
            || line.starts_with("Index: ")
            || line.starts_with("=== ")
        {
            return true;
        }
        if !saw_marker && (line.starts_with('+') || line.starts_with('-')) {
            saw_marker = true;
        }
        if saw_hunk && saw_marker {
            return true;
        }
    }
    false
}

/// Tokenize a fenced code block. Unknown languages get a light
/// comment/string/number pass; `diff`/`patch` (or anything that looks like a
/// unified diff) gets per-line +/− tinting.
pub fn highlight(code: &str, language: &str) -> Vec<Token> {
    let lang = language.to_lowercase();
    if lang == "diff" || lang == "patch" || looks_like_unified_diff(code) {
        return diff_tokens(code);
    }
    tokenize(code, &lang)
}
