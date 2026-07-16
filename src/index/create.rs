use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use regex::Regex;

use crate::bm25::Bm25Index;
use crate::chunking::chunk_source;
use crate::encoder::{SemanticIndex, StaticEncoder};
use crate::file_walker::{filter_extensions, language_for_path, walk_files};
use crate::graph::DependencyGraph;
use crate::tokens::tokenize;
use crate::types::Chunk;

const MAX_FILE_BYTES: u64 = 2_000_000;

fn text_for_indexing(chunk: &Chunk, fallback: bool) -> &str {
    if let Some(ref st) = chunk.search_text {
        if st.is_empty() {
            if fallback {
                &chunk.content
            } else {
                ""
            }
        } else {
            st
        }
    } else {
        &chunk.content
    }
}

fn enrich_for_bm25(chunk: &Chunk, fallback: bool) -> String {
    let text = text_for_indexing(chunk, fallback);
    let path = Path::new(&chunk.file_path);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let dir_parts: Vec<&str> = path
        .parent()
        .map(|p| {
            p.components()
                .filter_map(|c| {
                    let s = c.as_os_str().to_str()?;
                    if s == "." || s == "/" {
                        None
                    } else {
                        Some(s)
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let dir_text: String = dir_parts
        .iter()
        .rev()
        .take(3)
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    format!("{} {stem} {stem} {dir_text}", text)
}

pub fn create_index_from_path(
    path: &Path,
    encoder: &StaticEncoder,
    extensions: Option<&HashSet<String>>,
    ignore: Option<&HashSet<String>>,
    include_text_files: bool,
    display_root: &Path,
    chunk_regex: Option<&Regex>,
    single_file: Option<&str>,
    index_field: Option<&Regex>,
    index_fallback: bool,
) -> Result<(Bm25Index, SemanticIndex, Vec<Chunk>, DependencyGraph)> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut graph = DependencyGraph::new();

    let explicit_file = single_file.is_some();

    let files: Vec<std::path::PathBuf> = if let Some(sf) = single_file {
        let target = display_root.join(sf);
        if !target.exists() {
            bail!("File specified by --file not found: {}", target.display());
        }
        let target = target.canonicalize().context("Failed to resolve --file path")?;
        vec![target]
    } else {
        let exts = filter_extensions(extensions, include_text_files);
        walk_files(path, &exts, ignore)
    };

    for file_path in &files {
        let metadata = match file_path.metadata() {
            Ok(m) => m,
            Err(e) if explicit_file => {
                bail!("Cannot read --file '{}': {e}", file_path.display());
            }
            Err(_) => continue,
        };
        if metadata.len() > MAX_FILE_BYTES && !explicit_file {
            continue;
        }
        let source = match std::fs::read_to_string(file_path) {
            Ok(s) => s,
            Err(e) if explicit_file => {
                bail!("Cannot read --file '{}': {e}", file_path.display());
            }
            Err(_) => continue,
        };
        let language = language_for_path(file_path);
        let chunk_path = file_path
            .strip_prefix(display_root)
            .unwrap_or(file_path)
            .to_string_lossy()
            .to_string();
        chunks.extend(chunk_source(&source, &chunk_path, language, chunk_regex, index_field));

        if let Some(lang) = language {
            graph.add_file(&chunk_path, &source, lang);
        }
    }

    if chunks.is_empty() {
        if let Some(sf) = single_file {
            bail!("No chunks produced from --file '{}' (check that --include-text-files is set and --chunk-regex matches)", sf);
        } else {
            bail!("No supported files found under {}", path.display());
        }
    }

    graph.resolve_dependencies();

    let texts: Vec<String> = chunks
        .iter()
        .map(|c| text_for_indexing(c, index_fallback).to_string())
        .collect();
    let embeddings = encoder
        .encode_batch(&texts)
        .context("Failed to encode chunks")?;
    let semantic_index = SemanticIndex::new(embeddings);

    let bm25_docs: Vec<Vec<String>> = chunks
        .iter()
        .map(|chunk| tokenize(&enrich_for_bm25(chunk, index_fallback)))
        .collect();
    let bm25_index = Bm25Index::new(&bm25_docs);

    Ok((bm25_index, semantic_index, chunks, graph))
}
