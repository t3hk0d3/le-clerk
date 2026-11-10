//! Throwaway spike: rmcp's own Streamable HTTP client over the Unix socket,
//! with X-Clerk-* custom headers; lists tools and calls whoami and slow.
//! Usage: spike-client <socket>
use std::collections::HashMap;

use http::{HeaderName, HeaderValue};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation},
    transport::{
        StreamableHttpClientTransport,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let sock = std::env::args().nth(1).unwrap();
    let mut headers = HashMap::new();
    for (k, v) in [
        ("x-clerk-session", "test-session"),
        ("x-clerk-connection", "01TESTCONN"),
        ("x-clerk-project", "%2Ftmp%2Fproj"),
    ] {
        headers.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
    }
    let config = StreamableHttpClientTransportConfig::with_uri("http://localhost/mcp")
        .custom_headers(headers);
    let transport = StreamableHttpClientTransport::from_unix_socket_with_config(&sock, config);
    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("spike-client", "0"),
    )
    .serve(transport)
    .await?;
    println!("peer: {:?}", client.peer_info().map(|i| i.protocol_version.clone()));
    let tools = client.list_all_tools().await?;
    println!("tools: {:?}", tools.iter().map(|t| t.name.to_string()).collect::<Vec<_>>());
    let r = client
        .call_tool(CallToolRequestParams::new("whoami"))
        .await?;
    println!("whoami: {}", serde_json::to_string(&r)?);
    let r = client
        .call_tool(
            CallToolRequestParams::new("slow")
                .with_arguments(serde_json::from_str(r#"{"seconds":2,"every":1}"#)?),
        )
        .await?;
    println!("slow: {}", serde_json::to_string(&r)?);
    client.cancel().await?;
    Ok(())
}
