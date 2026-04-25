use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tool::{PermissionLevel, ToolContext, ToolHandler, ToolOutput, ToolRegistry, ToolSpec};

/// Intermediate struct matching the MCP protocol's `Tool` object.
/// MCP uses `inputSchema` (camelCase), and doesn't include omh-specific fields
/// like `required_permission` or `supports_parallel`.
#[derive(Debug, Clone, Deserialize)]
struct McpToolDef {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    input_schema: Option<Value>,
}

impl From<McpToolDef> for ToolSpec {
    fn from(def: McpToolDef) -> Self {
        Self {
            name: def.name,
            description: def.description.unwrap_or_default(),
            input_schema: def.input_schema.unwrap_or(json!({"type": "object"})),
            required_permission: PermissionLevel::ReadOnly,
            supports_parallel: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    StreamableHttp {
        uri: String,
        headers: HashMap<String, String>,
    },
}

pub struct McpClient {
    transport: McpTransport,
    state: Arc<Mutex<ConnectionState>>,
    next_id: Arc<AtomicU64>,
}

enum ConnectionState {
    Disconnected,
    Stdio(StdioConnection),
    StreamableHttp(HttpConnection),
}

struct StdioConnection {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

struct HttpConnection {
    client: reqwest::Client,
    session_id: Option<String>,
}

impl Clone for McpClient {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            state: Arc::clone(&self.state),
            next_id: Arc::clone(&self.next_id),
        }
    }
}

impl McpClient {
    pub fn connect(transport: McpTransport) -> Result<Self> {
        let state = match &transport {
            McpTransport::Stdio { command, args, env } => {
                let mut child = Command::new(command);
                child
                    .args(args)
                    .envs(env)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit());

                let mut child = child
                    .spawn()
                    .with_context(|| format!("failed to spawn MCP server `{command}`"))?;

                let stdin = child
                    .stdin
                    .take()
                    .context("MCP stdio server missing stdin pipe")?;
                let stdout = child
                    .stdout
                    .take()
                    .context("MCP stdio server missing stdout pipe")?;

                ConnectionState::Stdio(StdioConnection {
                    child,
                    stdin,
                    stdout: BufReader::new(stdout),
                })
            }
            McpTransport::StreamableHttp { headers, .. } => {
                let mut default_headers = HeaderMap::new();
                default_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                default_headers.insert(
                    ACCEPT,
                    HeaderValue::from_static("application/json, text/event-stream"),
                );
                for (key, value) in headers {
                    if let (Ok(name), Ok(val)) = (
                        reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                        HeaderValue::from_str(value),
                    ) {
                        default_headers.insert(name, val);
                    }
                }
                let client = reqwest::Client::builder()
                    .default_headers(default_headers)
                    .build()
                    .context("failed to build HTTP client for MCP")?;
                ConnectionState::StreamableHttp(HttpConnection {
                    client,
                    session_id: None,
                })
            }
        };

        let me = Self {
            transport,
            state: Arc::new(Mutex::new(state)),
            next_id: Arc::new(AtomicU64::new(1)),
        };

        // Perform initialization handshake for StreamableHttp
        if matches!(me.transport, McpTransport::StreamableHttp { .. }) {
            me.initialize()?;
        }

        Ok(me)
    }

    /// MCP initialization handshake: send `initialize` request, then `initialized` notification.
    fn initialize(&self) -> Result<()> {
        let params = json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "omh",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        // The initialize response may contain a session ID in the Mcp-Session-Id header.
        // We handle that inside send_http_request by storing it in the connection state.
        let _response = self.send_request("initialize", Some(params))?;

        // Send `initialized` notification (no id field, no response expected)
        if let McpTransport::StreamableHttp { uri, .. } = &self.transport {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("MCP client state lock poisoned"))?;
            if let ConnectionState::StreamableHttp(conn) = &*state {
                let notification = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                });
                let mut req = conn.client.post(uri).json(&notification);
                if let Some(sid) = &conn.session_id {
                    req = req.header("Mcp-Session-Id", sid.as_str());
                }
                let handle = tokio::runtime::Handle::current();
                let client_req = req;
                tokio::task::block_in_place(|| {
                    handle.block_on(async {
                        // Server should respond 202 Accepted for notifications
                        let _ = client_req.send().await;
                    })
                });
            }
        }

        Ok(())
    }

    pub fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        let response = self.send_request("tools/list", None)?;
        let result = response
            .result
            .ok_or_else(|| anyhow!("missing result in tools/list response"))?;

        let raw_tools: Vec<McpToolDef> = match result {
            Value::Array(_) => {
                serde_json::from_value(result).context("invalid tools/list result")?
            }
            Value::Object(mut object) => {
                let tools = object
                    .remove("tools")
                    .ok_or_else(|| anyhow!("missing `tools` field in tools/list response"))?;
                serde_json::from_value(tools).context("invalid tools/list response payload")?
            }
            _ => bail!("unexpected tools/list result payload"),
        };

        Ok(raw_tools.into_iter().map(ToolSpec::from).collect())
    }

    pub fn call_tool(&self, name: &str, input: Value) -> Result<ToolOutput> {
        let params = json!({
            "name": name,
            "arguments": input,
        });
        let response = self.send_request("tools/call", Some(params))?;
        let result = response
            .result
            .ok_or_else(|| anyhow!("missing result in tools/call response"))?;

        Self::parse_tool_output(result)
    }

    pub fn disconnect(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("MCP client state lock poisoned"))?;

        match &mut *state {
            ConnectionState::Disconnected => Ok(()),
            ConnectionState::Stdio(connection) => {
                match connection.child.kill() {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
                    Err(error) => return Err(error).context("failed to kill MCP stdio server"),
                }

                let _ = connection.child.wait();
                *state = ConnectionState::Disconnected;
                Ok(())
            }
            ConnectionState::StreamableHttp(conn) => {
                // Send DELETE to terminate the session if we have a session ID
                if let (McpTransport::StreamableHttp { uri, .. }, Some(sid)) =
                    (&self.transport, &conn.session_id)
                {
                    let client = conn.client.clone();
                    let uri = uri.clone();
                    let sid = sid.clone();
                    let handle = tokio::runtime::Handle::current();
                    tokio::task::block_in_place(|| {
                        handle.block_on(async {
                            let _ = client
                                .delete(&uri)
                                .header("Mcp-Session-Id", &sid)
                                .send()
                                .await;
                        })
                    });
                }
                *state = ConnectionState::Disconnected;
                Ok(())
            }
        }
    }

    fn send_request(&self, method: &str, params: Option<Value>) -> Result<JsonRpcResponse> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            method: method.to_string(),
            params,
        };

        match &self.transport {
            McpTransport::Stdio { .. } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("MCP client state lock poisoned"))?;

                let connection = match &mut *state {
                    ConnectionState::Stdio(connection) => connection,
                    ConnectionState::Disconnected => bail!("MCP client is disconnected"),
                    ConnectionState::StreamableHttp(_) => {
                        unreachable!("transport/state mismatch")
                    }
                };

                serde_json::to_writer(&mut connection.stdin, &request)
                    .context("failed to serialize JSON-RPC request")?;
                connection
                    .stdin
                    .write_all(b"\n")
                    .context("failed to write JSON-RPC delimiter")?;
                connection
                    .stdin
                    .flush()
                    .context("failed to flush JSON-RPC request")?;

                let response = read_json_rpc_response(&mut connection.stdout)?;
                if response.jsonrpc != "2.0" {
                    bail!("unexpected JSON-RPC version `{}`", response.jsonrpc);
                }
                if response.id != request.id {
                    bail!(
                        "JSON-RPC response id mismatch: expected {}, got {}",
                        request.id,
                        response.id
                    );
                }
                if let Some(error) = response.error.as_ref() {
                    bail!("MCP request failed ({}): {}", error.code, error.message);
                }

                Ok(response)
            }
            McpTransport::StreamableHttp { uri, .. } => self.send_http_request(&request, uri),
        }
    }

    fn send_http_request(&self, request: &JsonRpcRequest, uri: &str) -> Result<JsonRpcResponse> {
        let (client, session_id) = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("MCP client state lock poisoned"))?;
            match &*state {
                ConnectionState::StreamableHttp(conn) => {
                    (conn.client.clone(), conn.session_id.clone())
                }
                ConnectionState::Disconnected => bail!("MCP client is disconnected"),
                ConnectionState::Stdio(_) => unreachable!("transport/state mismatch"),
            }
        };

        let mut req = client.post(uri).json(request);
        if let Some(sid) = &session_id {
            req = req.header("Mcp-Session-Id", sid.as_str());
        }

        let handle = tokio::runtime::Handle::current();
        let (response, new_session_id) = tokio::task::block_in_place(|| {
            handle.block_on(async {
                let resp = req.send().await.context("MCP HTTP request failed")?;
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    bail!("MCP HTTP error {status}: {body}");
                }

                // Capture Mcp-Session-Id from response headers
                let new_sid = resp
                    .headers()
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                let content_type = resp
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();

                if content_type.contains("text/event-stream") {
                    // Parse SSE stream — collect events until we get the JSON-RPC response
                    let body = resp.text().await.context("failed to read SSE body")?;
                    let rpc_response = parse_sse_response(&body, request.id)?;
                    Ok((rpc_response, new_sid))
                } else {
                    let rpc_response: JsonRpcResponse = resp
                        .json()
                        .await
                        .context("failed to parse MCP HTTP response")?;
                    Ok((rpc_response, new_sid))
                }
            })
        })?;

        // Store new session ID if received
        if let Some(sid) = new_session_id {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("MCP client state lock poisoned"))?;
            if let ConnectionState::StreamableHttp(conn) = &mut *state {
                conn.session_id = Some(sid);
            }
        }

        if response.jsonrpc != "2.0" {
            bail!("unexpected JSON-RPC version `{}`", response.jsonrpc);
        }
        if response.id != request.id {
            bail!(
                "JSON-RPC response id mismatch: expected {}, got {}",
                request.id,
                response.id
            );
        }
        if let Some(error) = response.error.as_ref() {
            bail!("MCP request failed ({}): {}", error.code, error.message);
        }

        Ok(response)
    }

    fn parse_tool_output(result: Value) -> Result<ToolOutput> {
        if let Ok(output) = serde_json::from_value::<ToolOutput>(result.clone()) {
            return Ok(output);
        }

        if let Some(output) = result.get("output").cloned() {
            if let Ok(output) = serde_json::from_value::<ToolOutput>(output) {
                return Ok(output);
            }
        }

        if let Some(content) = result.get("content") {
            if let Some(text) = content.as_str() {
                return Ok(ToolOutput::text(text));
            }
        }

        Ok(ToolOutput {
            content: result.to_string(),
            is_error: false,
            metadata: HashMap::new(),
        })
    }
}

fn read_json_rpc_response(stdout: &mut BufReader<ChildStdout>) -> Result<JsonRpcResponse> {
    loop {
        let mut line = String::new();
        let bytes_read = stdout
            .read_line(&mut line)
            .context("failed to read JSON-RPC response")?;

        if bytes_read == 0 {
            bail!("MCP server closed the connection")
        }

        if line.trim().is_empty() {
            continue;
        }

        return serde_json::from_str(line.trim()).context("failed to parse JSON-RPC response");
    }
}

/// Parse an SSE body to extract the JSON-RPC response matching the given request id.
/// SSE format: lines starting with "data: " contain JSON payloads, blank lines delimit events.
fn parse_sse_response(body: &str, expected_id: u64) -> Result<JsonRpcResponse> {
    for line in body.lines() {
        let line = line.trim();
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            // Try to parse as JSON-RPC response
            if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(data) {
                if response.id == expected_id {
                    return Ok(response);
                }
            }
        }
    }
    bail!("no JSON-RPC response with id {expected_id} found in SSE stream")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: u64,
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct JsonRpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

pub struct McpToolBridge {
    clients: Vec<McpClient>,
}

impl McpToolBridge {
    pub fn new() -> Self {
        Self {
            clients: Vec::new(),
        }
    }

    pub fn add_client(&mut self, client: McpClient) {
        self.clients.push(client);
    }

    pub fn into_clients(self) -> Vec<McpClient> {
        self.clients
    }

    pub async fn add_server(&mut self, transport: McpTransport) -> Result<()> {
        self.clients.push(McpClient::connect(transport)?);
        Ok(())
    }

    pub async fn register_all(&self, registry: &ToolRegistry) -> Result<()> {
        for client in &self.clients {
            for spec in client.list_tools()? {
                registry.register(Box::new(McpToolProxy {
                    client: client.clone(),
                    spec,
                }));
            }
        }

        Ok(())
    }
}

impl Default for McpToolBridge {
    fn default() -> Self {
        Self::new()
    }
}

pub struct McpToolProxy {
    client: McpClient,
    spec: ToolSpec,
}

impl McpToolProxy {
    pub fn new(client: McpClient, spec: ToolSpec) -> Self {
        Self { client, spec }
    }
}

#[async_trait]
impl ToolHandler for McpToolProxy {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let _ = ctx;
        self.client.call_tool(&self.spec.name, input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_request_serialization_round_trip() {
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 7,
            method: "tools/list".to_string(),
            params: Some(json!({ "cursor": null })),
        };

        let serialized = serde_json::to_string(&request).unwrap();
        let deserialized: JsonRpcRequest = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized, request);
    }

    #[test]
    fn json_rpc_request_omits_params_when_none() {
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 2,
            method: "tools/list".to_string(),
            params: None,
        };

        let serialized = serde_json::to_string(&request).unwrap();
        // Must NOT contain "params" key at all — some MCP servers reject "params": null
        assert!(!serialized.contains("params"), "serialized: {serialized}");

        // Round-trip still works
        let deserialized: JsonRpcRequest = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, request);
    }

    #[test]
    fn mcp_transport_serialization_round_trip() {
        let transport = McpTransport::Stdio {
            command: "server".to_string(),
            args: vec!["--flag".to_string()],
            env: HashMap::from([(String::from("KEY"), String::from("VALUE"))]),
        };

        let serialized = serde_json::to_string(&transport).unwrap();
        let deserialized: McpTransport = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized, transport);
    }

    #[test]
    fn mcp_tool_bridge_new_is_empty() {
        let bridge = McpToolBridge::new();

        assert!(bridge.clients.is_empty());
    }

    #[test]
    #[ignore = "requires spawning an external MCP server"]
    fn stdio_process_integration_placeholder() {
        let transport = McpTransport::Stdio {
            command: "dummy-server".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
        };

        let _ = McpClient::connect(transport);
    }

    #[test]
    fn parse_sse_response_extracts_matching_id() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"tools\":[]}}\n\n";
        let response = parse_sse_response(body, 3).unwrap();
        assert_eq!(response.id, 3);
        assert!(response.result.is_some());
    }

    #[test]
    fn parse_sse_response_skips_non_matching_events() {
        let body = "\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n\
data: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"ok\":true}}\n\n";
        let response = parse_sse_response(body, 5).unwrap();
        assert_eq!(response.id, 5);
    }

    #[test]
    fn parse_sse_response_fails_when_id_not_found() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        assert!(parse_sse_response(body, 99).is_err());
    }

    #[test]
    fn mcp_tool_def_deserializes_from_protocol_json() {
        // Real MCP protocol format: camelCase `inputSchema`, no omh-specific fields
        let json = json!({
            "name": "search",
            "description": "Search the web",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }
        });

        let def: McpToolDef = serde_json::from_value(json).unwrap();
        assert_eq!(def.name, "search");
        assert_eq!(def.description.as_deref(), Some("Search the web"));
        assert!(def.input_schema.is_some());

        let spec = ToolSpec::from(def);
        assert_eq!(spec.name, "search");
        assert_eq!(spec.required_permission, PermissionLevel::ReadOnly);
        assert!(!spec.supports_parallel);
        assert!(spec.input_schema["properties"]["query"].is_object());
    }

    #[test]
    fn mcp_tool_def_handles_minimal_fields() {
        // Some MCP servers return tools with no description or inputSchema
        let json = json!({ "name": "ping" });

        let def: McpToolDef = serde_json::from_value(json).unwrap();
        let spec = ToolSpec::from(def);
        assert_eq!(spec.name, "ping");
        assert_eq!(spec.description, "");
        assert_eq!(spec.input_schema, json!({"type": "object"}));
    }
}
