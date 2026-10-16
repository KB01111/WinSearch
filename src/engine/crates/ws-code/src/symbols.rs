//! Language-aware symbol extraction (definitions vs. references/call sites) for
//! all 20 supported languages in `ws-code`:
//! Rust, C, C++, C#, Python, JavaScript, TypeScript, Go, Java, Kotlin, Swift,
//! PHP, Ruby, SQL, PowerShell, Bash, HTML, CSS, YAML, and TOML.

use crate::model::SymbolRole;

/// One extracted symbol occurrence (definition or reference) with 1-based line
/// numbers and its cleaned source signature line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedSymbol {
    pub name: String,
    pub kind: String,
    pub role: SymbolRole,
    pub line: u32,
    pub end_line: u32,
    pub signature: String,
}

/// Supported grammar families for structural parsing and symbol extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrammarKind {
    Rust,
    C,
    Cpp,
    CSharp,
    Python,
    JavaScript,
    TypeScript,
    Go,
    Java,
    Kotlin,
    Swift,
    Php,
    Ruby,
    Sql,
    PowerShell,
    Bash,
    Html,
    Css,
    Yaml,
    Toml,
    FallbackText,
}

impl GrammarKind {
    pub fn from_language(lang: &str) -> Self {
        match lang.to_ascii_lowercase().as_str() {
            "rust" => Self::Rust,
            "c" => Self::C,
            "cpp" | "c++" => Self::Cpp,
            "csharp" | "c#" => Self::CSharp,
            "python" => Self::Python,
            "javascript" | "js" => Self::JavaScript,
            "typescript" | "ts" => Self::TypeScript,
            "go" => Self::Go,
            "java" => Self::Java,
            "kotlin" => Self::Kotlin,
            "swift" => Self::Swift,
            "php" => Self::Php,
            "ruby" => Self::Ruby,
            "sql" => Self::Sql,
            "powershell" => Self::PowerShell,
            "bash" | "sh" | "shell" => Self::Bash,
            "html" | "xml" => Self::Html,
            "css" => Self::Css,
            "yaml" => Self::Yaml,
            "toml" => Self::Toml,
            _ => Self::FallbackText,
        }
    }

    pub fn uses_brace_blocks(self) -> bool {
        matches!(
            self,
            Self::Rust
                | Self::C
                | Self::Cpp
                | Self::CSharp
                | Self::JavaScript
                | Self::TypeScript
                | Self::Go
                | Self::Java
                | Self::Kotlin
                | Self::Swift
                | Self::Php
                | Self::PowerShell
                | Self::Bash
                | Self::Css
        )
    }

    pub fn uses_indentation_blocks(self) -> bool {
        matches!(self, Self::Python | Self::Yaml)
    }
}

/// Extracts all symbol definitions and references from `source` for `language`.
/// `base_line` is `1` for normal files, or the 1-based starting line offset
/// when extracting from a notebook cell.
pub fn extract_symbols(source: &str, language: &str) -> Vec<ExtractedSymbol> {
    extract_symbols_with_offset(source, language, 1)
}

pub fn extract_symbols_with_offset(
    source: &str,
    language: &str,
    base_line: u32,
) -> Vec<ExtractedSymbol> {
    let grammar = GrammarKind::from_language(language);
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut lex_state = LexState::default();
    let clean_lines: Vec<String> = lines
        .iter()
        .map(|&raw_line| strip_line_comments(raw_line, grammar, &mut lex_state))
        .collect();

    for (idx, &raw_line) in lines.iter().enumerate() {
        let line_no = base_line + idx as u32;
        let code_line = &clean_lines[idx];
        let trimmed = code_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // 1. Check for a top-level or member definition on this line.
        let mut defined_name_on_line: Option<String> = None;
        if let Some((name, kind)) = match_definition(trimmed, raw_line, grammar) {
            let end_line =
                compute_definition_end_line_clean(&lines, &clean_lines, idx, grammar, base_line);
            defined_name_on_line = Some(name.clone());
            out.push(ExtractedSymbol {
                name,
                kind,
                role: SymbolRole::Definition,
                line: line_no,
                end_line,
                signature: clean_signature(raw_line),
            });
        }

        // 2. Extract references / call sites on this line (excluding the
        // identifier that was just declared on this same line).
        for (ref_name, ref_kind) in
            extract_references_on_line(trimmed, raw_line, grammar, defined_name_on_line.as_deref())
        {
            out.push(ExtractedSymbol {
                name: ref_name,
                kind: ref_kind,
                role: SymbolRole::Reference,
                line: line_no,
                end_line: line_no,
                signature: clean_signature(raw_line),
            });
        }
    }

    out
}

fn clean_signature(raw: &str) -> String {
    let s = raw.trim();
    if s.chars().count() > 160 {
        s.chars().take(160).collect()
    } else {
        s.to_string()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct LexState {
    in_block_comment: bool,
    in_multiline_str: Option<MultilineQuote>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MultilineQuote {
    TripleDouble,
    TripleSingle,
    Backtick,
}

/// Strips single-line and C-style `/* ... */` comments as well as string
/// literals (including multiline `"""`, `'''`, and `` ` `` literals) so comment
/// or literal words/braces are never mistaken for definitions, block ends, or
/// call sites.
fn strip_line_comments(line: &str, grammar: GrammarKind, state: &mut LexState) -> String {
    let supports_slash_comments = matches!(
        grammar,
        GrammarKind::Rust
            | GrammarKind::C
            | GrammarKind::Cpp
            | GrammarKind::CSharp
            | GrammarKind::JavaScript
            | GrammarKind::TypeScript
            | GrammarKind::Go
            | GrammarKind::Java
            | GrammarKind::Kotlin
            | GrammarKind::Swift
            | GrammarKind::Php
            | GrammarKind::Css
    );
    let supports_hash_comments = matches!(
        grammar,
        GrammarKind::Python
            | GrammarKind::Ruby
            | GrammarKind::PowerShell
            | GrammarKind::Bash
            | GrammarKind::Yaml
            | GrammarKind::Toml
            | GrammarKind::Php
    );
    let supports_sql_comments = matches!(grammar, GrammarKind::Sql);
    let supports_triple_quotes = matches!(
        grammar,
        GrammarKind::Python
            | GrammarKind::Kotlin
            | GrammarKind::Swift
            | GrammarKind::Toml
    );
    let supports_backtick_strings = matches!(
        grammar,
        GrammarKind::JavaScript | GrammarKind::TypeScript | GrammarKind::Go
    );

    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut in_str: Option<u8> = None;

    while i < bytes.len() {
        if state.in_block_comment {
            if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                state.in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(mq) = state.in_multiline_str {
            match mq {
                MultilineQuote::TripleDouble => {
                    if i + 2 < bytes.len()
                        && bytes[i] == b'"'
                        && bytes[i + 1] == b'"'
                        && bytes[i + 2] == b'"'
                    {
                        state.in_multiline_str = None;
                        out.push_str("   ");
                        i += 3;
                    } else if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        out.push_str("  ");
                        i += 2;
                    } else {
                        out.push(' ');
                        i += 1;
                    }
                    continue;
                }
                MultilineQuote::TripleSingle => {
                    if i + 2 < bytes.len()
                        && bytes[i] == b'\''
                        && bytes[i + 1] == b'\''
                        && bytes[i + 2] == b'\''
                    {
                        state.in_multiline_str = None;
                        out.push_str("   ");
                        i += 3;
                    } else if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        out.push_str("  ");
                        i += 2;
                    } else {
                        out.push(' ');
                        i += 1;
                    }
                    continue;
                }
                MultilineQuote::Backtick => {
                    if bytes[i] == b'`' {
                        state.in_multiline_str = None;
                        out.push(' ');
                        i += 1;
                    } else if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        out.push_str("  ");
                        i += 2;
                    } else {
                        out.push(' ');
                        i += 1;
                    }
                    continue;
                }
            }
        }
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(' ');
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            out.push(' ');
            i += 1;
            continue;
        }
        if supports_triple_quotes && i + 2 < bytes.len() {
            if b == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                state.in_multiline_str = Some(MultilineQuote::TripleDouble);
                out.push_str("   ");
                i += 3;
                continue;
            }
            if b == b'\'' && bytes[i + 1] == b'\'' && bytes[i + 2] == b'\'' {
                state.in_multiline_str = Some(MultilineQuote::TripleSingle);
                out.push_str("   ");
                i += 3;
                continue;
            }
        }
        if supports_backtick_strings && b == b'`' {
            state.in_multiline_str = Some(MultilineQuote::Backtick);
            out.push(' ');
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            in_str = Some(b);
            out.push(' ');
            i += 1;
            continue;
        }
        if supports_slash_comments && i + 1 < bytes.len() {
            if bytes[i] == b'/' && bytes[i + 1] == b'/' {
                break;
            }
            if bytes[i] == b'/' && bytes[i + 1] == b'*' {
                state.in_block_comment = true;
                i += 2;
                continue;
            }
        }
        if supports_hash_comments && b == b'#' {
            break;
        }
        if supports_sql_comments && i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-'
        {
            break;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

/// Computes the 1-based inclusive `end_line` of a definition starting at `start_idx`.
pub fn compute_definition_end_line(
    lines: &[&str],
    start_idx: usize,
    grammar: GrammarKind,
    base_line: u32,
) -> u32 {
    let mut lex_state = LexState::default();
    let clean_lines: Vec<String> = lines
        .iter()
        .map(|&l| strip_line_comments(l, grammar, &mut lex_state))
        .collect();
    compute_definition_end_line_clean(lines, &clean_lines, start_idx, grammar, base_line)
}

fn compute_definition_end_line_clean(
    lines: &[&str],
    clean_lines: &[String],
    start_idx: usize,
    grammar: GrammarKind,
    base_line: u32,
) -> u32 {
    let start_line_no = base_line + start_idx as u32;
    if grammar.uses_brace_blocks() {
        let mut depth = 0i32;
        let mut seen_open = false;
        for (offset, l) in clean_lines[start_idx..].iter().enumerate().take(200) {
            for ch in l.chars() {
                if ch == '{' {
                    depth += 1;
                    seen_open = true;
                } else if ch == '}' {
                    depth -= 1;
                    if seen_open && depth <= 0 {
                        return base_line + (start_idx + offset) as u32;
                    }
                }
            }
            if !seen_open && l.trim_end().ends_with(';') {
                return base_line + (start_idx + offset) as u32;
            }
        }
        return start_line_no;
    }

    if grammar.uses_indentation_blocks() {
        let base_indent = leading_indent(lines[start_idx]);
        let mut last_non_empty = start_idx;
        for (offset, &l) in lines[start_idx + 1..].iter().enumerate().take(200) {
            let t = l.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let ind = leading_indent(l);
            if ind <= base_indent {
                break;
            }
            last_non_empty = start_idx + 1 + offset;
        }
        return base_line + last_non_empty as u32;
    }

    if matches!(grammar, GrammarKind::Ruby) {
        let mut depth = 1i32;
        for (offset, l) in clean_lines[start_idx + 1..].iter().enumerate().take(200) {
            let t = l.trim();
            if is_ruby_block_opener(t) {
                depth += 1;
            } else if t == "end" || t.starts_with("end ") {
                depth -= 1;
                if depth <= 0 {
                    return base_line + (start_idx + 1 + offset) as u32;
                }
            }
        }
    }

    if matches!(grammar, GrammarKind::Sql) {
        for (offset, l) in clean_lines[start_idx..].iter().enumerate().take(100) {
            if l.contains(';') {
                return base_line + (start_idx + offset) as u32;
            }
        }
    }

    start_line_no
}

fn is_ruby_block_opener(t: &str) -> bool {
    let starts_block = t.starts_with("def ")
        || t == "def"
        || t.starts_with("class ")
        || t.starts_with("module ")
        || t.starts_with("if ")
        || t.starts_with("unless ")
        || t.starts_with("case ")
        || t == "case"
        || t.starts_with("while ")
        || t.starts_with("until ")
        || t.starts_with("for ")
        || t == "begin"
        || t.starts_with("begin ");
    let has_do_block = t.ends_with(" do") || (t.contains(" do |") && t.ends_with('|'));
    starts_block || has_do_block
}

fn leading_indent(line: &str) -> usize {
    line.chars()
        .take_while(|&c| c == ' ' || c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// Detects whether `trimmed` (with `raw_line` for languages where strings/keys
/// matter like YAML/TOML/HTML/CSS) declares a symbol definition.
fn match_definition(
    trimmed: &str,
    raw_line: &str,
    grammar: GrammarKind,
) -> Option<(String, String)> {
    match grammar {
        GrammarKind::Rust => match_rust_def(trimmed),
        GrammarKind::C | GrammarKind::Cpp => match_c_cpp_def(trimmed),
        GrammarKind::CSharp => match_csharp_def(trimmed),
        GrammarKind::Python => match_python_def(trimmed),
        GrammarKind::JavaScript | GrammarKind::TypeScript => match_js_ts_def(trimmed),
        GrammarKind::Go => match_go_def(trimmed),
        GrammarKind::Java => match_java_def(trimmed),
        GrammarKind::Kotlin => match_kotlin_def(trimmed),
        GrammarKind::Swift => match_swift_def(trimmed),
        GrammarKind::Php => match_php_def(trimmed),
        GrammarKind::Ruby => match_ruby_def(trimmed),
        GrammarKind::Sql => match_sql_def(trimmed),
        GrammarKind::PowerShell => match_powershell_def(trimmed),
        GrammarKind::Bash => match_bash_def(trimmed),
        GrammarKind::Html => match_html_def(raw_line),
        GrammarKind::Css => match_css_def(raw_line),
        GrammarKind::Yaml => match_yaml_def(raw_line),
        GrammarKind::Toml => match_toml_def(raw_line),
        GrammarKind::FallbackText => None,
    }
}

fn strip_modifiers<'a>(mut s: &'a str, mods: &[&str]) -> &'a str {
    loop {
        s = s.trim_start();
        let mut matched = false;
        // Strip leading attributes/annotations like `@Override` or `#[inline]`
        if s.starts_with('@') {
            if let Some((_, rest)) = s.split_once(char::is_whitespace) {
                s = rest;
                continue;
            }
        }
        for &m in mods {
            if let Some(rest) = s.strip_prefix(m) {
                if rest.starts_with(char::is_whitespace) || rest.starts_with('(') {
                    if rest.starts_with('(') {
                        // e.g. `pub(crate)`
                        if let Some(close) = rest.find(')') {
                            s = &rest[close + 1..];
                            matched = true;
                            break;
                        }
                    } else {
                        s = rest;
                        matched = true;
                        break;
                    }
                }
            }
        }
        if !matched {
            return s.trim_start();
        }
    }
}

fn take_ident(s: &str) -> Option<String> {
    let s = s.trim_start();
    let s = s.strip_prefix('$').unwrap_or(s);
    let mut chars = s.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let mut out = String::new();
    out.push(first);
    for c in chars {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else {
            break;
        }
    }
    Some(out)
}

fn after_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(kw)?;
    if rest.starts_with(char::is_whitespace) || rest.starts_with('<') {
        Some(rest.trim_start())
    } else {
        None
    }
}

fn skip_leading_generics(mut s: &str) -> &str {
    s = s.trim_start();
    if s.starts_with('<') {
        let mut depth = 0i32;
        for (i, c) in s.char_indices() {
            if c == '<' {
                depth += 1;
            } else if c == '>' {
                depth -= 1;
                if depth == 0 {
                    return s[i + 1..].trim_start();
                }
            }
        }
    }
    s
}

fn match_rust_def(line: &str) -> Option<(String, String)> {
    let s = strip_modifiers(
        line,
        &["pub", "async", "unsafe", "extern", "default"],
    );
    let s_for_fn = strip_modifiers(s, &["const", "async", "unsafe", "extern"]);
    if let Some(rest) = after_keyword(s_for_fn, "fn") {
        if let Some(id) = take_ident(rest) {
            return Some((id, "function".into()));
        }
    }
    for (kw, kind) in [
        ("struct", "struct"),
        ("enum", "enum"),
        ("trait", "trait"),
        ("type", "type"),
        ("mod", "module"),
        ("const", "constant"),
        ("static", "static"),
    ] {
        if let Some(rest) = after_keyword(s, kw) {
            if let Some(id) = take_ident(rest) {
                return Some((id, kind.into()));
            }
        }
    }
    if let Some(rest) = after_keyword(s, "impl") {
        let rest = skip_leading_generics(rest);
        // `impl Trait for Type` -> take Type or Trait
        if let Some(pos) = rest.find(" for ") {
            if let Some(id) = take_ident(&rest[pos + 5..]) {
                return Some((id, "impl".into()));
            }
        }
        if let Some(id) = take_ident(rest) {
            return Some((id, "impl".into()));
        }
    }
    None
}

fn match_c_cpp_def(line: &str) -> Option<(String, String)> {
    let s = strip_modifiers(
        line,
        &[
            "template", "inline", "static", "virtual", "explicit", "constexpr", "extern", "friend",
        ],
    );
    let s = skip_leading_generics(s);
    for (kw, kind) in [
        ("struct", "struct"),
        ("class", "class"),
        ("enum class", "enum"),
        ("enum", "enum"),
        ("namespace", "namespace"),
    ] {
        if let Some(rest) = after_keyword(s, kw) {
            if let Some(id) = take_ident(rest) {
                if !s.trim_end().ends_with(';') || s.contains('{') {
                    return Some((id, kind.into()));
                }
            }
        }
    }
    // C/C++ function definition: `<ret_type> <name>(...) {`
    if (s.ends_with('{') || s.contains(") {") || s.contains("){") || s.contains(") const"))
        && !is_control_statement(s)
    {
        if let Some(paren) = s.find('(') {
            let before = s[..paren].trim_end();
            // Must have a return type token before the function name
            if before.contains(char::is_whitespace) || before.contains('*') || before.contains(':')
            {
                let last_tok = before
                    .rsplit(|c: char| c.is_whitespace() || c == '*' || c == '&' || c == ':')
                    .find(|t| !t.is_empty())?;
                if let Some(id) = take_ident(last_tok) {
                    if !is_keyword(&id) {
                        return Some((id, "function".into()));
                    }
                }
            }
        }
    }
    None
}

fn match_csharp_def(line: &str) -> Option<(String, String)> {
    let s = strip_modifiers(
        line,
        &[
            "public",
            "private",
            "protected",
            "internal",
            "static",
            "async",
            "virtual",
            "override",
            "abstract",
            "sealed",
            "partial",
            "readonly",
        ],
    );
    for (kw, kind) in [
        ("class", "class"),
        ("interface", "interface"),
        ("struct", "struct"),
        ("record", "record"),
        ("enum", "enum"),
        ("namespace", "namespace"),
    ] {
        if let Some(rest) = after_keyword(s, kw) {
            if let Some(id) = take_ident(rest) {
                return Some((id, kind.into()));
            }
        }
    }
    match_typed_method_def(s)
}

fn match_python_def(line: &str) -> Option<(String, String)> {
    let s = strip_modifiers(line, &["async"]);
    if let Some(rest) = after_keyword(s, "def") {
        if let Some(id) = take_ident(rest) {
            return Some((id, "function".into()));
        }
    }
    if let Some(rest) = after_keyword(s, "class") {
        if let Some(id) = take_ident(rest) {
            return Some((id, "class".into()));
        }
    }
    None
}

fn match_js_ts_def(line: &str) -> Option<(String, String)> {
    let s = strip_modifiers(
        line,
        &[
            "export",
            "default",
            "declare",
            "async",
            "public",
            "private",
            "protected",
            "readonly",
            "static",
            "abstract",
        ],
    );
    for (kw, kind) in [
        ("function", "function"),
        ("class", "class"),
        ("interface", "interface"),
        ("type", "type"),
        ("enum", "enum"),
    ] {
        if let Some(rest) = after_keyword(s, kw) {
            let rest = rest.strip_prefix('*').map(|r| r.trim_start()).unwrap_or(rest);
            if let Some(id) = take_ident(rest) {
                return Some((id, kind.into()));
            }
        }
    }
    // `const foo = (...) =>` or `const foo = function`
    for kw in ["const", "let", "var"] {
        if let Some(rest) = after_keyword(s, kw) {
            if let Some(id) = take_ident(rest) {
                if rest.contains("=>") || rest.contains("function") {
                    return Some((id, "function".into()));
                }
            }
        }
    }
    None
}

fn match_go_def(line: &str) -> Option<(String, String)> {
    if let Some(rest) = after_keyword(line, "func") {
        // Method receiver `func (s *Server) Handle(...)`
        let rest = if rest.starts_with('(') {
            rest.find(')').map(|i| rest[i + 1..].trim_start())?
        } else {
            rest
        };
        if let Some(id) = take_ident(rest) {
            return Some((id, "function".into()));
        }
    }
    if let Some(rest) = after_keyword(line, "type") {
        if let Some(id) = take_ident(rest) {
            let kind = if rest.contains("interface") {
                "interface"
            } else if rest.contains("struct") {
                "struct"
            } else {
                "type"
            };
            return Some((id, kind.into()));
        }
    }
    None
}

fn match_java_def(line: &str) -> Option<(String, String)> {
    let s = strip_modifiers(
        line,
        &[
            "public",
            "private",
            "protected",
            "static",
            "final",
            "abstract",
            "synchronized",
            "default",
        ],
    );
    let s = skip_leading_generics(s);
    for (kw, kind) in [
        ("class", "class"),
        ("interface", "interface"),
        ("enum", "enum"),
        ("record", "record"),
    ] {
        if let Some(rest) = after_keyword(s, kw) {
            if let Some(id) = take_ident(rest) {
                return Some((id, kind.into()));
            }
        }
    }
    match_typed_method_def(s)
}

fn match_kotlin_def(line: &str) -> Option<(String, String)> {
    let s = strip_modifiers(
        line,
        &[
            "public", "private", "protected", "internal", "open", "abstract", "override", "data",
            "sealed", "suspend", "inline", "operator",
        ],
    );
    if let Some(rest) = after_keyword(s, "fun") {
        let rest = skip_leading_generics(rest);
        if let Some(id) = take_ident(rest) {
            return Some((id, "function".into()));
        }
    }
    for (kw, kind) in [
        ("class", "class"),
        ("interface", "interface"),
        ("object", "object"),
    ] {
        if let Some(rest) = after_keyword(s, kw) {
            if let Some(id) = take_ident(rest) {
                return Some((id, kind.into()));
            }
        }
    }
    None
}

fn match_swift_def(line: &str) -> Option<(String, String)> {
    let s = strip_modifiers(
        line,
        &[
            "public",
            "private",
            "fileprivate",
            "internal",
            "open",
            "static",
            "override",
            "mutating",
            "final",
        ],
    );
    // Swift allows `class func` for overridable type methods as well as `class Foo` declarations.
    let s_for_func = strip_modifiers(s, &["class", "override", "final"]);
    if let Some(rest) = after_keyword(s_for_func, "func") {
        if let Some(id) = take_ident(rest) {
            return Some((id, "function".into()));
        }
    }
    for (kw, kind) in [
        ("struct", "struct"),
        ("class", "class"),
        ("enum", "enum"),
        ("protocol", "protocol"),
        ("actor", "actor"),
        ("extension", "extension"),
    ] {
        if let Some(rest) = after_keyword(s, kw) {
            if let Some(id) = take_ident(rest) {
                return Some((id, kind.into()));
            }
        }
    }
    None
}

fn match_php_def(line: &str) -> Option<(String, String)> {
    let s = strip_modifiers(
        line,
        &[
            "public",
            "private",
            "protected",
            "static",
            "final",
            "abstract",
            "readonly",
        ],
    );
    for (kw, kind) in [
        ("function", "function"),
        ("class", "class"),
        ("interface", "interface"),
        ("trait", "trait"),
        ("enum", "enum"),
    ] {
        if let Some(rest) = after_keyword(s, kw) {
            let rest = rest.strip_prefix('&').map(|r| r.trim_start()).unwrap_or(rest);
            if let Some(id) = take_ident(rest) {
                return Some((id, kind.into()));
            }
        }
    }
    None
}

fn match_ruby_def(line: &str) -> Option<(String, String)> {
    for (kw, kind) in [
        ("def", "function"),
        ("class", "class"),
        ("module", "module"),
    ] {
        if let Some(rest) = after_keyword(line, kw) {
            let rest = rest.strip_prefix("self.").unwrap_or(rest);
            if let Some(id) = take_ident(rest) {
                return Some((id, kind.into()));
            }
        }
    }
    None
}

fn match_sql_def(line: &str) -> Option<(String, String)> {
    let upper = line.to_ascii_uppercase();
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let upper_tokens: Vec<String> = tokens.iter().map(|t| t.to_ascii_uppercase()).collect();
    if upper.starts_with("CREATE ") {
        for (i, ut) in upper_tokens.iter().enumerate() {
            if matches!(
                ut.as_str(),
                "TABLE" | "FUNCTION" | "PROCEDURE" | "VIEW" | "INDEX" | "TRIGGER"
            ) {
                let kind = ut.to_ascii_lowercase();
                // Skip optional `IF NOT EXISTS`
                let mut name_idx = i + 1;
                if upper_tokens.get(name_idx).map(|s| s.as_str()) == Some("IF")
                    && upper_tokens.get(name_idx + 1).map(|s| s.as_str()) == Some("NOT")
                    && upper_tokens.get(name_idx + 2).map(|s| s.as_str()) == Some("EXISTS")
                {
                    name_idx += 3;
                }
                if let Some(&raw_name) = tokens.get(name_idx) {
                    let cleaned = raw_name
                        .trim_matches(|c: char| c == '`' || c == '"' || c == '[' || c == ']')
                        .split('(')
                        .next()
                        .unwrap_or("")
                        .rsplit('.')
                        .next()
                        .unwrap_or("");
                    if let Some(id) = take_ident(cleaned) {
                        return Some((id, kind));
                    }
                }
            }
        }
    }
    None
}

fn match_powershell_def(line: &str) -> Option<(String, String)> {
    let lower = line.to_ascii_lowercase();
    for (kw, kind) in [("function", "function"), ("filter", "function"), ("class", "class")] {
        if lower.starts_with(kw) {
            let rest = &line[kw.len()..];
            if rest.starts_with(char::is_whitespace) {
                if let Some(id) = take_ident(rest.trim_start()) {
                    return Some((id, kind.into()));
                }
            }
        }
    }
    None
}

fn match_bash_def(line: &str) -> Option<(String, String)> {
    if let Some(rest) = after_keyword(line, "function") {
        let cleaned = rest.split('(').next().unwrap_or(rest).trim();
        if let Some(id) = take_ident(cleaned) {
            return Some((id, "function".into()));
        }
    }
    // `foo() {` style
    if let Some(paren) = line.find("()") {
        let before = line[..paren].trim();
        let after = line[paren + 2..].trim();
        if after.starts_with('{') || after.is_empty() {
            if let Some(id) = take_ident(before) {
                if id == before {
                    return Some((id, "function".into()));
                }
            }
        }
    }
    None
}

fn match_html_def(raw_line: &str) -> Option<(String, String)> {
    // Extract `id="hero-banner"` or custom element / template definitions
    if let Some(pos) = raw_line.find("id=\"") {
        let rest = &raw_line[pos + 4..];
        if let Some(end) = rest.find('"') {
            if let Some(id) = take_ident(&rest[..end]) {
                return Some((id, "element_id".into()));
            }
        }
    }
    None
}

fn match_css_def(raw_line: &str) -> Option<(String, String)> {
    let t = raw_line.trim();
    if let Some(rest) = t.strip_prefix("@keyframes") {
        if let Some(id) = take_ident(rest) {
            return Some((id, "keyframes".into()));
        }
    }
    if t.ends_with('{') {
        let sel = t.trim_end_matches('{').trim();
        if let Some(rest) = sel.strip_prefix('.') {
            if let Some(id) = take_ident(rest) {
                return Some((id, "class_selector".into()));
            }
        } else if let Some(rest) = sel.strip_prefix('#') {
            if let Some(id) = take_ident(rest) {
                return Some((id, "id_selector".into()));
            }
        }
    }
    None
}

fn match_yaml_def(raw_line: &str) -> Option<(String, String)> {
    // Top-level YAML key (0 leading spaces) `service_name:`
    if raw_line.starts_with(' ') || raw_line.starts_with('\t') || raw_line.starts_with('-') {
        return None;
    }
    let t = raw_line.trim();
    if t.starts_with('#') || t.starts_with("---") {
        return None;
    }
    let colon = t.find(':')?;
    let key = t[..colon].trim();
    let id = take_ident(key)?;
    if id == key {
        Some((id, "key".into()))
    } else {
        None
    }
}

fn match_toml_def(raw_line: &str) -> Option<(String, String)> {
    let t = raw_line.trim();
    if t.starts_with('[') && t.contains(']') {
        let inner = t.trim_start_matches('[').split(']').next()?.trim();
        let seg = inner.rsplit('.').next()?.trim_matches('"');
        if let Some(id) = take_ident(seg) {
            return Some((id, "table".into()));
        }
    } else if !raw_line.starts_with(' ') && !raw_line.starts_with('\t') {
        if let Some(eq) = t.find('=') {
            let key = t[..eq].trim();
            if let Some(id) = take_ident(key) {
                if id == key {
                    return Some((id, "key".into()));
                }
            }
        }
    }
    None
}

fn match_typed_method_def(s: &str) -> Option<(String, String)> {
    if is_control_statement(s) || s.contains('=') && s.find('=').unwrap() < s.find('(').unwrap_or(usize::MAX) {
        return None;
    }
    if !(s.ends_with('{') || s.contains(") {") || s.contains("){") || s.contains("=>")) {
        return None;
    }
    let paren = s.find('(')?;
    let before = s[..paren].trim_end();
    if !before.contains(char::is_whitespace) {
        return None;
    }
    let last_tok = before.rsplit(char::is_whitespace).next()?;
    let id = take_ident(last_tok)?;
    if is_keyword(&id) {
        return None;
    }
    Some((id, "method".into()))
}

/// Extracts function/command/selector references on a line.
fn extract_references_on_line(
    trimmed: &str,
    raw_line: &str,
    grammar: GrammarKind,
    defined_name: Option<&str>,
) -> Vec<(String, String)> {
    let mut refs = Vec::new();

    match grammar {
        GrammarKind::PowerShell => {
            // PowerShell Verb-Noun invocations (`Invoke-BuildStep -Path ...`)
            for tok in trimmed.split(|c: char| {
                c.is_whitespace() || matches!(c, '(' | ')' | '{' | '}' | ';' | '|' | '=')
            }) {
                let tok = tok.trim();
                if tok.contains('-') && !tok.starts_with('-') {
                    if let Some(id) = take_ident(tok) {
                        if defined_name != Some(id.as_str())
                            && !id.eq_ignore_ascii_case("function")
                        {
                            refs.push((id, "call".into()));
                        }
                    }
                }
            }
        }
        GrammarKind::Bash => {
            // First command token on a non-definition line (or after `$(` / `|`)
            if defined_name.is_none() {
                for seg in trimmed.split(|c| c == '|' || c == ';') {
                    let cmd = seg
                        .trim()
                        .trim_start_matches("$(")
                        .split_whitespace()
                        .next()
                        .unwrap_or("");
                    if let Some(id) = take_ident(cmd) {
                        if id == cmd && !is_bash_builtin(&id) {
                            refs.push((id, "call".into()));
                        }
                    }
                }
            }
        }
        GrammarKind::Html => {
            // `href="#hero-banner"` or `<custom-widget>`
            if let Some(pos) = raw_line.find("href=\"#") {
                let rest = &raw_line[pos + 7..];
                if let Some(end) = rest.find('"') {
                    if let Some(id) = take_ident(&rest[..end]) {
                        refs.push((id, "id_ref".into()));
                    }
                }
            }
        }
        GrammarKind::Css => {
            // `animation: fade-in ...` or `animation-name: fade-in`
            if let Some(pos) = trimmed.find("animation") {
                let rest = &trimmed[pos..];
                if let Some(colon) = rest.find(':') {
                    let val = rest[colon + 1..].trim_start();
                    if let Some(id) = take_ident(val) {
                        if defined_name != Some(id.as_str()) {
                            refs.push((id, "keyframes_ref".into()));
                        }
                    }
                }
            }
        }
        GrammarKind::Yaml => {
            // YAML alias `*base_anchor` or value reference
            for tok in trimmed.split_whitespace() {
                if let Some(alias) = tok.strip_prefix('*') {
                    if let Some(id) = take_ident(alias) {
                        refs.push((id, "alias_ref".into()));
                    }
                }
            }
        }
        GrammarKind::Toml => {
            // Dotted table reference `[package.metadata]` or dependency key
            if !trimmed.starts_with('[') && defined_name.is_some() {
                if let Some(eq) = trimmed.find('=') {
                    let rhs = trimmed[eq + 1..].trim();
                    if let Some(pos) = rhs.find("path") {
                        let _ = pos;
                    }
                }
            }
        }
        _ => {}
    }

    // Standard `identifier(...)` call pattern across programming languages and SQL.
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' && i > 0 {
            // Walk backward over identifier chars before `(`
            let mut start = i;
            while start > 0 {
                let c = bytes[start - 1] as char;
                if c.is_ascii_alphanumeric() || c == '_' {
                    start -= 1;
                } else {
                    break;
                }
            }
            if start < i {
                let ident = &trimmed[start..i];
                if let Some(id) = take_ident(ident) {
                    if !is_keyword(&id)
                        && defined_name != Some(id.as_str())
                        && !refs.iter().any(|(n, _)| n == &id)
                    {
                        refs.push((id, "call".into()));
                    }
                }
            }
        }
        i += 1;
    }

    refs
}

fn is_control_statement(s: &str) -> bool {
    let first = s
        .split(|c: char| !c.is_ascii_alphabetic() && c != '_')
        .find(|t| !t.is_empty())
        .unwrap_or("");
    is_keyword(first)
}

fn is_keyword(ident: &str) -> bool {
    matches!(
        ident.to_ascii_lowercase().as_str(),
        "if"
            | "else"
            | "for"
            | "while"
            | "loop"
            | "match"
            | "switch"
            | "case"
            | "return"
            | "sizeof"
            | "alignof"
            | "typeof"
            | "catch"
            | "try"
            | "throw"
            | "new"
            | "delete"
            | "where"
            | "select"
            | "from"
            | "insert"
            | "into"
            | "values"
            | "update"
            | "table"
            | "primary"
            | "foreign"
            | "references"
            | "unique"
            | "check"
            | "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "fn"
            | "func"
            | "function"
            | "def"
            | "fun"
            | "class"
            | "struct"
            | "enum"
            | "interface"
            | "trait"
            | "impl"
            | "pub"
            | "super"
            | "self"
            | "this"
            | "vec"
            | "print"
            | "println"
            | "format"
            | "assert"
            | "assert_eq"
    )
}

fn is_bash_builtin(ident: &str) -> bool {
    matches!(
        ident,
        "if"
            | "then"
            | "else"
            | "elif"
            | "fi"
            | "for"
            | "in"
            | "do"
            | "done"
            | "while"
            | "case"
            | "esac"
            | "echo"
            | "printf"
            | "return"
            | "exit"
            | "local"
            | "export"
            | "readonly"
            | "set"
            | "unset"
            | "cd"
            | "pwd"
            | "true"
            | "false"
            | "test"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotation_skip_detects_method_and_handles_multibyte() {
        let src = "@Override public void run() { // café\n    work();\n}\n";
        let syms = extract_symbols(src, "java");
        assert!(
            syms.iter()
                .any(|s| s.name == "run" && s.role == SymbolRole::Definition),
            "got {syms:?}"
        );
    }

    #[test]
    fn brace_end_ignores_braces_in_strings_and_comments() {
        let src = "fn demo() {\n    let s = \"}\"; // }\n    let x = 1;\n}\n";
        let syms = extract_symbols(src, "rust");
        let def = syms
            .iter()
            .find(|s| s.name == "demo" && s.role == SymbolRole::Definition)
            .expect("demo definition");
        assert_eq!(def.line, 1);
        assert_eq!(def.end_line, 4);
    }

    #[test]
    fn ruby_end_counts_nested_blocks() {
        let src = "def compute(x)\n  if x > 0\n    items.each do |i|\n      puts(i)\n    end\n  end\n  x + 1\nend\n";
        let syms = extract_symbols(src, "ruby");
        let def = syms
            .iter()
            .find(|s| s.name == "compute" && s.role == SymbolRole::Definition)
            .expect("compute definition");
        assert_eq!(def.line, 1);
        assert_eq!(def.end_line, 8);
    }

    #[test]
    fn multiline_docstring_does_not_emit_false_symbols() {
        let src = "def real_fn():\n    \"\"\"\n    def hidden_in_docstring():\n        fake_call()\n    \"\"\"\n    return helper_call()\n";
        let syms = extract_symbols(src, "python");
        assert!(
            syms.iter()
                .any(|s| s.name == "real_fn" && s.role == SymbolRole::Definition)
        );
        assert!(
            !syms.iter().any(|s| s.name == "hidden_in_docstring"),
            "docstring code should not be indexed: {syms:?}"
        );
        assert!(
            !syms.iter().any(|s| s.name == "fake_call"),
            "docstring calls should not be indexed: {syms:?}"
        );
        assert!(
            syms.iter()
                .any(|s| s.name == "helper_call" && s.role == SymbolRole::Reference)
        );
    }

    #[test]
    fn callable_some_is_indexed_as_reference() {
        let src = "const ok = items.some(pred);\n";
        let syms = extract_symbols(src, "typescript");
        assert!(
            syms.iter()
                .any(|s| s.name == "some" && s.role == SymbolRole::Reference),
            "expected 'some' call reference, got {syms:?}"
        );
    }

    #[test]
    fn rust_const_and_const_fn_and_swift_class_and_class_func_are_indexed() {
        let rust_src = "pub const LIMIT: usize = 4;\npub const fn calc() -> usize {\n    LIMIT\n}\n";
        let rust_syms = extract_symbols(rust_src, "rust");
        assert!(
            rust_syms.iter().any(|s| s.name == "LIMIT"
                && s.kind == "constant"
                && s.role == SymbolRole::Definition),
            "expected Rust const LIMIT, got {rust_syms:?}"
        );
        assert!(
            rust_syms.iter().any(|s| s.name == "calc"
                && s.kind == "function"
                && s.role == SymbolRole::Definition),
            "expected Rust const fn calc, got {rust_syms:?}"
        );

        let swift_src = "public class Widget {\n    public class func make() {}\n}\n";
        let swift_syms = extract_symbols(swift_src, "swift");
        assert!(
            swift_syms.iter().any(|s| s.name == "Widget"
                && s.kind == "class"
                && s.role == SymbolRole::Definition),
            "expected Swift class Widget, got {swift_syms:?}"
        );
        assert!(
            swift_syms.iter().any(|s| s.name == "make"
                && s.kind == "function"
                && s.role == SymbolRole::Definition),
            "expected Swift class func make, got {swift_syms:?}"
        );
    }
}
