//! Bounded, versioned discovery for the Ferrosa Memory MCP tool catalog.
//! Correctness: Correct when server-owned visibility and normalized selection
//! determine a stable page whose caller-visible encoding never exceeds the
//! protocol budget without constructing the complete catalog in memory.
//! Last revised: 2026-08-12
//! Last changed: Added lazy filtering, versioned cursors, projections, hints,
//! and surface-aware semantic byte packing.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::tool_schemas::{ToolRecord, tool_definition_records};

/// Maximum UTF-8 bytes in a caller-visible catalog result.
pub const MAX_CATALOG_RESPONSE_BYTES: usize = 16_384;
const MAX_CURSOR_BYTES: usize = 2_048;
const MAX_QUERY_BYTES: usize = 256;
const MAX_NAMES: usize = 20;
const MAX_NAMES_BYTES: usize = 4_096;
const MAX_CATEGORIES: usize = 16;
const CURSOR_CODEC_VERSION: u8 = 1;

/// Projection requested for catalog entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogDetail {
    /// Bounded discovery metadata without full JSON Schemas.
    Compact,
    /// Complete MCP tool definitions.
    Schema,
}

/// Protocol surface that owns the final response envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSurface {
    /// The `all_tools` MCP tool call.
    AllTools,
    /// Legacy MCP `tools/list`.
    LegacyToolsList,
    /// Modern MCP `tools/list`.
    ModernToolsList,
    /// Operator/workbench HTTP catalog.
    Operator,
}

/// Server-selected visibility policy for one catalog request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogVisibility {
    /// Compact default MCP tool surface.
    Tier1,
    /// Complete public discovery surface.
    Full,
    /// Authenticated operator discovery surface.
    Operator,
}

/// Typed catalog failure with machine-readable recovery fields.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    /// Caller arguments violate the bounded request contract.
    #[error("INVALID_CATALOG_ARGUMENTS: {0}")]
    InvalidArguments(String),
    /// Cursor cannot be decoded under the current codec.
    #[error("INVALID_CURSOR: {0}")]
    InvalidCursor(String),
    /// Cursor belongs to a different normalized request.
    #[error("CURSOR_QUERY_MISMATCH")]
    CursorQueryMismatch { restart_arguments: Value },
    /// Cursor belongs to an older effective catalog.
    #[error("STALE_CURSOR")]
    StaleCursor {
        current_version: String,
        restart_arguments: Value,
    },
    /// Exact named lookup contains an unknown public name.
    #[error("UNKNOWN_TOOL_NAME: {0}")]
    UnknownToolName(String),
    /// One complete definition cannot fit within the final response budget.
    #[error("ENTRY_TOO_LARGE: {name}")]
    EntryTooLarge { name: String },
    /// Serialization failed while enforcing the final response budget.
    #[error("CATALOG_SERIALIZATION_FAILED: {0}")]
    Serialization(String),
}

impl CatalogError {
    /// Structured error data suitable for MCP or HTTP adapters.
    pub fn data(&self) -> Value {
        match self {
            Self::CursorQueryMismatch { restart_arguments } => json!({
                "code": "CURSOR_QUERY_MISMATCH",
                "restart_arguments": restart_arguments,
                "hint": "Restart this catalog request with the supplied arguments."
            }),
            Self::StaleCursor {
                current_version,
                restart_arguments,
            } => json!({
                "code": "STALE_CURSOR",
                "catalog_version": current_version,
                "restart_arguments": restart_arguments,
                "hint": "The catalog changed. Restart without the stale cursor."
            }),
            Self::InvalidCursor(message) => json!({"code": "INVALID_CURSOR", "message": message}),
            Self::InvalidArguments(message) => {
                json!({"code": "INVALID_CATALOG_ARGUMENTS", "message": message})
            }
            Self::UnknownToolName(name) => json!({"code": "UNKNOWN_TOOL_NAME", "name": name}),
            Self::EntryTooLarge { name } => json!({"code": "ENTRY_TOO_LARGE", "name": name}),
            Self::Serialization(message) => {
                json!({"code": "CATALOG_SERIALIZATION_FAILED", "message": message})
            }
        }
    }
}

/// A normalized, server-authorized catalog request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogQuery {
    detail: CatalogDetail,
    surface: CatalogSurface,
    visibility: CatalogVisibility,
    query: Option<String>,
    categories: Vec<String>,
    names: Vec<String>,
    cursor: Option<String>,
}

impl CatalogQuery {
    /// Normalize caller arguments together with server-owned surface policy.
    pub fn for_surface(
        surface: CatalogSurface,
        visibility: CatalogVisibility,
        arguments: Value,
    ) -> Result<Self, CatalogError> {
        let args = match arguments {
            Value::Null => Map::new(),
            Value::Object(map) => map,
            _ => {
                return Err(CatalogError::InvalidArguments(
                    "catalog arguments must be an object".into(),
                ));
            }
        };
        let mut detail = match args.get("detail").and_then(Value::as_str) {
            None if matches!(surface, CatalogSurface::AllTools | CatalogSurface::Operator) => {
                CatalogDetail::Compact
            }
            None => CatalogDetail::Schema,
            Some("compact") => CatalogDetail::Compact,
            Some("schema") => CatalogDetail::Schema,
            Some(other) => {
                return Err(CatalogError::InvalidArguments(format!(
                    "unsupported catalog detail: {other}"
                )));
            }
        };
        let query = bounded_string(args.get("query"), "query", MAX_QUERY_BYTES)?
            .map(|value| value.to_lowercase());
        let categories = bounded_strings(args.get("categories"), "categories", MAX_CATEGORIES)?;
        let names = bounded_strings(args.get("names"), "names", MAX_NAMES)?;
        if names.iter().map(String::len).sum::<usize>() > MAX_NAMES_BYTES {
            return Err(CatalogError::InvalidArguments(
                "names exceed the aggregate byte limit".into(),
            ));
        }
        if !names.is_empty() {
            if query.is_some() || !categories.is_empty() {
                return Err(CatalogError::InvalidArguments(
                    "names cannot be combined with query or categories".into(),
                ));
            }
            detail = CatalogDetail::Schema;
        }
        let cursor = bounded_string(args.get("cursor"), "cursor", MAX_CURSOR_BYTES)?;
        Ok(Self {
            detail,
            surface,
            visibility,
            query,
            categories,
            names,
            cursor,
        })
    }

    /// Return the normalized projection.
    pub fn detail(&self) -> CatalogDetail {
        self.detail
    }

    /// Return the server-selected protocol surface.
    pub fn surface(&self) -> CatalogSurface {
        self.surface
    }

    /// Return the server-selected visibility policy.
    pub fn visibility(&self) -> CatalogVisibility {
        self.visibility
    }

    fn restart_arguments(&self) -> Value {
        let mut args = Map::new();
        args.insert("detail".into(), json!(self.detail));
        if matches!(
            self.surface,
            CatalogSurface::LegacyToolsList | CatalogSurface::ModernToolsList
        ) && matches!(self.visibility, CatalogVisibility::Full)
        {
            args.insert("include_all".into(), Value::Bool(true));
        }
        if let Some(query) = &self.query {
            args.insert("query".into(), json!(query));
        }
        if !self.categories.is_empty() {
            args.insert("categories".into(), json!(self.categories));
        }
        if !self.names.is_empty() {
            args.insert("names".into(), json!(self.names));
        }
        Value::Object(args)
    }

    fn fingerprint(&self) -> Result<String, CatalogError> {
        let value = json!({
            "surface": self.surface,
            "visibility": self.visibility,
            "detail": self.detail,
            "query": self.query,
            "categories": self.categories,
            "names": self.names,
        });
        digest_serialized(&value)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CatalogCursor {
    codec: u8,
    catalog_version: String,
    after: usize,
    query_fingerprint: String,
}

/// Build one bounded semantic page using a family-lazy catalog traversal.
pub fn build_catalog_page(
    entity_types: &[String],
    query: &CatalogQuery,
) -> Result<Value, CatalogError> {
    let catalog_version = catalog_version(entity_types)?;
    let fingerprint = query.fingerprint()?;
    let after = decode_cursor(query, &catalog_version, &fingerprint)?;
    let mut tools = Vec::new();
    let mut last_admitted = after;
    let mut has_more = false;

    // Validate exact names independently of page position. Otherwise a name
    // emitted on page one would appear "missing" when page two completes.
    if !query.names.is_empty() {
        let mut missing: BTreeSet<&str> = query.names.iter().map(String::as_str).collect();
        for record in tool_definition_records(entity_types) {
            missing.remove(record.tool.name.as_str());
            if missing.is_empty() {
                break;
            }
        }
        if let Some(name) = missing.first() {
            return Err(CatalogError::UnknownToolName((*name).to_string()));
        }
    }

    for (index, record) in tool_definition_records(entity_types).enumerate() {
        if index <= after.unwrap_or(usize::MAX) && after.is_some() {
            continue;
        }
        if !eligible(&record, query) {
            continue;
        }
        let entry = project(&record, query.detail)?;
        let mut candidate_tools = tools.clone();
        candidate_tools.push(entry);
        let candidate = page_value(
            query,
            &catalog_version,
            &candidate_tools,
            true,
            Some(index),
            &fingerprint,
        )?;
        if encoded_len(query.surface, &candidate)? > MAX_CATALOG_RESPONSE_BYTES {
            if tools.is_empty() {
                return Err(CatalogError::EntryTooLarge {
                    name: record.tool.name,
                });
            }
            has_more = true;
            break;
        }
        tools = candidate_tools;
        last_admitted = Some(index);
    }

    let page = page_value(
        query,
        &catalog_version,
        &tools,
        has_more,
        last_admitted,
        &fingerprint,
    )?;
    let final_bytes = encoded_len(query.surface, &page)?;
    if final_bytes > MAX_CATALOG_RESPONSE_BYTES {
        return Err(CatalogError::Serialization(
            "final catalog result exceeds 16 KiB".into(),
        ));
    }
    tracing::debug!(
        surface = ?query.surface,
        detail = ?query.detail,
        entries = tools.len(),
        final_bytes,
        has_more,
        "built bounded tool catalog page"
    );
    Ok(page)
}

/// Wrap an `all_tools` catalog page without duplicating every schema in the
/// text fallback and `structuredContent`.
///
/// Text-only MCP clients receive the complete bounded page. Structured-result
/// clients receive the same navigation metadata and public names without a
/// second copy of every schema.
pub(super) fn wrap_all_tools_page(
    page: &Value,
    requested_name: &str,
    duration_ms: u64,
) -> Result<Value, CatalogError> {
    let tool_names = page
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let text = serde_json::to_string(page)
        .map_err(|error| CatalogError::Serialization(error.to_string()))?;
    let structured = json!({
        "tool": "all_tools",
        "requested_tool": requested_name,
        "duration_ms": duration_ms,
        "is_error": false,
        "catalog_version": page.get("catalog_version").cloned().unwrap_or(Value::Null),
        "detail": page.get("detail").cloned().unwrap_or(Value::Null),
        "tool_names": tool_names,
        "has_more": page.get("has_more").cloned().unwrap_or(Value::Bool(false)),
        "next_cursor": page.get("next_cursor").cloned().unwrap_or(Value::Null),
        "hint": page.get("hint").cloned().unwrap_or(Value::Null),
        "_meta": page.get("_meta").cloned().unwrap_or_else(|| json!({})),
    });
    Ok(json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured,
    }))
}

fn bounded_string(
    value: Option<&Value>,
    name: &str,
    maximum: usize,
) -> Result<Option<String>, CatalogError> {
    let Some(value) = value else { return Ok(None) };
    let text = value
        .as_str()
        .ok_or_else(|| CatalogError::InvalidArguments(format!("{name} must be a string")))?
        .trim();
    if text.is_empty() || text.len() > maximum {
        return Err(CatalogError::InvalidArguments(format!(
            "{name} must contain 1..={maximum} UTF-8 bytes"
        )));
    }
    Ok(Some(text.to_string()))
}

fn bounded_strings(
    value: Option<&Value>,
    name: &str,
    maximum: usize,
) -> Result<Vec<String>, CatalogError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| CatalogError::InvalidArguments(format!("{name} must be an array")))?;
    if array.len() > maximum {
        return Err(CatalogError::InvalidArguments(format!(
            "{name} may contain at most {maximum} entries"
        )));
    }
    let mut seen = BTreeSet::new();
    let mut result = Vec::with_capacity(array.len());
    for value in array {
        let item = value
            .as_str()
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .ok_or_else(|| {
                CatalogError::InvalidArguments(format!("{name} entries must be non-empty strings"))
            })?;
        let normalized = item.to_lowercase();
        if !seen.insert(normalized.clone()) {
            return Err(CatalogError::InvalidArguments(format!(
                "{name} contains a duplicate entry"
            )));
        }
        result.push(normalized);
    }
    Ok(result)
}

fn eligible(record: &ToolRecord, query: &CatalogQuery) -> bool {
    if matches!(query.visibility, CatalogVisibility::Tier1) && !super::is_tier1(&record.tool.name) {
        return false;
    }
    if !query.names.is_empty() && !query.names.iter().any(|name| name == &record.tool.name) {
        return false;
    }
    if !query.categories.is_empty()
        && !query
            .categories
            .iter()
            .any(|category| category == record.category)
    {
        return false;
    }
    let Some(needle) = &query.query else {
        return true;
    };
    record.tool.name.to_lowercase().contains(needle)
        || record.category.contains(needle)
        || summary(&record.tool.description)
            .to_lowercase()
            .contains(needle)
}

fn project(record: &ToolRecord, detail: CatalogDetail) -> Result<Value, CatalogError> {
    match detail {
        CatalogDetail::Compact => Ok(json!({
            "name": record.tool.name,
            "category": record.category,
            "summary": summary(&record.tool.description),
            "schema_digest": digest_serialized(&record.tool)?,
            // The one part of the schema a caller cannot do without.
            //
            // A compact entry carries no inputSchema on purpose -- the whole
            // point is to describe 100+ tools inside a token budget, and the
            // digest plus a `detail=schema` follow-up is how a client gets the
            // rest. But that left the compact catalog saying nothing about
            // which arguments are mandatory, and "absent" is read by clients as
            // "none": a caller who trusted the listing called `ingest` without
            // `content` and got -32602 "missing required string: content", an
            // error the listing said could not happen. Worse, the call fails at
            // STORE time, so every retrieval test after it returns zero for the
            // wrong reason (QA-0068).
            //
            // Required names only -- no types, no descriptions, no properties.
            // That is a handful of tokens per tool and it is the difference
            // between a listing a client can call from and one it cannot.
            "required": required_fields(&record.tool.input_schema),
        })),
        CatalogDetail::Schema => serde_json::to_value(&record.tool)
            .map_err(|error| CatalogError::Serialization(error.to_string())),
    }
}

/// The `required` array from a tool's input schema, as an array (empty if none).
///
/// Always an array, never null. A missing key and an empty list mean different
/// things to a caller -- "I was not told" versus "nothing is mandatory" -- and
/// this projection is the one place that distinction is decided. Emitting the
/// empty array says the second, truthfully, for the tools where it is true.
fn required_fields(input_schema: &Value) -> Value {
    input_schema
        .get("required")
        .filter(|value| value.is_array())
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()))
}

fn summary(description: &str) -> String {
    let first = description.lines().next().unwrap_or(description).trim();
    let end = first
        .char_indices()
        .find_map(|(index, ch)| (ch == '.' && index >= 24).then_some(index + 1))
        .unwrap_or(first.len());
    let sentence = &first[..end];
    let mut chars = sentence.chars();
    let mut bounded: String = chars.by_ref().take(240).collect();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

fn page_value(
    query: &CatalogQuery,
    catalog_version: &str,
    tools: &[Value],
    has_more: bool,
    last_admitted: Option<usize>,
    fingerprint: &str,
) -> Result<Value, CatalogError> {
    let cursor = if has_more {
        Some(encode_cursor(CatalogCursor {
            codec: CURSOR_CODEC_VERSION,
            catalog_version: catalog_version.to_string(),
            after: last_admitted.ok_or_else(|| {
                CatalogError::Serialization("continuation page made no progress".into())
            })?,
            query_fingerprint: fingerprint.to_string(),
        })?)
    } else {
        None
    };
    let mut next_arguments = query.restart_arguments();
    if let (Some(cursor), Some(args)) = (&cursor, next_arguments.as_object_mut()) {
        args.insert("cursor".into(), json!(cursor));
    }
    let hint = if has_more {
        json!({
            "message": "Continue this catalog traversal with the exact arguments below.",
            "next_arguments": next_arguments
        })
    } else {
        json!({
            "message": "Catalog traversal complete. Request schema detail by exact public name.",
            "schema_lookup_arguments": {"detail": "schema", "names": ["tool_name"]}
        })
    };
    let mut page = json!({
        "catalog_version": catalog_version,
        "detail": query.detail,
        "tools": tools,
        "has_more": has_more,
        "hint": hint,
        "_meta": {
            "catalogVersion": catalog_version,
            "paginationHint": hint
        }
    });
    if let Some(map) = page.as_object_mut()
        && let Some(cursor) = cursor
    {
        let field = if matches!(
            query.surface,
            CatalogSurface::LegacyToolsList | CatalogSurface::ModernToolsList
        ) {
            "nextCursor"
        } else {
            "next_cursor"
        };
        map.insert(field.into(), json!(cursor));
    }
    Ok(page)
}

fn encode_cursor(cursor: CatalogCursor) -> Result<String, CatalogError> {
    let bytes = serde_json::to_vec(&cursor)
        .map_err(|error| CatalogError::Serialization(error.to_string()))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_cursor(
    query: &CatalogQuery,
    current_version: &str,
    fingerprint: &str,
) -> Result<Option<usize>, CatalogError> {
    let Some(encoded) = &query.cursor else {
        return Ok(None);
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| CatalogError::InvalidCursor("cursor is not valid Base64URL".into()))?;
    let cursor: CatalogCursor = serde_json::from_slice(&bytes)
        .map_err(|_| CatalogError::InvalidCursor("cursor payload is invalid".into()))?;
    if cursor.codec != CURSOR_CODEC_VERSION {
        return Err(CatalogError::InvalidCursor(
            "unsupported cursor codec".into(),
        ));
    }
    if cursor.catalog_version != current_version {
        return Err(CatalogError::StaleCursor {
            current_version: current_version.to_string(),
            restart_arguments: query.restart_arguments(),
        });
    }
    if cursor.query_fingerprint != fingerprint {
        return Err(CatalogError::CursorQueryMismatch {
            restart_arguments: query.restart_arguments(),
        });
    }
    Ok(Some(cursor.after))
}

fn add_modern_envelope(mut result: Value, cacheable: bool) -> Value {
    let Some(map) = result.as_object_mut() else {
        return result;
    };
    map.insert("resultType".into(), json!("complete"));
    if cacheable {
        map.insert("ttlMs".into(), json!(30_000));
        map.insert("cacheScope".into(), json!("private"));
    }
    let meta = map.entry("_meta").or_insert_with(|| json!({}));
    if !meta.is_object() {
        *meta = json!({});
    }
    if let Some(meta) = meta.as_object_mut() {
        meta.insert(
            "io.modelcontextprotocol/serverInfo".into(),
            json!({
                "name": "ferrosa-memory-mcp",
                "version": env!("CARGO_PKG_VERSION")
            }),
        );
    }
    result
}

fn encoded_len(surface: CatalogSurface, page: &Value) -> Result<usize, CatalogError> {
    let result = match surface {
        CatalogSurface::AllTools => {
            // `tools/call` may be served by the modern protocol adapter. Model
            // that larger envelope so both modern and legacy responses fit.
            let result = wrap_all_tools_page(page, "all_tools", u64::MAX)?;
            add_modern_envelope(result, false)
        }
        CatalogSurface::ModernToolsList => add_modern_envelope(page.clone(), true),
        CatalogSurface::LegacyToolsList | CatalogSurface::Operator => page.clone(),
    };
    serde_json::to_vec(&result)
        .map(|bytes| bytes.len())
        .map_err(|error| CatalogError::Serialization(error.to_string()))
}

fn digest_serialized<T: Serialize>(value: &T) -> Result<String, CatalogError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| CatalogError::Serialization(error.to_string()))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

fn catalog_version(entity_types: &[String]) -> Result<String, CatalogError> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let key = digest_serialized(&entity_types)?;
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(version) = cache
        .lock()
        .map_err(|_| CatalogError::Serialization("catalog version cache poisoned".into()))?
        .get(&key)
        .cloned()
    {
        return Ok(version);
    }
    let mut hasher = Sha256::new();
    for record in tool_definition_records(entity_types) {
        serde_json::to_writer(HashWriter(&mut hasher), &record.tool)
            .map_err(|error| CatalogError::Serialization(error.to_string()))?;
    }
    let version = format!("sha256:{}", hex::encode(hasher.finalize()));
    cache
        .lock()
        .map_err(|_| CatalogError::Serialization("catalog version cache poisoned".into()))?
        .insert(key, version.clone());
    Ok(version)
}

struct HashWriter<'a>(&'a mut Sha256);

impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_rejects_changed_query() {
        let types = vec!["person".to_string()];
        let first = CatalogQuery::for_surface(
            CatalogSurface::AllTools,
            CatalogVisibility::Full,
            json!({"detail": "schema"}),
        )
        .unwrap();
        let page = build_catalog_page(&types, &first).unwrap();
        let cursor = page["next_cursor"]
            .as_str()
            .expect("schema catalog spans more than one bounded page");
        let changed = CatalogQuery::for_surface(
            CatalogSurface::AllTools,
            CatalogVisibility::Full,
            json!({"query": "graph", "cursor": cursor}),
        )
        .unwrap();
        assert!(matches!(
            build_catalog_page(&types, &changed),
            Err(CatalogError::CursorQueryMismatch { .. })
        ));
    }

    #[test]
    fn cursor_rejects_changed_catalog_with_restart_hint() {
        let first = CatalogQuery::for_surface(
            CatalogSurface::AllTools,
            CatalogVisibility::Full,
            json!({"detail": "schema"}),
        )
        .unwrap();
        let page = build_catalog_page(&["person".into()], &first).unwrap();
        let cursor = page["next_cursor"].as_str().unwrap();
        let resumed = CatalogQuery::for_surface(
            CatalogSurface::AllTools,
            CatalogVisibility::Full,
            json!({"detail": "schema", "cursor": cursor}),
        )
        .unwrap();
        let error = build_catalog_page(&["person".into(), "project".into()], &resumed)
            .expect_err("effective schema changes stale the cursor");
        let data = error.data();
        assert_eq!(data["code"], "STALE_CURSOR");
        assert_eq!(data["restart_arguments"], json!({"detail": "schema"}));
        assert!(data["hint"].as_str().unwrap().contains("Restart"));
    }

    #[test]
    fn named_lookup_returns_only_requested_schema() {
        let query = CatalogQuery::for_surface(
            CatalogSurface::AllTools,
            CatalogVisibility::Full,
            json!({"detail": "schema", "names": ["stats"]}),
        )
        .unwrap();
        let page = build_catalog_page(&[], &query).unwrap();
        assert_eq!(page["tools"].as_array().unwrap().len(), 1);
        assert_eq!(page["tools"][0]["name"], "stats");
        assert!(page["tools"][0]["inputSchema"].is_object());
    }

    /// A compact listing must say which arguments are mandatory.
    ///
    /// The compact projection carries no inputSchema -- that is the whole point
    /// of it -- but it also said nothing about required fields, and clients read
    /// "absent" as "none". A caller that trusted the listing called `ingest`
    /// without `content` and got -32602 "missing required string: content", an
    /// error the listing said could not happen. Worse, the call fails at STORE
    /// time, so every retrieval check after it returns zero for the wrong
    /// reason (QA-0068).
    #[test]
    fn a_compact_listing_names_the_arguments_a_caller_must_supply() {
        let query = CatalogQuery::for_surface(
            CatalogSurface::AllTools,
            CatalogVisibility::Full,
            json!({"detail": "compact", "query": "ingest"}),
        )
        .unwrap();
        let page = build_catalog_page(&[], &query).unwrap();
        let tools = page["tools"].as_array().expect("tools array");

        let ingest = tools
            .iter()
            .find(|tool| tool["name"] == "ingest")
            .expect("the compact catalog must list `ingest`");

        // Still no schema: the digest plus a detail=schema follow-up is how a
        // client gets the rest, and that stays true.
        assert!(
            ingest.get("inputSchema").is_none(),
            "compact entries must not carry a full schema: {ingest:?}"
        );
        assert!(ingest["schema_digest"].is_string());

        let required: Vec<&str> = ingest["required"]
            .as_array()
            .expect("a compact entry must carry a `required` array, even when empty")
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        assert!(
            required.contains(&"content"),
            "`content` is rejected when omitted, so the listing must say it is required: {required:?}"
        );
    }

    /// Always an array, never a missing key.
    ///
    /// "I was not told" and "nothing is mandatory" are different answers, and a
    /// caller cannot tell them apart from an absent field. Every compact entry
    /// answers the question, including the tools whose answer is "none".
    #[test]
    fn every_compact_entry_answers_the_required_question() {
        let query = CatalogQuery::for_surface(
            CatalogSurface::AllTools,
            CatalogVisibility::Full,
            json!({"detail": "compact"}),
        )
        .unwrap();
        let page = build_catalog_page(&[], &query).unwrap();
        let tools = page["tools"].as_array().expect("tools array");
        assert!(!tools.is_empty(), "an empty page would pass this vacuously");
        for tool in tools {
            assert!(
                tool["required"].is_array(),
                "{} has no `required` array",
                tool["name"]
            );
        }
    }

    #[test]
    fn largest_named_schema_fits_after_text_deduplication() {
        let query = CatalogQuery::for_surface(
            CatalogSurface::AllTools,
            CatalogVisibility::Full,
            json!({"names": ["search"]}),
        )
        .unwrap();
        let page = build_catalog_page(&[], &query).unwrap();
        let wrapped = wrap_all_tools_page(&page, "all_tools", u64::MAX).unwrap();
        assert_eq!(page["tools"][0]["name"], "search");
        assert!(encoded_len(CatalogSurface::AllTools, &page).unwrap() <= 16_384);
        assert!(
            wrapped["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("inputSchema")
        );
        assert!(wrapped["structuredContent"]["tools"].is_null());
        assert_eq!(wrapped["structuredContent"]["tool_names"][0], "search");
    }

    #[test]
    fn unknown_named_lookup_fails_before_returning_a_page() {
        let query = CatalogQuery::for_surface(
            CatalogSurface::AllTools,
            CatalogVisibility::Full,
            json!({"names": ["does_not_exist"]}),
        )
        .unwrap();
        assert!(matches!(
            build_catalog_page(&[], &query),
            Err(CatalogError::UnknownToolName(name)) if name == "does_not_exist"
        ));
    }

    #[test]
    fn all_tools_pages_fit_final_call_result_budget() {
        let query =
            CatalogQuery::for_surface(CatalogSurface::AllTools, CatalogVisibility::Full, json!({}))
                .unwrap();
        let page = build_catalog_page(&[], &query).unwrap();
        assert!(encoded_len(CatalogSurface::AllTools, &page).unwrap() <= 16_384);
        assert!(page["hint"].is_object());
    }
}
