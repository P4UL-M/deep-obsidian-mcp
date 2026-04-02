use std::sync::Arc;

use deep_obsidian_types::ResolvedServiceConfig;
use serde_json::{json, Value};

use crate::protocol::{
    InitializeResult, JsonRpcError, JsonRpcErrorResponse, JsonRpcRequest, JsonRpcResponse,
    ResourceListResult, ResourceReadResult, ResourceTemplateListResult, ServerInfo, ToolCallResult,
    ToolListResult,
};
use crate::{resources, tools};
use crate::runtime::RuntimeState;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ResolvedServiceConfig>,
    pub runtime: Arc<RuntimeState>,
}

impl AppState {
    pub fn new(config: ResolvedServiceConfig, runtime: Arc<RuntimeState>) -> Self {
        Self {
            config: Arc::new(config),
            runtime,
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

fn initialize_result() -> InitializeResult {
    InitializeResult {
        protocol_version: "2025-03-26",
        capabilities: json!({
            "tools": {},
            "resources": {}
        }),
        server_info: ServerInfo {
            name: "deep-obsidian-mcp",
            version: "0.1.0",
        },
    }
}

pub fn initialize_response() -> JsonRpcResponse<InitializeResult> {
    json_response(json!(1), initialize_result())
}

pub async fn handle_request(state: AppState, request: JsonRpcRequest) -> Result<Option<Value>, JsonRpcErrorResponse> {
    let id = request.id.unwrap_or(Value::Null);

    match request.method.as_str() {
        "notifications/initialized" => Ok(None),
        "initialize" => Ok(Some(
            serde_json::to_value(json_response(id, initialize_result()))
                .expect("initialize response to serialize"),
        )),
        "tools/list" => Ok(Some(
            serde_json::to_value(json_response(
                id,
                ToolListResult {
                    tools: tools::list_tools(),
                },
            ))
            .expect("tool list response to serialize"),
        )),
        "tools/call" => {
            let tool_name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| json_error_response(id.clone(), -32602, "missing tool name"))?;
            let arguments = request.params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let result: ToolCallResult = tools::call_tool(&state, tool_name, &arguments)
                .await
                .map_err(|error| json_error_response(id.clone(), -32000, error))?;
            Ok(Some(
                serde_json::to_value(json_response(id, result)).expect("tool response to serialize"),
            ))
        }
        "resources/list" => {
            let result: ResourceListResult = resources::list_resources(&state)
                .await
                .map_err(|error| json_error_response(id.clone(), -32000, error))?;
            Ok(Some(
                serde_json::to_value(json_response(id, result)).expect("resource list response to serialize"),
            ))
        }
        "resources/templates/list" => {
            let result: ResourceTemplateListResult = resources::list_resource_templates();
            Ok(Some(
                serde_json::to_value(json_response(id, result))
                    .expect("resource template list response to serialize"),
            ))
        }
        "resources/read" => {
            let uri = request
                .params
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| json_error_response(id.clone(), -32602, "missing resource uri"))?;
            let result: ResourceReadResult = resources::read_resource(&state, uri)
                .await
                .map_err(|error| json_error_response(id.clone(), -32000, error))?;
            Ok(Some(
                serde_json::to_value(json_response(id, result)).expect("resource read response to serialize"),
            ))
        }
        _ => Err(json_error_response(
            id,
            -32601,
            format!("unsupported method: {}", request.method),
        )),
    }
}
