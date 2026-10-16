//! Symbol-aware code chunking and line-based fallback for `ws-code`.
//!
//! Supported languages (20 grammars + line-based fallback):
//! Rust, C, C++, C#, Python, JavaScript, TypeScript, Go, Java, Kotlin, Swift,
//! PHP, Ruby, SQL, PowerShell, Bash, HTML, CSS, YAML, and TOML, plus `.ipynb`
//! notebooks via [`crate::notebook::parse_notebook`].

use std::fs;
use std::path::Path;

use crate::model::{language_for_file, NewCodeChunk, NewCodeSymbol, SymbolRole};
use crate::notebook::parse_notebook;
use crate::store::{CodeStore, CodeStoreError};
use crate::symbols::{extract_symbols_with_offset, ExtractedSymbol, GrammarKind};

/// Maximum lines in one fallback text chunk when no symbol boundaries apply.
pub const FALLBACK_CHUNK_LINES: usize = 60;
/// Overlap lines between consecutive fallback chunks.
pub const FALLBACK_CHUNK_OVERLAP: usize = 10;

/// One extracted code chunk with 1-based inclusive `(start_line, end_line)`
/// range and optional enclosing symbol metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCodeUnit {
    pub start_line: u32,
    pub end_line: u32,
    pub symbol_name: String,
    pub symbol_kind: String,
    pub text: String,
}

/// Complete parse result for a source file or notebook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCodeFile {
    pub language: String,
    pub chunks: Vec<ParsedCodeUnit>,
    pub symbols: Vec<ExtractedSymbol>,
}

/// Parses `source` for `language` starting at 1-based line `1`.
pub fn parse_source(source: &str, language: &str) -> ParsedCodeFile {
    if language.eq_ignore_ascii_case("notebook") || language.eq_ignore_ascii_case("jupyter") {
        return match parse_notebook(source) {
            Ok(nb) => nb.parsed,
            Err(_) => ParsedCodeFile {
                language: language.to_string(),
                chunks: Vec::new(),
                symbols: Vec::new(),
            },
        };
    }
    parse_source_with_offset(source, language, 1)
}

/// Parses `source` for `language` with a 1-based line offset (`base_line = 1`
/// for standalone files, or the cell's starting line in a notebook).
pub fn parse_source_with_offset(
    source: &str,
    language: &str,
    base_line: u32,
) -> ParsedCodeFile {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return ParsedCodeFile {
            language: language.to_string(),
            chunks: Vec::new(),
            symbols: Vec::new(),
        };
    }

    let grammar = GrammarKind::from_language(language);
    let symbols = extract_symbols_with_offset(source, language, base_line);

    let chunks = if grammar == GrammarKind::FallbackText {
        fallback_line_chunks(&lines, base_line)
    } else {
        symbol_aware_chunks(&lines, &symbols, base_line)
    };

    ParsedCodeFile {
        language: language.to_string(),
        chunks,
        symbols,
    }
}

/// Parses a file on disk (auto-detecting language from filename/extension or
/// `.ipynb` notebook format).
pub fn parse_file(path: &Path) -> std::io::Result<ParsedCodeFile> {
    let raw = fs::read_to_string(path)?;
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let lang = language_for_file(&name, &ext);
    Ok(parse_source(&raw, lang))
}

/// Persists extracted symbols and chunks for `file_id` into `store` (`code.db`).
/// Returns the inserted/updated chunk ids in chunk order.
pub fn persist_parsed_file(
    store: &mut CodeStore,
    file_id: i64,
    parsed: &ParsedCodeFile,
) -> Result<Vec<i64>, CodeStoreError> {
    let sym_inputs: Vec<NewCodeSymbol<'_>> = parsed
        .symbols
        .iter()
        .map(|s| NewCodeSymbol {
            file_id,
            name: &s.name,
            kind: &s.kind,
            role: s.role,
            line: s.line,
            end_line: s.end_line,
            signature: &s.signature,
        })
        .collect();
    store.replace_symbols_for_file(file_id, &sym_inputs)?;

    let mut chunk_ids = Vec::with_capacity(parsed.chunks.len());
    for (idx, ch) in parsed.chunks.iter().enumerate() {
        let id = store.upsert_chunk(NewCodeChunk {
            file_id,
            chunk_index: idx as i64,
            start_line: ch.start_line,
            end_line: ch.end_line,
            symbol_name: &ch.symbol_name,
            symbol_kind: &ch.symbol_kind,
            text: &ch.text,
            vector: &[],
        })?;
        chunk_ids.push(id);
    }
    store.truncate_chunks_for_file(file_id, parsed.chunks.len() as i64)?;
    Ok(chunk_ids)
}

fn symbol_aware_chunks(
    lines: &[&str],
    symbols: &[ExtractedSymbol],
    base_line: u32,
) -> Vec<ParsedCodeUnit> {
    let defs: Vec<&ExtractedSymbol> = symbols
        .iter()
        .filter(|s| s.role == SymbolRole::Definition)
        .collect();

    if defs.is_empty() {
        return fallback_line_chunks(lines, base_line);
    }

    let mut chunks = Vec::new();
    let total_lines = lines.len();

    for (i, def) in defs.iter().enumerate() {
        let start_idx = (def.line.saturating_sub(base_line) as usize).min(total_lines - 1);
        let mut end_idx = (def.end_line.saturating_sub(base_line) as usize).min(total_lines - 1);

        // If `end_line == line` (single-line declaration or header), extend up to
        // the line before the next definition (capped at 40 lines) so the body
        // context travels with the symbol chunk.
        if end_idx <= start_idx {
            let next_start = defs
                .get(i + 1)
                .map(|next| next.line.saturating_sub(base_line) as usize)
                .unwrap_or(total_lines);
            if next_start > start_idx + 1 {
                end_idx = (next_start - 1).min(start_idx + 40).min(total_lines - 1);
            }
        }

        let slice = &lines[start_idx..=end_idx];
        let text = slice.join("\n");
        if !text.trim().is_empty() {
            chunks.push(ParsedCodeUnit {
                start_line: base_line + start_idx as u32,
                end_line: base_line + end_idx as u32,
                symbol_name: def.name.clone(),
                symbol_kind: def.kind.clone(),
                text,
            });
        }
    }

    if chunks.is_empty() {
        fallback_line_chunks(lines, base_line)
    } else {
        chunks
    }
}

fn fallback_line_chunks(lines: &[&str], base_line: u32) -> Vec<ParsedCodeUnit> {
    let mut chunks = Vec::new();
    let step = (FALLBACK_CHUNK_LINES - FALLBACK_CHUNK_OVERLAP).max(1);
    let mut start = 0usize;
    while start < lines.len() {
        let end = (start + FALLBACK_CHUNK_LINES).min(lines.len()) - 1;
        let text = lines[start..=end].join("\n");
        if !text.trim().is_empty() {
            chunks.push(ParsedCodeUnit {
                start_line: base_line + start as u32,
                end_line: base_line + end as u32,
                symbol_name: String::new(),
                symbol_kind: "lines".to_string(),
                text,
            });
        }
        if end + 1 >= lines.len() {
            break;
        }
        start += step;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CodeFileRecord, DetectedRepo};

    fn assert_def_and_ref(
        lang: &str,
        source: &str,
        expected_def: (&str, u32),
        expected_ref: (&str, u32),
    ) {
        let parsed = parse_source(source, lang);
        assert!(
            !parsed.chunks.is_empty(),
            "language {lang}: expected non-empty chunks"
        );
        let found_def = parsed.symbols.iter().find(|s| {
            s.role == SymbolRole::Definition && s.name == expected_def.0 && s.line == expected_def.1
        });
        assert!(
            found_def.is_some(),
            "language {lang}: missing definition {:?} at line {} in {:#?}",
            expected_def.0,
            expected_def.1,
            parsed.symbols
        );

        let found_ref = parsed.symbols.iter().find(|s| {
            s.role == SymbolRole::Reference && s.name == expected_ref.0 && s.line == expected_ref.1
        });
        assert!(
            found_ref.is_some(),
            "language {lang}: missing reference {:?} at line {} in {:#?}",
            expected_ref.0,
            expected_ref.1,
            parsed.symbols
        );
    }

    #[test]
    fn all_twenty_grammars_extract_definitions_references_and_exact_line_numbers() {
        // 1. Rust
        assert_def_and_ref(
            "rust",
            "// comment\npub fn compute_score(x: i32) -> i32 {\n    helper_calc(x)\n}\n",
            ("compute_score", 2),
            ("helper_calc", 3),
        );

        // 2. C
        assert_def_and_ref(
            "c",
            "#include <stdio.h>\nint parse_header(const char *buf) {\n    return decode_token(buf);\n}\n",
            ("parse_header", 2),
            ("decode_token", 3),
        );

        // 3. C++
        assert_def_and_ref(
            "cpp",
            "class VectorEngine {\npublic:\n    void rebuild_index() {\n        flush_segments();\n    }\n};\n",
            ("VectorEngine", 1),
            ("flush_segments", 4),
        );

        // 4. C#
        assert_def_and_ref(
            "csharp",
            "public class OverlayController {\n    public void ShowOverlay() {\n        FocusSearchBox();\n    }\n}\n",
            ("OverlayController", 1),
            ("FocusSearchBox", 3),
        );

        // 5. Python
        assert_def_and_ref(
            "python",
            "# module\ndef train_embedder(batch):\n    return normalize_batch(batch)\n",
            ("train_embedder", 2),
            ("normalize_batch", 3),
        );

        // 6. JavaScript
        assert_def_and_ref(
            "javascript",
            "export function renderResults(items) {\n    return formatRow(items);\n}\n",
            ("renderResults", 1),
            ("formatRow", 2),
        );

        // 7. TypeScript
        assert_def_and_ref(
            "typescript",
            "export async function fetchManifest(url: string): Promise<void> {\n    await verifyChecksum(url);\n}\n",
            ("fetchManifest", 1),
            ("verifyChecksum", 2),
        );

        // 8. Go
        assert_def_and_ref(
            "go",
            "package main\nfunc ServePipe(name string) {\n    acceptClient(name)\n}\n",
            ("ServePipe", 2),
            ("acceptClient", 3),
        );

        // 9. Java
        assert_def_and_ref(
            "java",
            "public class IndexManager {\n    public void runSweep() {\n        purgeDeleted();\n    }\n}\n",
            ("IndexManager", 1),
            ("purgeDeleted", 3),
        );

        // 10. Kotlin
        assert_def_and_ref(
            "kotlin",
            "suspend fun syncRepository(path: String) {\n    scanDirectory(path)\n}\n",
            ("syncRepository", 1),
            ("scanDirectory", 2),
        );

        // 11. Swift
        assert_def_and_ref(
            "swift",
            "public func launchApplication(target: String) {\n    openWorkspace(target)\n}\n",
            ("launchApplication", 1),
            ("openWorkspace", 2),
        );

        // 12. PHP
        assert_def_and_ref(
            "php",
            "<?php\nfunction handleWebhook($payload) {\n     verifySignature($payload);\n}\n",
            ("handleWebhook", 2),
            ("verifySignature", 3),
        );

        // 13. Ruby
        assert_def_and_ref(
            "ruby",
            "class WorkerPool\n  def enqueue_job(item)\n    dispatch_now(item)\n  end\nend\n",
            (" enqueue_job".trim(), 2),
            ("dispatch_now", 3),
        );

        // 14. SQL
        assert_def_and_ref(
            "sql",
            "-- schema\nCREATE TABLE IF NOT EXISTS code_files (\n    id INTEGER PRIMARY KEY,\n    created_at INTEGER DEFAULT (unixepoch())\n);\n",
            ("code_files", 2),
            ("unixepoch", 4),
        );

        // 15. PowerShell
        assert_def_and_ref(
            "powershell",
            "function Start-WinSearchDaemon {\n    Invoke-PipeHealthCheck -Timeout 5\n}\n",
            ("Start-WinSearchDaemon", 1),
            ("Invoke-PipeHealthCheck", 2),
        );

        // 16. Bash
        assert_def_and_ref(
            "bash",
            "#!/usr/bin/env bash\nbuild_release() {\n    compile_native_target\n}\n",
            ("build_release", 2),
            ("compile_native_target", 3),
        );

        // 17. HTML
        assert_def_and_ref(
            "html",
            "<div id=\"search-overlay\">\n  <a href=\"#search-overlay\">Jump</a>\n</div>\n",
            ("search-overlay", 1),
            ("search-overlay", 2),
        );

        // 18. CSS
        assert_def_and_ref(
            "css",
            "@keyframes pulse-glow {\n  from { opacity: 0; }\n}\n.result-row {\n  animation: pulse-glow 0.2s;\n}\n",
            ("pulse-glow", 1),
            ("pulse-glow", 5),
        );

        // 19. YAML
        assert_def_and_ref(
            "yaml",
            "defaults: &base_defaults\n  timeout: 30\nproduction:\n  <<: *base_defaults\n",
            ("defaults", 1),
            ("base_defaults", 4),
        );

        // 20. TOML
        assert_def_and_ref(
            "toml",
            "[package]\nname = \"ws-code\"\nversion = resolve_version()\n",
            ("package", 1),
            ("resolve_version", 3),
        );
    }

    #[test]
    fn fallback_text_chunks_preserve_exact_line_ranges() {
        let mut lines = Vec::new();
        for i in 1..=120 {
            lines.push(format!("plain text line number {i}"));
        }
        let src = lines.join("\n");
        let parsed = parse_source(&src, "text");
        assert!(parsed.symbols.is_empty());
        assert!(parsed.chunks.len() >= 2);
        assert_eq!(parsed.chunks[0].start_line, 1);
        assert_eq!(parsed.chunks[0].end_line, 60);
        assert_eq!(parsed.chunks[1].start_line, 51);
        assert_eq!(parsed.chunks[1].end_line, 110);
    }

    #[test]
    fn notebook_indexes_cells_only_never_outputs_and_preserves_languages_and_lines() {
        let raw_ipynb = r##"{
  "metadata": {
    "kernelspec": {
      "name": "python3",
      "language": "python"
    },
    "language_info": {
      "name": "python"
    }
  },
  "cells": [
    {
      "cell_type": "markdown",
      "metadata": {},
      "source": [
        "# Notebook Heading\n",
        "Overview of the experiment."
      ]
    },
    {
      "cell_type": "code",
      "execution_count": 1,
      "metadata": {},
      "source": [
        "def compute_metrics(preds):\n",
        "    return score_predictions(preds)\n"
      ],
      "outputs": [
        {
          "output_type": "stream",
          "name": "stdout",
          "text": [
            "def leaked_output_function():\n",
            "    SECRET_OUTPUT_TOKEN_NEVER_INDEXED()\n"
          ]
        },
        {
          "output_type": "execute_result",
          "data": {
            "text/plain": ["SECRET_OUTPUT_TOKEN_NEVER_INDEXED"]
          }
        }
      ]
    },
    {
      "cell_type": "code",
      "execution_count": 2,
      "metadata": {},
      "source": [
        "%%sql\n",
        "CREATE TABLE experiment_runs (\n",
        "    id INT PRIMARY KEY\n",
        ");\n"
      ],
      "outputs": []
    }
  ],
  "nbformat": 4,
  "nbformat_minor": 5
}"##;

        let nb = parse_notebook(raw_ipynb).expect("parse valid .ipynb");
        assert_eq!(nb.default_language, "python");
        assert_eq!(nb.cells.len(), 3);
        assert_eq!(nb.cells[0].language, "markdown");
        assert_eq!(nb.cells[0].start_line, 1);
        assert_eq!(nb.cells[0].end_line, 2);

        assert_eq!(nb.cells[1].language, "python");
        assert_eq!(nb.cells[1].start_line, 3);
        assert_eq!(nb.cells[1].end_line, 4);

        assert_eq!(nb.cells[2].language, "sql");
        assert_eq!(nb.cells[2].start_line, 5);
        assert_eq!(nb.cells[2].end_line, 8);

        // Verify definitions and references from cells have exact virtual line numbers
        assert!(nb.parsed.symbols.iter().any(|s| {
            s.name == "compute_metrics" && s.role == SymbolRole::Definition && s.line == 3
        }));
        assert!(nb.parsed.symbols.iter().any(|s| {
            s.name == "score_predictions" && s.role == SymbolRole::Reference && s.line == 4
        }));
        assert!(nb.parsed.symbols.iter().any(|s| {
            s.name == "experiment_runs" && s.role == SymbolRole::Definition && s.line == 6
        }));

        // Verify cell outputs are NEVER indexed in chunks or symbols
        for ch in &nb.parsed.chunks {
            assert!(
                !ch.text.contains("SECRET_OUTPUT_TOKEN_NEVER_INDEXED")
                    && !ch.text.contains("leaked_output_function"),
                "notebook output leaked into chunk: {ch:?}"
            );
        }
        for sym in &nb.parsed.symbols {
            assert!(
                !sym.name.contains("SECRET_OUTPUT_TOKEN")
                    && sym.name != "leaked_output_function",
                "notebook output leaked into symbol: {sym:?}"
            );
        }

        // Persist into CodeStore and verify FTS and symbol queries
        let mut store = CodeStore::open_in_memory().unwrap();
        let repo_id = store
            .ensure_repo(&DetectedRepo {
                root_path: "C:\\nb".into(),
                name: "nb".into(),
                has_git: true,
            })
            .unwrap();
        let outcomes = store
            .apply_file_batch(
                repo_id,
                1,
                &[CodeFileRecord {
                    path: "C:\\nb\\exp.ipynb".into(),
                    rel_path: "exp.ipynb".into(),
                    name: "exp.ipynb".into(),
                    ext: "ipynb".into(),
                    language: "notebook".into(),
                    size: raw_ipynb.len() as u64,
                    mtime: 100,
                    volume_serial: 1,
                    file_id: 1,
                }],
            )
            .unwrap();
        let file_id = outcomes[0].file_id();
        persist_parsed_file(&mut store, file_id, &nb.parsed).unwrap();

        let found = store.search_symbols("compute_metrics", 10).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].is_definition);
        assert_eq!(found[0].line, 3);

        let leaked = store.search_symbols("leaked_output_function", 10).unwrap();
        assert!(leaked.is_empty());
    }

    #[test]
    fn malformed_notebook_returns_empty_parsed_file_without_leaking_raw_json() {
        let malformed = r#"{"cells": [{"cell_type": "code", "outputs": ["SECRET_LEAK_TOKEN""#;
        let parsed = parse_source(malformed, "notebook");
        assert_eq!(parsed.language, "notebook");
        assert!(parsed.chunks.is_empty());
        assert!(parsed.symbols.is_empty());
    }

    #[test]
    fn persist_parsed_file_truncates_stale_tail_chunks() {
        let mut store = CodeStore::open_in_memory().unwrap();
        let repo_id = store
            .ensure_repo(&DetectedRepo {
                root_path: "C:\\truncate".into(),
                name: "truncate".into(),
                has_git: true,
            })
            .unwrap();
        let outcomes = store
            .apply_file_batch(
                repo_id,
                1,
                &[CodeFileRecord {
                    path: "C:\\truncate\\lib.rs".into(),
                    rel_path: "lib.rs".into(),
                    name: "lib.rs".into(),
                    ext: "rs".into(),
                    language: "rust".into(),
                    size: 100,
                    mtime: 1,
                    volume_serial: 1,
                    file_id: 1,
                }],
            )
            .unwrap();
        let file_id = outcomes[0].file_id();

        let v1 = parse_source(
            "pub fn alpha() {\n    call_one();\n}\npub fn beta() {\n    call_two();\n}\npub fn gamma() {\n    stale_tail_marker();\n}\n",
            "rust",
        );
        assert_eq!(v1.chunks.len(), 3);
        persist_parsed_file(&mut store, file_id, &v1).unwrap();
        assert_eq!(store.list_chunks_for_file(file_id).unwrap().len(), 3);

        let v2 = parse_source("pub fn alpha() {\n    call_one();\n}\n", "rust");
        assert_eq!(v2.chunks.len(), 1);
        persist_parsed_file(&mut store, file_id, &v2).unwrap();

        let chunks = store.list_chunks_for_file(file_id).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].symbol_name, "alpha");
        assert_eq!(store.stats().unwrap().chunks_total, 1);
    }
}
