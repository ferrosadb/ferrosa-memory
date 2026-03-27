# Entity Extraction Quality Fixes

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix three extraction quality bugs that cause empty/single-word entity names.

**Architecture:** Targeted fixes to `extract_entity_candidates`, `heuristic_extract_entity`, and `llm_extract_entity`. No API changes — all fixes are internal.

**Tech Stack:** Rust

---

### Task 1: Fix sentence-starter skip in `extract_entity_candidates`

**Files:**
- Modify: `crates/ferrosa-memory-core/src/smart_ingest.rs`

The `i > 0` check at line ~271 skips ALL entities at position 0, including real entities like "Ben Kearns". The intent was to skip sentence starters like "The" and "However" — but those are already handled by `is_common_word()`.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn extract_entities_at_position_zero() {
    let text = "Ben Kearns built Ferrosa from scratch";
    let candidates = extract_entity_candidates(text);
    let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
    assert!(names.contains(&"Ben Kearns"), "should extract entity at position 0, got: {names:?}");
}
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test --lib -p ferrosa-memory-core -- smart_ingest::tests::extract_entities_at_position_zero`

- [ ] **Step 3: Remove the `i > 0` check**

In `extract_entity_candidates`, remove the `&& i > 0` condition from the if-statement. The `is_common_word()` check already filters "The", "However", etc. so the sentence-starter skip is redundant.

Change:
```rust
if word.len() > 1
    && word.chars().next().is_some_and(|c| c.is_uppercase())
    && !is_common_word(word)
    // skip sentence starters (i > 0)
    && i > 0
```
To:
```rust
if word.len() > 1
    && word.chars().next().is_some_and(|c| c.is_uppercase())
    && !is_common_word(word)
```

- [ ] **Step 4: Fix the existing test `extract_entities_skips_sentence_starters`**

This test asserts that "Cassandra" at position 0 is NOT extracted. After the fix, it SHOULD be extracted. Update the test:

```rust
#[test]
fn extract_entities_at_sentence_start() {
    let text = "Cassandra is great. Redis is fast.";
    let candidates = extract_entity_candidates(text);
    let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
    // Both should be extracted — position 0 is no longer skipped
    assert!(names.contains(&"Cassandra"), "should extract Cassandra at position 0");
    assert!(names.contains(&"Redis"), "should extract Redis mid-sentence");
}
```

Also fix the test at line ~679 (`extract_entities_from_technical_text`) — it currently prefixes with "uses" to work around the skip. Now "Ferrosa" should be extractable at any position. Verify all existing tests still pass, updating assertions as needed.

- [ ] **Step 5: Run all tests**

Run: `cargo test --lib -p ferrosa-memory-core -- smart_ingest::tests`

- [ ] **Step 6: Commit**

```bash
git commit -m "fix: remove position-0 entity skip in extract_entity_candidates"
```

---

### Task 2: Rank candidates in `heuristic_extract_entity`

**Files:**
- Modify: `crates/ferrosa-memory-core/src/ner.rs`

Currently takes the first candidate blindly. Should prefer multi-word proper names over single generic words.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn heuristic_prefers_multi_word_over_single_word() {
    // "Storage" is a single generic word, "Ben Kearns" is a proper name
    let (name, _) = heuristic_extract_entity("Storage design by Ben Kearns is excellent");
    assert_eq!(name, "Ben Kearns", "should prefer multi-word entity over single word");
}

#[test]
fn heuristic_prefers_typed_over_concept() {
    // "Docker" is a known tool, "Original" is a generic concept
    let (name, etype) = heuristic_extract_entity("Original design uses Docker containers");
    assert_eq!(name, "Docker");
    assert_eq!(etype, "tool");
}
```

- [ ] **Step 2: Run test, verify it fails**

- [ ] **Step 3: Implement candidate ranking**

Replace the `candidates.into_iter().next()` in `heuristic_extract_entity` with a ranking function:

```rust
pub fn heuristic_extract_entity(content: &str) -> (String, String) {
    let candidates = crate::smart_ingest::extract_entity_candidates(content);
    if let Some((name, etype)) = rank_candidates(candidates) {
        (name, etype)
    } else {
        let truncated: String = content
            .split_whitespace()
            .take(8)
            .collect::<Vec<_>>()
            .join(" ");
        let etype = crate::smart_ingest::infer_entity_type(&truncated);
        (truncated, etype.to_string())
    }
}

/// Rank entity candidates by quality. Prefers:
/// 1. Multi-word entities with a non-concept type (e.g. "Ben Kearns" → person)
/// 2. Single-word entities with a non-concept type (e.g. "Docker" → tool)
/// 3. Multi-word entities typed as concept
/// 4. Single-word entities typed as concept
fn rank_candidates(candidates: Vec<(String, String)>) -> Option<(String, String)> {
    if candidates.is_empty() {
        return None;
    }

    let mut best = &candidates[0];
    let mut best_score = candidate_score(best);

    for candidate in &candidates[1..] {
        let score = candidate_score(candidate);
        if score > best_score {
            best = candidate;
            best_score = score;
        }
    }

    Some((best.0.clone(), best.1.clone()))
}

fn candidate_score(candidate: &(String, String)) -> u8 {
    let multi_word = candidate.0.split_whitespace().count() >= 2;
    let typed = candidate.1 != "concept";
    match (multi_word, typed) {
        (true, true) => 3,
        (false, true) => 2,
        (true, false) => 1,
        (false, false) => 0,
    }
}
```

- [ ] **Step 4: Run all ner tests**

Run: `cargo test --lib -p ferrosa-memory-core -- ner::tests`

- [ ] **Step 5: Commit**

```bash
git commit -m "fix: rank entity candidates by quality instead of taking first"
```

---

### Task 3: Truncate content and improve LLM prompt

**Files:**
- Modify: `crates/ferrosa-memory-core/src/ner.rs`

The LLM gets the full content (potentially thousands of chars) and returns empty names. Truncate to first 500 chars and improve the prompt.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn llm_prompt_truncates_long_content() {
    let long_content = "word ".repeat(1000);
    // Verify the prompt builder truncates
    let truncated = truncate_for_prompt(&long_content, 500);
    assert!(truncated.len() <= 500);
    assert!(truncated.ends_with("..."));
}

#[test]
fn llm_prompt_preserves_short_content() {
    let short = "Ben Kearns built Ferrosa";
    let result = truncate_for_prompt(short, 500);
    assert_eq!(result, short);
}
```

- [ ] **Step 2: Run test, verify it fails**

- [ ] **Step 3: Add `truncate_for_prompt` and update `llm_extract_entity`**

Add helper:
```rust
/// Truncate content for LLM prompts, breaking at word boundaries.
fn truncate_for_prompt(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars {
        return content.to_string();
    }
    let truncated = &content[..content[..max_chars].rfind(' ').unwrap_or(max_chars)];
    format!("{truncated}...")
}
```

Update the prompt in `llm_extract_entity` to truncate content and be more directive:

```rust
let truncated_content = truncate_for_prompt(content, 500);
let prompt = format!(
    "/no_think\nExtract the single most important named entity from this text.\n\
     Return JSON: {{\"name\": \"<entity name>\", \"type\": \"<one of: person|org|tool|project|place|event|concept|decision|pattern|preference>\"}}\n\
     Rules:\n\
     - name must be a proper noun or specific name (e.g. \"Ben Kearns\", \"Docker\", \"Ferrosa\")\n\
     - name must NOT be empty\n\
     - name must NOT be a generic word like \"Storage\" or \"Original\"\n\
     Text: {truncated_content}\n\
     JSON:"
);
```

- [ ] **Step 4: Run all ner tests**

Run: `cargo test --lib -p ferrosa-memory-core -- ner::tests`

- [ ] **Step 5: Commit**

```bash
git commit -m "fix: truncate LLM content and improve extraction prompt"
```

---

### Task 4: Full CI validation

- [ ] **Step 1:** `cargo fmt --check`
- [ ] **Step 2:** `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] **Step 3:** `cargo test --workspace --lib`
- [ ] **Step 4:** Coverage check (≥80%)
- [ ] **Step 5:** `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items`
- [ ] **Step 6:** `git push` and update PR
