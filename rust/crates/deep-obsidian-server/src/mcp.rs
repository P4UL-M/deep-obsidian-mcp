use std::sync::Arc;

use deep_obsidian_types::ResolvedServiceConfig;
use serde_json::{json, Value};

use crate::protocol::{
    InitializeResult, JsonRpcError, JsonRpcErrorResponse, JsonRpcRequest, JsonRpcResponse, ServerInfo, ToolCallResult,
    ToolContent, ToolDefinition, ToolListResult,
};
use crate::vault::{read_file, vault_info};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ResolvedServiceConfig>,
}

impl AppState {
    pub fn new(config: ResolvedServiceConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

fn json_response<T>(id: Value, result: T) -> JsonRpcResponse<T> {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result,
    }
}

fn json_error_response(id: Value, code: i64, message: impl Into<String>) -> JsonRpcErrorResponse {
    JsonRpcErrorResponse {
        jsonrpc: "2.0",
        id,
        error: JsonRpcError {
            code,
            message: message.into(),
        },
    }
}

fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "vault_info".to_string(),
            description: "Return basic metadata about the configured vault.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a file from the vault, optionally constrained to a line range.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "startLine": { "type": "integer", "minimum": 1 },
                    "endLine": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
    ]
}

pub fn initialize_response() -> JsonRpcResponse<InitializeResult> {
    json_response(
        json!(1),
        InitializeResult {
            protocol_version: "2025-03-26",
            capabilities: json!({
                "tools": {}
            }),
            server_info: ServerInfo {
                name: "deep-obsidian-server",
                version: "0.1.0",
            },
        },
    )
}

pub async fn handle_request(state: AppState, request: JsonRpcRequest) -> Result<Option<Value>, JsonRpcErrorResponse> {
    let id = request.id.unwrap_or(Value::Null);

    match request.method.as_str() {
        "notifications/initialized" => Ok(None),
        "initialize" => Ok(Some(serde_json::to_value(json_response(
            id,
            InitializeResult {
                protocol_version: "2025-03-26",
                capabilities: json!({ "tools": {} }),
                server_info: ServerInfo {
                    name: "deep-obsidian-server",
                    version: "0.1.0",
                },
            },
        ))
        .expect("initialize response to serialize"))),
        "tools/list" => Ok(Some(serde_json::to_value(json_response(
            id,
            ToolListResult {
                tools: tool_definitions(),
            },
        ))
        .expect("tool list response to serialize"))),
        "tools/call" => {
            let tool_name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| json_error_response(id.clone(), -32602, "missing tool name"))?;
            let arguments = request.params.get("arguments").cloned().unwrap_or_else(|| json!({}));

            let result = match tool_name {
                "vault_info" => {
                    let info = vault_info(&state.config.vault_path).map_err(|error| json_error_response(id.clone(), -32000, error.to_string()))?;
                    let payload = json!({
                        "vaultPath": info.vault_path,
                        "markdownFileCount": info.markdown_file_count,
                        "service": info.service,
                        "prototype": info.prototype,
                    });
                    ToolCallResult {
                        content: vec![ToolContent {
                            kind: "text",
                            text: serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
                        }],
                        structured_content: payload,
                    }
                }
                "read_file" => {
                    let path = arguments
                        .get("path")
                        .and_then(Value::as_str)
                        .ok_or_else(|| json_error_response(id.clone(), -32602, "missing path"))?;
                    let start_line = arguments.get("startLine").and_then(Value::as_u64).map(|value| value as usize);
                    let end_line = arguments.get("endLine").and_then(Value::as_u64).map(|value| value as usize);
                    let file = read_file(&state.config.vault_path, path, start_line, end_line)
                        .map_err(|error| json_error_response(id.clone(), -32000, error.to_string()))?;
                    let payload = serde_json::to_value(&file).map_err(|error| json_error_response(id.clone(), -32000, error.to_string()))?;
                    ToolCallResult {
                        content: vec![ToolContent {
                            kind: "text",
                            text: serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
                        }],
                        structured_content: payload,
                    }
                }
                _ => {
                    return Err(json_error_response(id, -32601, format!("unknown tool: {tool_name}")));
                }
            };

            Ok(Some(serde_json::to_value(json_response(id, result)).expect("tool response to serialize")))
        }
        _ => Err(json_error_response(id, -32601, format!("unsupported method: {}", request.method))),
    }
}
