# Remote Knowledge Sharing

## Status

In process.

## Trigger Use Case

The BRIGHT-Pro paper was needed while working in `ferrosa-memory`, but the local corpus did not contain it. Another machine did:

- Host: `192.168.202.88`
- Summary: `/home/bkearns/bright_pro_eval_summary.md`
- PDF: `/home/bkearns/corpus/arxiv_2605.04018.pdf`
- SHA-256: `597bc950e18d745c110c7714829e7250ba8e8de108acc2355ff54e71d02a83b6`
- Local mirror:
  - `/Users/bkearns/src/research/corpus/ai-retrieval/bright_pro_eval_summary.md`
  - `/Users/bkearns/src/research/corpus/ai-retrieval/arxiv_2605.04018.pdf`

This is the concrete failure mode: a useful knowledge artifact existed in the user's personal research environment, but only on another host. The agent could not know it existed from the local corpus.

## Product Goal

Make remote personal knowledge discoverable and fetchable with provenance, without turning every agent session into an ad hoc SSH spelunking exercise.

The intended interaction is:

1. Agent asks the local memory server for knowledge about a topic.
2. Local memory server can answer from local memory or report remote candidates.
3. Agent fetches a small remote summary first.
4. Agent fetches the full source artifact only when the task needs it.
5. Ingested knowledge records source host, path, hash, and retrieval time.

## Non-Goals

- Do not silently crawl entire remote disks.
- Do not execute arbitrary remote commands from normal memory queries.
- Do not copy large PDFs unless explicitly requested by the agent or user workflow.
- Do not treat remote files as trusted without host/path/hash provenance.

## Proposed Model

### Remote Source Registry

Configured list of remote knowledge roots:

```toml
[[remote_knowledge.sources]]
name = "gpu-pc"
host = "192.168.202.88"
user = "bkearns"
roots = [
  "/home/bkearns/corpus",
  "/home/bkearns/.hermes/cron/output",
  "/home/bkearns/src/ferrosa-suite"
]
summary_roots = [
  "/home/bkearns"
]
transport = "ssh"
```

### Artifact Record

Remote artifact metadata stored locally after discovery:

```json
{
  "artifact_id": "sha256:597bc950e18d745c110c7714829e7250ba8e8de108acc2355ff54e71d02a83b6",
  "title": "Rethinking Reasoning-Intensive Retrieval: Evaluating and Advancing Retrievers in Agentic Search Systems",
  "source_host": "192.168.202.88",
  "source_path": "/home/bkearns/corpus/arxiv_2605.04018.pdf",
  "summary_path": "/home/bkearns/bright_pro_eval_summary.md",
  "local_path": "/Users/bkearns/src/research/corpus/ai-retrieval/arxiv_2605.04018.pdf",
  "sha256": "597bc950e18d745c110c7714829e7250ba8e8de108acc2355ff54e71d02a83b6",
  "content_type": "application/pdf",
  "retrieved_at": "2026-06-03T18:33:00Z",
  "tags": ["BRIGHT-Pro", "reasoning-intensive retrieval", "agentic retrieval"]
}
```

## Tool/API Surface

### `remote_knowledge_search`

Search registered remote indexes and lightweight manifests.

Input:

```json
{
  "query": "BRIGHT-Pro reasoning-intensive retrieval",
  "sources": ["gpu-pc"],
  "limit": 10
}
```

Output:

```json
{
  "candidates": [
    {
      "source": "gpu-pc",
      "host": "192.168.202.88",
      "path": "/home/bkearns/bright_pro_eval_summary.md",
      "kind": "summary",
      "title": "BRIGHT-Pro: Evaluation Metrics & Protocol",
      "snippet": "BRIGHT-Pro introduces aspect-aware metrics for reasoning-intensive retrieval..."
    }
  ]
}
```

### `remote_artifact_fetch`

Fetch one known remote artifact by host/path, with optional hash verification and size cap.

Input:

```json
{
  "source": "gpu-pc",
  "remote_path": "/home/bkearns/corpus/arxiv_2605.04018.pdf",
  "expected_sha256": "597bc950e18d745c110c7714829e7250ba8e8de108acc2355ff54e71d02a83b6",
  "local_dir": "/Users/bkearns/src/research/corpus/ai-retrieval",
  "max_bytes": 25000000
}
```

Output:

```json
{
  "fetched": true,
  "local_path": "/Users/bkearns/src/research/corpus/ai-retrieval/arxiv_2605.04018.pdf",
  "sha256": "597bc950e18d745c110c7714829e7250ba8e8de108acc2355ff54e71d02a83b6",
  "bytes": 5170000,
  "provenance": {
    "source": "gpu-pc",
    "host": "192.168.202.88",
    "remote_path": "/home/bkearns/corpus/arxiv_2605.04018.pdf"
  }
}
```

### `remote_summary_get`

Fetch only a summary or first N KB of a remote text artifact.

This is the default for agent context building. It avoids pulling large PDFs when a small human-authored summary is enough.

## Storage Integration

When a remote artifact is used, `smart_ingest` should accept source metadata:

- `source_type`: `remote_file`
- `source_host`
- `source_path`
- `source_sha256`
- `local_path`
- `retrieved_at`

This generalizes the existing web-ingestion `source_url` idea to non-web personal knowledge.

## BRIGHT-Pro Readiness

For BRIGHT-Pro, remote knowledge sharing should support:

1. Locate the paper by title, arXiv id, or benchmark name.
2. Retrieve the summary first for planning.
3. Fetch and hash the PDF when implementation needs exact method details.
4. Record the paper as a corpus artifact.
5. Let the eval harness resolve corpus inputs from:
   - local corpus
   - remote source registry
   - previously mirrored artifacts

## Security And Failure Modes

- SSH access must use existing user credentials/agent; no password prompts in MCP tools.
- Only configured roots are readable.
- Fetches require a max byte limit.
- Hash mismatch fails loud and does not ingest.
- Remote command failures return structured errors with host/path/exit status.
- Remote summaries are treated as context, not canonical truth; the PDF or source file remains the provenance anchor.

## Acceptance Criteria

- [x] Agent can verify a remote summary and PDF exist over SSH.
- [x] Agent can mirror the BRIGHT-Pro summary and PDF locally with SHA-256 provenance.
- [ ] A repo-local configuration format defines remote knowledge sources.
- [ ] A read-only local retrieval tool can fetch text summaries from configured roots.
- [ ] A read-only artifact fetch tool can copy a remote file with byte cap and hash verification.
- [ ] `smart_ingest` can store remote-file provenance metadata.
- [ ] BRIGHT-Pro eval setup can resolve required paper/corpus artifacts from local or remote sources.

## Implementation Slices

1. **Spec and manual mirror**: document flow, verify SSH, copy BRIGHT-Pro summary/PDF.
2. **Config and registry**: add remote source config parsing.
3. **Read-only fetch command/tool**: fetch summaries and artifacts with root allowlist + hash checks.
4. **Provenance ingest**: extend `smart_ingest` source metadata.
5. **BRIGHT-Pro adapter**: add eval harness resolver for local/remote corpus artifacts.
