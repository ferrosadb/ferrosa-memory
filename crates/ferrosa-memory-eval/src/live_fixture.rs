//! Live MCP-backed fixture runner.
//!
//! This adapter loads benchmark fixtures into a real Ferrosa MCP HTTP endpoint,
//! then scores retrieval through the same fixture contracts used by the
//! deterministic lexical runner.

use std::collections::HashMap;

use anyhow::{Context, anyhow};
use serde_json::Value;
use uuid::Uuid;

use crate::fixture::{
    BrightProFixture, BrightProFixtureResult, CorpusDocument, FixtureHit, FixtureRetriever,
    run_bright_pro_fixture,
};
use crate::mcp_client::{HttpMcpClient, ToolCallResult};
use crate::memorybench::{MemoryBenchFixture, MemoryBenchResult, run_memorybench_fixture};

/// Runs corpus-backed fixtures against a live MCP HTTP endpoint.
#[derive(Debug)]
pub struct LiveMcpFixtureRunner {
    client: HttpMcpClient,
    session_id: Uuid,
    documents_by_entity_id: HashMap<String, CorpusDocument>,
}

impl LiveMcpFixtureRunner {
    pub fn new(client: HttpMcpClient, session_id: Uuid) -> Self {
        Self {
            client,
            session_id,
            documents_by_entity_id: HashMap::new(),
        }
    }

    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    /// Verify the endpoint is reachable before mutating fixture state.
    pub async fn initialize(&mut self) -> anyhow::Result<()> {
        self.client
            .initialize()
            .await
            .with_context(|| format!("initialize MCP endpoint {}", self.client.url()))?;
        Ok(())
    }

    /// Ingest each fixture document as one memory entity and remember the
    /// returned entity UUID so live search hits can be scored by fixture ID.
    pub async fn ingest_corpus(&mut self, documents: &[CorpusDocument]) -> anyhow::Result<()> {
        for document in documents {
            let entity_name = format!("{}::{}", self.session_id, document.id);
            let response = self
                .client
                .call_tool(
                    "smart_ingest",
                    serde_json::json!({
                        "session_id": self.session_id.to_string(),
                        "content": document.text.as_str(),
                        "entity_type": document.metadata.get("entity_type").map(String::as_str).unwrap_or("benchmark_memory"),
                        "entity_name": entity_name,
                    }),
                )
                .await
                .with_context(|| format!("smart_ingest failed for fixture document {}", document.id))?;
            let value = tool_payload_json(&response)?;
            let entity_id = extract_ingest_entity_id(&value)
                .with_context(|| format!("smart_ingest response missing entity id: {value}"))?;
            self.documents_by_entity_id
                .entry(entity_id)
                .or_insert_with(|| document.clone());
        }
        Ok(())
    }

    pub async fn retrieve(&mut self, query: &str, k: usize) -> anyhow::Result<Vec<FixtureHit>> {
        let response = self
            .client
            .call_tool(
                "hybrid_search",
                serde_json::json!({
                    "session_id": self.session_id.to_string(),
                    "query": query,
                    "limit": k,
                    "scope": "session",
                }),
            )
            .await
            .with_context(|| format!("hybrid_search failed for query {query:?}"))?;
        let value = tool_payload_json(&response)?;
        let results = value
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("hybrid_search response missing results array: {value}"))?;

        let mut hits = Vec::with_capacity(results.len());
        for (idx, result) in results.iter().enumerate() {
            let entity_id = result
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let score = result
                .get("score")
                .and_then(Value::as_f64)
                .unwrap_or_default();
            let fallback_text = result
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mapped = self.documents_by_entity_id.get(&entity_id);
            hits.push(FixtureHit {
                id: mapped
                    .map(|document| document.id.clone())
                    .unwrap_or_else(|| entity_id.clone()),
                text: mapped
                    .map(|document| document.text.clone())
                    .unwrap_or(fallback_text),
                score,
                rank: idx + 1,
            });
        }
        Ok(hits)
    }
}

pub async fn run_bright_pro_fixture_live(
    fixture: &BrightProFixture,
    runner: &mut LiveMcpFixtureRunner,
    k: usize,
) -> anyhow::Result<BrightProFixtureResult> {
    runner.ingest_corpus(&fixture.corpus).await?;
    let hits = runner.retrieve(&fixture.query, k).await?;
    let retriever = ReplayFixtureRetriever::single(fixture.query.clone(), hits);
    Ok(run_bright_pro_fixture(fixture, &retriever, k))
}

pub async fn run_memorybench_fixture_live(
    fixture: &MemoryBenchFixture,
    runner: &mut LiveMcpFixtureRunner,
    k: usize,
) -> anyhow::Result<MemoryBenchResult> {
    runner.ingest_corpus(&fixture.corpus_documents()).await?;
    let mut hits_by_query = HashMap::new();
    for case in &fixture.cases {
        hits_by_query.insert(case.query.clone(), runner.retrieve(&case.query, k).await?);
    }
    let retriever = ReplayFixtureRetriever { hits_by_query };
    Ok(run_memorybench_fixture(fixture, &retriever, k))
}

#[derive(Debug, Default)]
struct ReplayFixtureRetriever {
    hits_by_query: HashMap<String, Vec<FixtureHit>>,
}

impl ReplayFixtureRetriever {
    fn single(query: String, hits: Vec<FixtureHit>) -> Self {
        Self {
            hits_by_query: HashMap::from([(query, hits)]),
        }
    }
}

impl FixtureRetriever for ReplayFixtureRetriever {
    fn retrieve(&self, query: &str, k: usize) -> Vec<FixtureHit> {
        let mut hits = self.hits_by_query.get(query).cloned().unwrap_or_default();
        hits.truncate(k);
        hits
    }
}

fn tool_payload_json(response: &ToolCallResult) -> anyhow::Result<Value> {
    if let Some(text) = response
        .response
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
    {
        return serde_json::from_str(text).context("tool content text was not JSON");
    }
    Ok(response.response.clone())
}

fn extract_ingest_entity_id(value: &Value) -> Option<String> {
    ["entity_id", "new_entity_id", "existing_entity_id"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str).map(str::to_string))
}
