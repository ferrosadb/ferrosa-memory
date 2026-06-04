//! Semantic Markdown document chunking.
//!
//! The chunker keeps stable Markdown block boundaries where possible and
//! records enough linkage metadata for storage layers to persist temporal
//! prev/next relationships later.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentChunkConfig {
    pub max_chars: usize,
    pub overlap_chars: usize,
}

impl Default for DocumentChunkConfig {
    fn default() -> Self {
        Self {
            max_chars: 2_000,
            overlap_chars: 200,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentChunk {
    pub ordinal: usize,
    pub text: String,
    pub bm25_text: String,
    pub semantic_kind: SemanticChunkKind,
    pub section_path: Vec<String>,
    pub prev_ordinal: Option<usize>,
    pub next_ordinal: Option<usize>,
    pub has_leading_overlap: bool,
    pub has_trailing_overlap: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticChunkKind {
    Heading,
    Paragraph,
    List,
    CodeFence,
    Mixed,
}

#[derive(Debug, Clone)]
struct MarkdownBlock {
    kind: SemanticChunkKind,
    text: String,
    section_path: Vec<String>,
}

#[derive(Debug, Clone)]
struct ChunkDraft {
    text: String,
    kind: SemanticChunkKind,
    section_path: Vec<String>,
    has_leading_overlap: bool,
    has_trailing_overlap: bool,
}

pub fn chunk_markdown_document(input: &str, config: &DocumentChunkConfig) -> Vec<DocumentChunk> {
    let max_chars = config.max_chars.max(1);
    let overlap_chars = config.overlap_chars.min(max_chars.saturating_sub(1));
    let blocks = parse_markdown_blocks(input);
    let mut drafts = Vec::new();
    let mut current: Option<ChunkDraft> = None;

    for block in blocks {
        if block.text.chars().count() > max_chars {
            flush_current(&mut current, &mut drafts);
            drafts.extend(split_oversized_block(&block, max_chars, overlap_chars));
            continue;
        }

        match current.take() {
            None => current = Some(ChunkDraft::from_block(block)),
            Some(mut draft) => {
                let same_section = draft.section_path == block.section_path;
                let combined_len = draft.text.chars().count() + 2 + block.text.chars().count();
                let keep_list_atomic =
                    block.kind == SemanticChunkKind::List || draft.kind == SemanticChunkKind::List;
                if same_section && combined_len <= max_chars && !keep_list_atomic {
                    draft.text.push_str("\n\n");
                    draft.text.push_str(&block.text);
                    if draft.kind != block.kind {
                        draft.kind = SemanticChunkKind::Mixed;
                    }
                    current = Some(draft);
                } else {
                    drafts.push(draft);
                    current = Some(ChunkDraft::from_block(block));
                }
            }
        }
    }

    flush_current(&mut current, &mut drafts);
    link_chunks(drafts)
}

fn parse_markdown_blocks(input: &str) -> Vec<MarkdownBlock> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = input.lines().collect();
    let mut section_path: Vec<String> = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        if line.trim().is_empty() {
            index += 1;
            continue;
        }

        if let Some((level, title)) = parse_heading(line) {
            section_path.truncate(level.saturating_sub(1));
            section_path.push(title.to_string());
            blocks.push(MarkdownBlock {
                kind: SemanticChunkKind::Heading,
                text: line.trim_end().to_string(),
                section_path: section_path.clone(),
            });
            index += 1;
            continue;
        }

        if is_fence_start(line) {
            let start = index;
            let fence = line.trim_start().chars().next().unwrap_or('`');
            index += 1;
            while index < lines.len() {
                let trimmed = lines[index].trim_start();
                index += 1;
                if trimmed.starts_with(&fence.to_string().repeat(3)) {
                    break;
                }
            }
            blocks.push(MarkdownBlock {
                kind: SemanticChunkKind::CodeFence,
                text: lines[start..index].join("\n").trim_end().to_string(),
                section_path: section_path.clone(),
            });
            continue;
        }

        if is_list_marker(line) {
            let start = index;
            index += 1;
            while index < lines.len() {
                let next = lines[index];
                if next.trim().is_empty() {
                    let following = next_nonblank_line(&lines, index + 1);
                    if following.is_some_and(is_list_marker) {
                        index += 1;
                        continue;
                    }
                    break;
                }
                if is_list_marker(next) || next.starts_with(' ') || next.starts_with('\t') {
                    index += 1;
                    continue;
                }
                break;
            }
            blocks.push(MarkdownBlock {
                kind: SemanticChunkKind::List,
                text: lines[start..index].join("\n").trim_end().to_string(),
                section_path: section_path.clone(),
            });
            continue;
        }

        let start = index;
        index += 1;
        while index < lines.len() {
            let next = lines[index];
            if next.trim().is_empty()
                || parse_heading(next).is_some()
                || is_fence_start(next)
                || is_list_marker(next)
            {
                break;
            }
            index += 1;
        }
        blocks.push(MarkdownBlock {
            kind: SemanticChunkKind::Paragraph,
            text: lines[start..index].join("\n").trim_end().to_string(),
            section_path: section_path.clone(),
        });
    }

    blocks
}

fn split_oversized_block(
    block: &MarkdownBlock,
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<ChunkDraft> {
    match block.kind {
        SemanticChunkKind::List => split_oversized_list(block, max_chars, overlap_chars),
        _ => split_text_with_overlap(block, max_chars, overlap_chars),
    }
}

fn split_oversized_list(
    block: &MarkdownBlock,
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<ChunkDraft> {
    let items = split_list_items(&block.text);
    if items.len() <= 1 {
        return split_text_with_overlap(block, max_chars, overlap_chars);
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut last_item = String::new();

    for item in items {
        let candidate_len = joined_len(&current, &item);
        if !current.is_empty() && candidate_len > max_chars {
            chunks.push(current);
            current = overlap_prefix(&last_item, overlap_chars);
        }

        if current.is_empty() {
            current = item.clone();
        } else {
            current.push('\n');
            current.push_str(&item);
        }
        last_item = item;
    }

    if !current.trim().is_empty() {
        chunks.push(current);
    }

    chunks
        .into_iter()
        .enumerate()
        .map(|(idx, text)| ChunkDraft {
            text,
            kind: SemanticChunkKind::List,
            section_path: block.section_path.clone(),
            has_leading_overlap: idx > 0,
            has_trailing_overlap: false,
        })
        .collect::<Vec<_>>()
        .mark_trailing_overlap()
}

fn split_text_with_overlap(
    block: &MarkdownBlock,
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<ChunkDraft> {
    let chars: Vec<char> = block.text.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < chars.len() {
        let end = chars.len().min(start + max_chars);
        let text = chars[start..end].iter().collect::<String>();
        chunks.push(ChunkDraft {
            text,
            kind: block.kind,
            section_path: block.section_path.clone(),
            has_leading_overlap: start > 0,
            has_trailing_overlap: end < chars.len(),
        });
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(overlap_chars);
        if start == end {
            start += 1;
        }
    }

    chunks
}

fn split_list_items(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = Vec::new();

    for line in text.lines() {
        if is_list_marker(line) && !current.is_empty() {
            items.push(current.join("\n"));
            current.clear();
        }
        current.push(line.to_string());
    }

    if !current.is_empty() {
        items.push(current.join("\n"));
    }

    items
}

fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.starts_with(' ') {
        return None;
    }
    let title = rest.trim();
    if title.is_empty() {
        return None;
    }
    Some((level, title))
}

fn is_fence_start(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn is_list_marker(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("+ ")
        || ordered_list_marker(trimmed)
}

fn ordered_list_marker(trimmed: &str) -> bool {
    let Some((prefix, rest)) = trimmed.split_once(". ") else {
        return false;
    };
    !prefix.is_empty() && prefix.chars().all(|ch| ch.is_ascii_digit()) && !rest.is_empty()
}

fn next_nonblank_line<'a>(lines: &'a [&str], start: usize) -> Option<&'a str> {
    lines
        .iter()
        .skip(start)
        .copied()
        .find(|line| !line.trim().is_empty())
}

fn joined_len(left: &str, right: &str) -> usize {
    if left.is_empty() {
        right.chars().count()
    } else {
        left.chars().count() + 1 + right.chars().count()
    }
}

fn overlap_prefix(text: &str, overlap_chars: usize) -> String {
    if overlap_chars == 0 {
        String::new()
    } else {
        take_last_chars(text, overlap_chars)
    }
}

fn take_last_chars(text: &str, count: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(count);
    chars[start..].iter().collect()
}

trait MarkTrailingOverlap {
    fn mark_trailing_overlap(self) -> Self;
}

impl MarkTrailingOverlap for Vec<ChunkDraft> {
    fn mark_trailing_overlap(mut self) -> Self {
        let len = self.len();
        for (idx, chunk) in self.iter_mut().enumerate() {
            chunk.has_trailing_overlap = idx + 1 < len;
        }
        self
    }
}

impl ChunkDraft {
    fn from_block(block: MarkdownBlock) -> Self {
        Self {
            text: block.text,
            kind: block.kind,
            section_path: block.section_path,
            has_leading_overlap: false,
            has_trailing_overlap: false,
        }
    }
}

fn flush_current(current: &mut Option<ChunkDraft>, chunks: &mut Vec<ChunkDraft>) {
    if let Some(draft) = current.take() {
        chunks.push(draft);
    }
}

fn link_chunks(drafts: Vec<ChunkDraft>) -> Vec<DocumentChunk> {
    let len = drafts.len();
    drafts
        .into_iter()
        .enumerate()
        .map(|(ordinal, draft)| DocumentChunk {
            ordinal,
            bm25_text: draft.text.to_lowercase(),
            text: draft.text,
            semantic_kind: draft.kind,
            section_path: draft.section_path,
            prev_ordinal: ordinal.checked_sub(1),
            next_ordinal: (ordinal + 1 < len).then_some(ordinal + 1),
            has_leading_overlap: draft.has_leading_overlap,
            has_trailing_overlap: draft.has_trailing_overlap,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_list_together() {
        let chunks = chunk_markdown_document(
            "Intro text.\n\n- alpha\n- beta\n- gamma\n\nAfter.",
            &DocumentChunkConfig {
                max_chars: 120,
                overlap_chars: 10,
            },
        );

        let list_chunk = chunks
            .iter()
            .find(|chunk| chunk.text.contains("- alpha"))
            .expect("list chunk");
        assert_eq!(list_chunk.semantic_kind, SemanticChunkKind::List);
        assert!(list_chunk.text.contains("- beta"));
        assert!(list_chunk.text.contains("- gamma"));
    }

    #[test]
    fn links_prev_next_ordinals() {
        let chunks = chunk_markdown_document(
            "First paragraph is intentionally long enough.\n\nSecond paragraph also stands alone.\n\nThird paragraph.",
            &DocumentChunkConfig {
                max_chars: 44,
                overlap_chars: 8,
            },
        );

        assert!(chunks.len() >= 3);
        assert_eq!(chunks[0].prev_ordinal, None);
        assert_eq!(chunks[0].next_ordinal, Some(1));
        assert_eq!(chunks[1].prev_ordinal, Some(0));
        assert_eq!(chunks[1].next_ordinal, Some(2));
        assert_eq!(chunks.last().unwrap().next_ordinal, None);
    }

    #[test]
    fn splits_oversized_list_with_overlap() {
        let document =
            "- alpha alpha alpha\n- beta beta beta\n- gamma gamma gamma\n- delta delta delta";
        let chunks = chunk_markdown_document(
            document,
            &DocumentChunkConfig {
                max_chars: 42,
                overlap_chars: 18,
            },
        );

        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.semantic_kind == SemanticChunkKind::List)
        );
        assert!(chunks[0].has_trailing_overlap);
        assert!(chunks[1].has_leading_overlap);
        assert_eq!(chunks[0].next_ordinal, Some(1));
        assert_eq!(chunks[1].prev_ordinal, Some(0));
        assert!(chunks[1].text.contains("beta beta beta"));
    }

    #[test]
    fn preserves_heading_section_path() {
        let chunks = chunk_markdown_document(
            "# Project\n\n## Design\n\nThe chunk belongs to the design section.",
            &DocumentChunkConfig {
                max_chars: 200,
                overlap_chars: 20,
            },
        );

        let chunk = chunks
            .iter()
            .find(|chunk| chunk.text.contains("design section"))
            .expect("section chunk");
        assert_eq!(chunk.section_path, vec!["Project", "Design"]);
    }
}
