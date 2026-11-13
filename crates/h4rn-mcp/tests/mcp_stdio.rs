//! End-to-end coverage for the streamable-HTTP MCP endpoint.
//!
//! The upstream is a real child process ([`mcp-fixture`]) speaking MCP over
//! stdio, so this test covers token auth, the handshake, namespacing, tool
//! calls, resources, prompts and per-session scoping without mocking the
//! transport.

use std::sync::Arc;

use h4rn_mcp::hub::McpHub;
use h4rn_mcp::provider::EchoProvider;
use h4rn_mcp::registry::MemoryRegistry;
use h4rn_mcp::secrets::MemorySecretStore;
use h4rn_mcp::session::McpScope;
use h4rn_mcp::upstream::UpstreamLimits;
use h4rn_protocol::HarnessKind;
use h4rn_protocol::HostId;
use h4rn_protocol::mcp::{McpEnabledMatrix, McpServerInput, McpTransportInput};
use serde_json::{Value, json};

struct Fixture {
    url: String,
    token: String,
    hub: Arc<McpHub>,
    client: reqwest::Client,
}

async fn fixture(scope: McpScope) -> Fixture {
    let hub = Arc::new(
        McpHub::open(
            HostId::new("host_a"),
            Arc::new(MemoryRegistry::new()),
            Arc::new(MemorySecretStore::new()),
            UpstreamLimits::default(),
        )
        .await
        .unwrap(),
    );
    hub.register_provider(Arc::new(EchoProvider::new("test")));
    hub.start();
    hub.upsert(McpServerInput {
        name: "fixture".into(),
        transport: McpTransportInput::Stdio {
            command: env!("CARGO_BIN_EXE_mcp-fixture").into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        },
        tags: Vec::new(),
        enabled: McpEnabledMatrix::default(),
    })
    .await
    .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = h4rn_mcp::server::router(Arc::clone(&hub));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let token = hub.issue_session(scope).token;
    Fixture {
        url: format!("http://{address}/mcp"),
        token,
        hub,
        client: reqwest::Client::new(),
    }
}

impl Fixture {
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "HTTP status for {method}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["jsonrpc"], "2.0", "body for {method}: {body}");
        body
    }

    async fn call(&self, method: &str, params: Value) -> Value {
        let body = self.rpc(method, params).await;
        assert!(body.get("error").is_none(), "{method} failed: {body}");
        body["result"].clone()
    }
}

fn scope() -> McpScope {
    McpScope::harness(HarnessKind::ClaudeCode, HostId::new("host_a"), None)
}

#[tokio::test]
async fn initialize_advertises_the_hub_and_negotiates_the_revision() {
    let fixture = fixture(scope()).await;
    let response = fixture
        .client
        .post(&fixture.url)
        .bearer_auth(&fixture.token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(
        response.headers().contains_key("mcp-session-id"),
        "initialize binds the session id"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(body["result"]["serverInfo"]["name"], "h4rn");
    assert_eq!(body["result"]["capabilities"]["tools"]["listChanged"], true);
}

#[tokio::test]
async fn missing_or_wrong_tokens_are_rejected() {
    let fixture = fixture(scope()).await;
    let unauthorized = fixture
        .client
        .post(&fixture.url)
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let forged = fixture
        .client
        .post(&fixture.url)
        .bearer_auth("h4rn_mcp_not-a-real-token")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 401);
}

#[tokio::test]
async fn tools_are_namespaced_and_reach_a_real_stdio_upstream() {
    let fixture = fixture(scope()).await;
    let listed = fixture.call("tools/list", json!({})).await;
    let names: Vec<&str> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"h4rn__echo"));
    assert!(names.contains(&"fixture__echo"));
    assert!(names.contains(&"fixture__fail"));

    let echoed = fixture
        .call(
            "tools/call",
            json!({"name": "fixture__echo", "arguments": {"message": "hello hub"}}),
        )
        .await;
    assert_eq!(echoed["content"][0]["text"], "hello hub");
    assert_eq!(echoed["isError"], false);

    let failed = fixture
        .call(
            "tools/call",
            json!({"name": "fixture__fail", "arguments": {}}),
        )
        .await;
    assert_eq!(failed["isError"], true);
    assert_eq!(
        failed["content"][0]["text"],
        "the fixture failed on purpose"
    );

    let unknown = fixture
        .rpc(
            "tools/call",
            json!({"name": "fixture__missing", "arguments": {}}),
        )
        .await;
    assert!(
        unknown["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no tool named"),
        "unexpected error: {unknown}"
    );
}

#[tokio::test]
async fn resources_and_prompts_are_namespaced_per_server() {
    let fixture = fixture(scope()).await;
    let resources = fixture.call("resources/list", json!({})).await;
    let resource = &resources["resources"][0];
    let uri = resource["uri"].as_str().unwrap();
    assert!(uri.starts_with("h4rn-resource://fixture/"), "{uri}");

    let read = fixture.call("resources/read", json!({"uri": uri})).await;
    assert_eq!(read["contents"][0]["text"], "hello from the fixture");

    let prompts = fixture.call("prompts/list", json!({})).await;
    let prompt = &prompts["prompts"][0];
    assert_eq!(prompt["name"], "fixture__greet");
    let got = fixture
        .call(
            "prompts/get",
            json!({"name": "fixture__greet", "arguments": {"who": "hub"}}),
        )
        .await;
    assert_eq!(got["messages"][0]["content"]["text"], "hello hub");
}

#[tokio::test]
async fn a_session_only_sees_the_tools_its_scope_allows() {
    let mut scope = scope();
    scope.allowed_tools = Some(vec!["h4rn__echo".into()]);
    let fixture = fixture(scope).await;
    let listed = fixture.call("tools/list", json!({})).await;
    let names: Vec<&str> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"h4rn__echo"));
    assert!(
        !names.contains(&"fixture__echo"),
        "a narrowed session must not call an upstream tool: {names:?}"
    );

    let denied = fixture
        .rpc(
            "tools/call",
            json!({"name": "fixture__echo", "arguments": {"message": "nope"}}),
        )
        .await;
    assert!(denied.get("error").is_some(), "{denied}");
}

#[tokio::test]
async fn deleting_the_session_revokes_the_token() {
    let fixture = fixture(scope()).await;
    fixture.call("tools/list", json!({})).await;
    let deleted = fixture
        .client
        .delete(&fixture.url)
        .bearer_auth(&fixture.token)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
    let after = fixture
        .client
        .post(&fixture.url)
        .bearer_auth(&fixture.token)
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 401);

    fixture.hub.shutdown().await;
}
