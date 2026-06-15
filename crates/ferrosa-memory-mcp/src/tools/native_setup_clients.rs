//! Module: Native setup client configuration previews.
#![allow(dead_code)]
//! Correctness: Correct when supported clients, tool profiles, and rendered snippets stay bounded and side-effect free under unit tests.
//! Last revised: 2026-06-15
//! Last changed: Added data-only client metadata and preview rendering for the native setup CLI.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

/// Stable server key used in generated client snippets.
pub const SERVER_KEY: &str = "ferrosa-memory";

/// Maximum number of custom tool names the setup CLI will carry in memory.
pub const MAX_CUSTOM_TOOLS: usize = 64;

/// Maximum byte length for an individual MCP tool name.
pub const MAX_TOOL_NAME_BYTES: usize = 128;

/// Maximum byte length for a rendered path in a preview.
pub const MAX_PATH_BYTES: usize = 4096;

/// Maximum byte length for a generated snippet.
pub const MAX_SNIPPET_BYTES: usize = 64 * 1024;

/// Compact default tools for ordinary memory use.
///
/// These are the short names advertised by `tools/list`, not the canonical
/// dispatch names. Client-side allow-lists generally filter discovery output.
pub const RECOMMENDED_TOOLS: &[&str] = &[
    "all_tools",
    "ingest",
    "search",
    "chunk_ctx",
    "check",
    "feedback",
    "stats",
    "find",
    "list",
    "forget",
];

/// Full native setup allow-list. This mirrors the MCP server's public tool
/// catalog without importing dispatch code into the setup helper.
pub const FULL_TOOLS: &[&str] = &[
    "all_tools",
    "ctx_ingest",
    "ctx_search",
    "ctx_window",
    "turn_chain",
    "chunk_ctx",
    "memo",
    "memo_store",
    "plan_write",
    "plan",
    "plan_update",
    "task_put",
    "task_get",
    "task_current",
    "task_list",
    "task_done",
    "task_cancel",
    "task_focus",
    "task_observe",
    "fold_start",
    "fold_append",
    "fold_done",
    "fold",
    "upsert",
    "ingest_batch",
    "ingest_many",
    "edge",
    "edges_add",
    "edges_update",
    "edges_delete",
    "entities_update",
    "entities_delete",
    "find",
    "list",
    "outcome",
    "feedback",
    "config",
    "delete_session",
    "ingest",
    "skill_ingest",
    "skills",
    "skill",
    "tag_parent",
    "skill_verify",
    "intend",
    "check",
    "done",
    "intentions",
    "snooze",
    "fact",
    "history",
    "explore",
    "search",
    "authority",
    "consolidate",
    "enrich",
    "stats",
    "metrics",
    "migrations",
    "describe",
    "forget",
    "restore",
    "type_counts",
    "promote",
    "demote",
    "importance",
    "chain",
    "predict",
    "spread",
    "duplicates",
    "recurse",
    "derive",
    "rules",
    "claims",
    "approvals",
    "aliases",
    "explain",
    "ruleset",
    "pred_promote",
    "derived_cache",
];

const CLIENTS: &[Client] = &[
    Client::ClaudeCode,
    Client::Codex,
    Client::ClaudeDesktop,
    Client::Zed,
    Client::Hermes,
];

const TOOL_PROFILES: &[ToolProfileMetadata] = &[
    ToolProfileMetadata {
        id: ToolProfileId::Recommended,
        name: "recommended",
        label: "Recommended",
        description: "Compact memory surface for routine agent sessions.",
    },
    ToolProfileMetadata {
        id: ToolProfileId::Full,
        name: "full",
        label: "Full",
        description: "Every public Ferrosa Memory MCP tool, including administrative and graph surfaces.",
    },
    ToolProfileMetadata {
        id: ToolProfileId::Custom,
        name: "custom",
        label: "Custom",
        description: "Caller-selected bounded include list.",
    },
];

/// First-class clients the native setup CLI can preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Client {
    ClaudeCode,
    Codex,
    ClaudeDesktop,
    Zed,
    Hermes,
}

/// Metadata used by setup prompts and preview labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientMetadata {
    pub client: Client,
    pub id: &'static str,
    pub display_name: &'static str,
    pub default_config_hint: &'static str,
    pub snippet_format: SnippetFormat,
    pub supports_tool_include: bool,
}

/// Configuration snippet format used by a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnippetFormat {
    Json,
    Toml,
    Yaml,
}

/// Scope selected by the setup wizard for client configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigScope {
    Global,
    Project,
    Both,
}

/// Stable profile identifiers for UI data and saved setup choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolProfileId {
    Recommended,
    Full,
    Custom,
}

/// Metadata for the setup wizard's tool-profile choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolProfileMetadata {
    pub id: ToolProfileId,
    pub name: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

/// Runtime tool-profile selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolProfile<'a> {
    Recommended,
    Full,
    Custom(&'a [&'a str]),
}

/// Input required to render a side-effect-free client config preview.
#[derive(Debug, Clone, Copy)]
pub struct PreviewRequest<'a> {
    pub install_root: &'a Path,
    pub config_path: &'a Path,
    pub binary_path: &'a Path,
    pub scope: ConfigScope,
    pub tool_profile: ToolProfile<'a>,
}

/// Side-effect-free preview for one client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfigPreview {
    pub client: Client,
    pub client_id: &'static str,
    pub display_name: &'static str,
    pub config_path: PathBuf,
    pub install_root: PathBuf,
    pub binary_path: PathBuf,
    pub scope: ConfigScope,
    pub snippet_format: SnippetFormat,
    pub snippet: String,
    pub selected_tools: Vec<String>,
    pub tool_include_applied: bool,
    pub note: &'static str,
}

/// Validation or rendering failure for preview generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewError {
    EmptyPath {
        field: &'static str,
    },
    PathTooLong {
        field: &'static str,
        len: usize,
        max: usize,
    },
    EmptyCustomProfile,
    TooManyCustomTools {
        count: usize,
        max: usize,
    },
    EmptyToolName {
        index: usize,
    },
    ToolNameTooLong {
        name: String,
        len: usize,
        max: usize,
    },
    InvalidToolName {
        name: String,
    },
    DuplicateToolName {
        name: String,
    },
    SnippetTooLarge {
        len: usize,
        max: usize,
    },
}

impl Client {
    /// Return metadata for this first-class setup client.
    pub const fn metadata(self) -> ClientMetadata {
        match self {
            Client::ClaudeCode => ClientMetadata {
                client: self,
                id: "claude_code",
                display_name: "Claude Code",
                default_config_hint: "~/.claude.json or project .mcp.json",
                snippet_format: SnippetFormat::Json,
                supports_tool_include: false,
            },
            Client::Codex => ClientMetadata {
                client: self,
                id: "codex",
                display_name: "Codex",
                default_config_hint: "~/.codex/config.toml or project .codex/config.toml",
                snippet_format: SnippetFormat::Toml,
                supports_tool_include: false,
            },
            Client::ClaudeDesktop => ClientMetadata {
                client: self,
                id: "claude_desktop",
                display_name: "Claude Desktop",
                default_config_hint: "~/Library/Application Support/Claude/claude_desktop_config.json",
                snippet_format: SnippetFormat::Json,
                supports_tool_include: false,
            },
            Client::Zed => ClientMetadata {
                client: self,
                id: "zed",
                display_name: "Zed",
                default_config_hint: "~/.config/zed/settings.json",
                snippet_format: SnippetFormat::Json,
                supports_tool_include: false,
            },
            Client::Hermes => ClientMetadata {
                client: self,
                id: "hermes",
                display_name: "Hermes",
                default_config_hint: "~/.hermes/config.yaml",
                snippet_format: SnippetFormat::Yaml,
                supports_tool_include: true,
            },
        }
    }
}

impl ConfigScope {
    /// Stable identifier for saved setup choices.
    pub const fn id(self) -> &'static str {
        match self {
            ConfigScope::Global => "global",
            ConfigScope::Project => "project",
            ConfigScope::Both => "both",
        }
    }
}

impl<'a> ToolProfile<'a> {
    /// Stable profile identifier.
    pub const fn id(self) -> ToolProfileId {
        match self {
            ToolProfile::Recommended => ToolProfileId::Recommended,
            ToolProfile::Full => ToolProfileId::Full,
            ToolProfile::Custom(_) => ToolProfileId::Custom,
        }
    }
}

impl fmt::Display for PreviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PreviewError::EmptyPath { field } => write!(f, "{field} path must not be empty"),
            PreviewError::PathTooLong { field, len, max } => {
                write!(f, "{field} path is {len} bytes, exceeding {max}")
            }
            PreviewError::EmptyCustomProfile => {
                write!(f, "custom tool profile must include at least one tool")
            }
            PreviewError::TooManyCustomTools { count, max } => {
                write!(f, "custom tool profile has {count} tools, exceeding {max}")
            }
            PreviewError::EmptyToolName { index } => {
                write!(f, "custom tool name at index {index} must not be empty")
            }
            PreviewError::ToolNameTooLong { name, len, max } => {
                write!(f, "tool name {name:?} is {len} bytes, exceeding {max}")
            }
            PreviewError::InvalidToolName { name } => {
                write!(
                    f,
                    "tool name {name:?} must use ASCII letters, numbers, '_' or '-'"
                )
            }
            PreviewError::DuplicateToolName { name } => {
                write!(f, "custom tool profile contains duplicate tool {name:?}")
            }
            PreviewError::SnippetTooLarge { len, max } => {
                write!(f, "generated snippet is {len} bytes, exceeding {max}")
            }
        }
    }
}

impl Error for PreviewError {}

/// Return all first-class clients in prompt order.
pub const fn supported_clients() -> &'static [Client] {
    CLIENTS
}

/// Return static metadata for the setup wizard's tool-profile choices.
pub const fn tool_profiles() -> &'static [ToolProfileMetadata] {
    TOOL_PROFILES
}

/// Return the static tool list for non-custom profiles.
pub const fn profile_tools(profile: ToolProfileId) -> Option<&'static [&'static str]> {
    match profile {
        ToolProfileId::Recommended => Some(RECOMMENDED_TOOLS),
        ToolProfileId::Full => Some(FULL_TOOLS),
        ToolProfileId::Custom => None,
    }
}

/// Render a preview for one client without reading or writing user files.
pub fn preview_client_config(
    client: Client,
    request: &PreviewRequest<'_>,
) -> Result<ClientConfigPreview, PreviewError> {
    let install_root = validate_path("install_root", request.install_root)?;
    let config_path = validate_path("config_path", request.config_path)?;
    let binary_path = validate_path("binary_path", request.binary_path)?;
    let selected_tools = selected_tools(request.tool_profile)?;
    let metadata = client.metadata();
    let snippet = match client {
        Client::ClaudeCode | Client::ClaudeDesktop => {
            render_mcp_servers_json(&binary_path, &config_path)
        }
        Client::Codex => render_codex_toml(&binary_path, &config_path),
        Client::Zed => render_zed_json(&binary_path, &config_path),
        Client::Hermes => render_hermes_yaml(&binary_path, &config_path, &selected_tools),
    };

    if snippet.len() > MAX_SNIPPET_BYTES {
        return Err(PreviewError::SnippetTooLarge {
            len: snippet.len(),
            max: MAX_SNIPPET_BYTES,
        });
    }

    Ok(ClientConfigPreview {
        client,
        client_id: metadata.id,
        display_name: metadata.display_name,
        config_path: PathBuf::from(&config_path),
        install_root: PathBuf::from(&install_root),
        binary_path: PathBuf::from(&binary_path),
        scope: request.scope,
        snippet_format: metadata.snippet_format,
        snippet,
        selected_tools: selected_tools.into_iter().map(str::to_owned).collect(),
        tool_include_applied: metadata.supports_tool_include,
        note: if metadata.supports_tool_include {
            "Client supports native tool filtering; tools.include is rendered in the preview."
        } else {
            "Client config has no native tools.include field here; Ferrosa Memory compact defaults and all_tools handle progressive disclosure."
        },
    })
}

/// Render previews for multiple clients without reading or writing user files.
pub fn preview_client_configs(
    clients: &[Client],
    request: &PreviewRequest<'_>,
) -> Result<Vec<ClientConfigPreview>, PreviewError> {
    clients
        .iter()
        .copied()
        .map(|client| preview_client_config(client, request))
        .collect()
}

fn selected_tools<'a>(profile: ToolProfile<'a>) -> Result<Vec<&'a str>, PreviewError> {
    match profile {
        ToolProfile::Recommended => Ok(RECOMMENDED_TOOLS.to_vec()),
        ToolProfile::Full => Ok(FULL_TOOLS.to_vec()),
        ToolProfile::Custom(tools) => validate_custom_tools(tools),
    }
}

fn validate_custom_tools<'a>(tools: &'a [&'a str]) -> Result<Vec<&'a str>, PreviewError> {
    if tools.is_empty() {
        return Err(PreviewError::EmptyCustomProfile);
    }
    if tools.len() > MAX_CUSTOM_TOOLS {
        return Err(PreviewError::TooManyCustomTools {
            count: tools.len(),
            max: MAX_CUSTOM_TOOLS,
        });
    }

    for (index, tool) in tools.iter().enumerate() {
        validate_tool_name(index, tool)?;
        if tools[..index].contains(tool) {
            return Err(PreviewError::DuplicateToolName {
                name: (*tool).to_owned(),
            });
        }
    }

    Ok(tools.to_vec())
}

fn validate_tool_name(index: usize, tool: &str) -> Result<(), PreviewError> {
    if tool.is_empty() {
        return Err(PreviewError::EmptyToolName { index });
    }
    if tool.len() > MAX_TOOL_NAME_BYTES {
        return Err(PreviewError::ToolNameTooLong {
            name: tool.to_owned(),
            len: tool.len(),
            max: MAX_TOOL_NAME_BYTES,
        });
    }
    if !tool
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(PreviewError::InvalidToolName {
            name: tool.to_owned(),
        });
    }
    Ok(())
}

fn validate_path(field: &'static str, path: &Path) -> Result<String, PreviewError> {
    if path.as_os_str().is_empty() {
        return Err(PreviewError::EmptyPath { field });
    }
    let rendered = path.display().to_string();
    if rendered.len() > MAX_PATH_BYTES {
        return Err(PreviewError::PathTooLong {
            field,
            len: rendered.len(),
            max: MAX_PATH_BYTES,
        });
    }
    Ok(rendered)
}

fn render_mcp_servers_json(binary_path: &str, config_path: &str) -> String {
    format!(
        "{{\n  \"mcpServers\": {{\n    \"{SERVER_KEY}\": {{\n      \"command\": {},\n      \"args\": [\"--config\", {}]\n    }}\n  }}\n}}\n",
        json_string(binary_path),
        json_string(config_path),
    )
}

fn render_codex_toml(binary_path: &str, config_path: &str) -> String {
    format!(
        "[mcp_servers.{SERVER_KEY}]\ncommand = {}\nargs = [\"--config\", {}]\n",
        toml_string(binary_path),
        toml_string(config_path),
    )
}

fn render_zed_json(binary_path: &str, config_path: &str) -> String {
    format!(
        "{{\n  \"context_servers\": {{\n    \"{SERVER_KEY}\": {{\n      \"command\": {{\n        \"path\": {},\n        \"args\": [\"--config\", {}]\n      }}\n    }}\n  }}\n}}\n",
        json_string(binary_path),
        json_string(config_path),
    )
}

fn render_hermes_yaml(binary_path: &str, config_path: &str, tools: &[&str]) -> String {
    let mut snippet = format!(
        "mcp_servers:\n  {SERVER_KEY}:\n    command: {}\n    args:\n      - \"--config\"\n      - {}\n    enabled: true\n    tools:\n      include:\n",
        yaml_string(binary_path),
        yaml_string(config_path),
    );
    for tool in tools {
        snippet.push_str("        - ");
        snippet.push_str(tool);
        snippet.push('\n');
    }
    snippet
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if c.is_control() => {
                let code = c as u32;
                out.push_str("\\u");
                out.push(hex_digit((code >> 12) & 0xf));
                out.push(hex_digit((code >> 8) & 0xf));
                out.push(hex_digit((code >> 4) & 0xf));
                out.push(hex_digit(code & 0xf));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn toml_string(value: &str) -> String {
    json_string(value)
}

fn yaml_string(value: &str) -> String {
    json_string(value)
}

fn hex_digit(value: u32) -> char {
    match value {
        0..=9 => (b'0' + value as u8) as char,
        10..=15 => (b'a' + (value as u8 - 10)) as char,
        _ => unreachable!("hex digit input is masked to four bits"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(tool_profile: ToolProfile<'a>) -> PreviewRequest<'a> {
        PreviewRequest {
            install_root: Path::new("/Users/example/.ferrosa"),
            config_path: Path::new("/Users/example/.ferrosa/config/ferrosa-memory.toml"),
            binary_path: Path::new("/Users/example/.ferrosa/bin/ferrosa-memory-mcp"),
            scope: ConfigScope::Global,
            tool_profile,
        }
    }

    #[test]
    fn supported_clients_include_first_class_native_setup_targets() {
        assert_eq!(
            supported_clients(),
            &[
                Client::ClaudeCode,
                Client::Codex,
                Client::ClaudeDesktop,
                Client::Zed,
                Client::Hermes,
            ]
        );
        assert_eq!(Client::ClaudeCode.metadata().id, "claude_code");
        assert_eq!(Client::Hermes.metadata().id, "hermes");
    }

    #[test]
    fn recommended_tools_are_compact_and_include_required_surface() {
        assert_eq!(RECOMMENDED_TOOLS.len(), 10);
        for required in ["ingest", "search", "stats", "forget"] {
            assert!(RECOMMENDED_TOOLS.contains(&required));
        }
    }

    #[test]
    fn hermes_recommended_preview_uses_mcp_servers_tools_include_yaml() {
        let preview = preview_client_config(Client::Hermes, &request(ToolProfile::Recommended))
            .expect("Hermes preview should render");

        assert_eq!(preview.snippet_format, SnippetFormat::Yaml);
        assert!(preview.tool_include_applied);
        assert!(preview.snippet.contains("mcp_servers:\n"));
        assert!(preview.snippet.contains("  ferrosa-memory:\n"));
        assert!(
            preview
                .snippet
                .contains("    command: \"/Users/example/.ferrosa/bin/ferrosa-memory-mcp\"\n")
        );
        assert!(preview.snippet.contains("    args:\n      - \"--config\"\n      - \"/Users/example/.ferrosa/config/ferrosa-memory.toml\"\n"));
        assert!(preview.snippet.contains("    enabled: true\n"));
        assert!(preview.snippet.contains("    tools:\n      include:\n"));
        for tool in RECOMMENDED_TOOLS {
            assert!(preview.snippet.contains(&format!("        - {tool}\n")));
        }
    }

    #[test]
    fn codex_preview_uses_toml_mcp_servers_table() {
        let preview = preview_client_config(Client::Codex, &request(ToolProfile::Recommended))
            .expect("Codex preview should render");

        assert_eq!(preview.snippet_format, SnippetFormat::Toml);
        assert_eq!(
            preview.snippet,
            "[mcp_servers.ferrosa-memory]\ncommand = \"/Users/example/.ferrosa/bin/ferrosa-memory-mcp\"\nargs = [\"--config\", \"/Users/example/.ferrosa/config/ferrosa-memory.toml\"]\n"
        );
        assert!(!preview.tool_include_applied);
    }

    #[test]
    fn custom_profile_is_validated_and_rendered_for_hermes() {
        let preview = preview_client_config(
            Client::Hermes,
            &request(ToolProfile::Custom(&["search", "metrics"])),
        )
        .expect("custom Hermes preview should render");

        assert_eq!(preview.selected_tools, vec!["search", "metrics"]);
        assert!(preview.snippet.contains("        - search\n"));
        assert!(preview.snippet.contains("        - metrics\n"));
        assert!(!preview.snippet.contains("        - ingest\n"));
    }

    #[test]
    fn custom_profile_rejects_invalid_names_and_duplicates() {
        let invalid =
            preview_client_config(Client::Hermes, &request(ToolProfile::Custom(&["bad name"])))
                .unwrap_err();
        assert!(matches!(invalid, PreviewError::InvalidToolName { .. }));

        let duplicate = preview_client_config(
            Client::Hermes,
            &request(ToolProfile::Custom(&["search", "search"])),
        )
        .unwrap_err();
        assert!(matches!(duplicate, PreviewError::DuplicateToolName { .. }));
    }

    #[test]
    fn custom_profile_is_bounded() {
        let tools = ["x"; MAX_CUSTOM_TOOLS + 1];
        let err = preview_client_config(Client::Hermes, &request(ToolProfile::Custom(&tools)))
            .unwrap_err();

        assert!(matches!(err, PreviewError::TooManyCustomTools { .. }));
    }

    #[test]
    fn preview_client_configs_renders_requested_clients_only() {
        let previews = preview_client_configs(
            &[Client::ClaudeCode, Client::Hermes],
            &request(ToolProfile::Recommended),
        )
        .expect("selected previews should render");

        assert_eq!(previews.len(), 2);
        assert_eq!(previews[0].client, Client::ClaudeCode);
        assert_eq!(previews[1].client, Client::Hermes);
    }
}
