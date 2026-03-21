# ADR-001: Rust-Native Compression (No Python)

## Status

Accepted

## Context

The original spec (Section 8.1) called for LLMLingua as a "Python subprocess invoked via WASM boundary." This introduces a Python runtime dependency, subprocess management complexity, and latency from cross-process calls.

## Decision

Implement compression entirely in Rust. Port the core LLMLingua algorithm — token importance scoring via information-theoretic weights — to a pure Rust module. No Python subprocesses, no Python dependencies anywhere in the project.

## Approach

The compression module will:
1. Tokenize input text (whitespace + punctuation tokenizer, not a full BPE — we don't need model-compatible tokens for compression)
2. Score token importance using TF-IDF weighted by position (beginning/end of sentences weighted higher)
3. Drop lowest-importance tokens to reach target compression ratio
4. Preserve sentence structure markers and semantic connectives

This captures ~80% of LLMLingua's compression quality for the fold trajectory use case without requiring model inference. The remaining 20% (model-perplexity-based token scoring) can be added later by calling the embedding endpoint for perplexity estimates.

## Consequences

- No Python runtime required at deploy time
- Compression is synchronous and fast (no subprocess latency)
- Slightly lower compression quality than full LLMLingua with model inference
- The NL-Compress "capsule" format is also implementable in pure Rust
- WASM UDF compilation is straightforward (pure Rust, no FFI)
