use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tool::{ToolContext, ToolHandler, ToolOutput, ToolRegistry, ToolSpec};

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
    StreamableHttp,
}

struct StdioConnection {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
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
            McpTransport::StreamableHttp { .. } => ConnectionState::StreamableHttp,
        };

        Ok(Self {
            transport,
            state: Arc::new(Mutex::new(state)),
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        let response = self.send_request("tools/list", None)?;
        let result = response
            .result
            .ok_or_else(|| anyhow!("missing result in tools/list response"))?;

        match result {
            Value::Array(_) => serde_json::from_value(result).context("invalid tools/list result"),
            Value::Object(mut object) => {
                let tools = object
                    .remove("tools")
                    .ok_or_else(|| anyhow!("missing `tools` field in tools/list response"))?;
                serde_json::from_value(tools).context("invalid tools/list response payload")
            }
            _ => bail!("unexpected tools/list result payload"),
        }
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
            ConnectionState::StreamableHttp => {
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
                    ConnectionState::StreamableHttp => unreachable!("transport/state mismatch"),
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
            McpTransport::StreamableHttp { uri, headers } => {
                self.send_http_request(&request, uri, headers)
            }
        }
    }

    fn send_http_request(
        &self,
        request: &JsonRpcRequest,
        uri: &str,
        headers: &HashMap<String, String>,
    ) -> Result<JsonRpcResponse> {
        let handle = tokio::runtime::Handle::current();
        let client = reqwest::Client::new();
        let mut req = client.post(uri).json(request);
        for (key, value) in headers {
            req = req.header(key.as_str(), value.as_str());
        }

        let response: JsonRpcResponse = tokio::task::block_in_place(|| {
            handle.block_on(async {
                let resp = req.send().await.context("MCP HTTP request failed")?;
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    bail!("MCP HTTP error {status}: {body}");
                }
                resp.json::<JsonRpcResponse>()
                    .await
                    .context("failed to parse MCP HTTP response")
            })
        })?;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
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
}
