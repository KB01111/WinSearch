//! `.ipynb` Jupyter Notebook parser for `ws-code`.
//!
//! Rules (P5.2):
//! - Extracts `code` and `markdown` cell `source` lines ONLY.
//! - **Never** reads or indexes cell `outputs` (`output_type`, `data`, `text`,
//!   `traceback`, base64 images, etc.) so notebook execution dumps never
//!   pollute code search or inflate `code.db`.
//! - Detects notebook language from `metadata.kernelspec.language` or
//!   `metadata.language_info.name` (defaulting to `"python"`), with per-cell
//!   magic overrides (`%%sql`, `%%bash`, `%%sh`, `%%html`, `%%javascript`,
//!   `%%js`, `%%ruby`, `%%r`).
//! - Preserves deterministic 1-based virtual line ranges across cells and runs
//!   symbol + chunk extraction on code cells.

use serde::Deserialize;

use crate::parse::{parse_source_with_offset, ParsedCodeFile, ParsedCodeUnit};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookCell {
    pub cell_index: usize,
    pub cell_type: String,
    pub language: String,
    pub start_line: u32,
    pub end_line: u32,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedNotebook {
    pub default_language: String,
    pub cells: Vec<NotebookCell>,
    pub parsed: ParsedCodeFile,
}

#[derive(Debug, Deserialize)]
struct RawNotebook {
    #[serde(default)]
    metadata: RawMetadata,
    #[serde(default)]
    cells: Vec<RawCell>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMetadata {
    #[serde(default)]
    kernelspec: Option<RawKernelSpec>,
    #[serde(default)]
    language_info: Option<RawLanguageInfo>,
}

#[derive(Debug, Deserialize)]
struct RawKernelSpec {
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawLanguageInfo {
    #[serde(default)]
    name: Option<String>,
}

/// Note: `outputs` is intentionally omitted from `RawCell` so serde skips cell
/// outputs entirely during deserialization.
#[derive(Debug, Deserialize)]
struct RawCell {
    #[serde(default)]
    cell_type: String,
    #[serde(default)]
    source: RawSource,
    #[serde(default)]
    metadata: Option<RawCellMetadata>,
}

#[derive(Debug, Default, Deserialize)]
struct RawCellMetadata {
    #[serde(default)]
    language: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawSource {
    Lines(Vec<String>),
    Single(String),
}

impl Default for RawSource {
    fn default() -> Self {
        Self::Lines(Vec::new())
    }
}

impl RawSource {
    fn into_text(self) -> String {
        match self {
            Self::Lines(lines) => lines.concat(),
            Self::Single(s) => s,
        }
    }
}

/// Parses a `.ipynb` JSON string into cells (without outputs), per-cell
/// languages, symbol-aware chunks, and extracted symbols with preserved line
/// numbers.
pub fn parse_notebook(raw_json: &str) -> Result<ParsedNotebook, serde_json::Error> {
    let raw: RawNotebook = serde_json::from_str(raw_json)?;
    let default_language = detect_notebook_language(&raw.metadata);

    let mut cells = Vec::new();
    let mut chunks = Vec::new();
    let mut symbols = Vec::new();
    let mut current_line = 1u32;

    for (cell_index, raw_cell) in raw.cells.into_iter().enumerate() {
        let cell_type = raw_cell.cell_type.to_ascii_lowercase();
        if cell_type != "code" && cell_type != "markdown" {
            continue;
        }
        let source = raw_cell.source.into_text();
        if source.trim().is_empty() {
            continue;
        }

        let line_count = source.lines().count().max(1) as u32;
        let start_line = current_line;
        let end_line = start_line + line_count - 1;
        current_line = end_line + 1;

        let cell_lang = if cell_type == "markdown" {
            "markdown".to_string()
        } else {
            detect_cell_language(
                &source,
                raw_cell
                    .metadata
                    .as_ref()
                    .and_then(|m| m.language.as_deref()),
                &default_language,
            )
        };

        if cell_type == "code" {
            let cell_parsed = parse_source_with_offset(&source, &cell_lang, start_line);
            for mut ch in cell_parsed.chunks {
                if ch.symbol_name.is_empty() {
                    ch.symbol_name = format!("cell_{}", cell_index + 1);
                    ch.symbol_kind = format!("notebook_cell:{cell_lang}");
                }
                chunks.push(ch);
            }
            symbols.extend(cell_parsed.symbols);
        } else {
            chunks.push(ParsedCodeUnit {
                start_line,
                end_line,
                symbol_name: format!("cell_{}", cell_index + 1),
                symbol_kind: "markdown_cell".to_string(),
                text: source.clone(),
            });
        }

        cells.push(NotebookCell {
            cell_index,
            cell_type,
            language: cell_lang,
            start_line,
            end_line,
            source,
        });
    }

    Ok(ParsedNotebook {
        default_language: default_language.clone(),
        cells,
        parsed: ParsedCodeFile {
            language: "notebook".to_string(),
            chunks,
            symbols,
        },
    })
}

fn detect_notebook_language(meta: &RawMetadata) -> String {
    if let Some(lang) = meta
        .language_info
        .as_ref()
        .and_then(|li| li.name.as_deref())
        .filter(|s| !s.trim().is_empty())
    {
        return normalize_lang(lang);
    }
    if let Some(ks) = meta.kernelspec.as_ref() {
        if let Some(lang) = ks.language.as_deref().filter(|s| !s.trim().is_empty()) {
            return normalize_lang(lang);
        }
        if let Some(name) = ks.name.as_deref().filter(|s| !s.trim().is_empty()) {
            return normalize_lang(name);
        }
    }
    "python".to_string()
}

fn detect_cell_language(
    source: &str,
    cell_meta_lang: Option<&str>,
    default_lang: &str,
) -> String {
    if let Some(l) = cell_meta_lang.filter(|s| !s.trim().is_empty()) {
        return normalize_lang(l);
    }
    if let Some(first_line) = source.lines().find(|l| !l.trim().is_empty()) {
        let t = first_line.trim();
        if let Some(magic) = t.strip_prefix("%%") {
            let magic_cmd = magic.split_whitespace().next().unwrap_or("");
            if !magic_cmd.is_empty() {
                return normalize_lang(magic_cmd);
            }
        }
    }
    default_lang.to_string()
}

fn normalize_lang(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "py" | "python" | "python3" | "ipython" => "python".into(),
        "rs" | "rust" | "evcxr" => "rust".into(),
        "js" | "javascript" | "node" => "javascript".into(),
        "ts" | "typescript" | "deno" => "typescript".into(),
        "sh" | "bash" | "shell" => "bash".into(),
        "ps1" | "pwsh" | "powershell" => "powershell".into(),
        "c#" | "csharp" | "dotnet-csharp" => "csharp".into(),
        "c++" | "cpp" => "cpp".into(),
        "sql" => "sql".into(),
        "html" => "html".into(),
        "css" => "css".into(),
        "ruby" | "rb" => "ruby".into(),
        "go" | "golang" => "go".into(),
        other => other.to_string(),
    }
}
