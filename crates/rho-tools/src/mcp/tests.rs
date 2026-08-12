use std::time::Duration;

use rho_ai::{CancellationToken, ContentBlock};
use serde_json::json;

use super::*;

fn shell_server(name: &str, script: &str) -> McpStdioConfig {
    let mut config = McpStdioConfig::new(name, "sh");
    config.args = vec!["-c".to_owned(), script.to_owned()];
    config.request_timeout = Duration::from_secs(2);
    config.probe_timeout = Duration::from_millis(200);
    config
}

#[tokio::test]
async fn current_server_is_discovered_paginated_namespaced_and_called() {
    let script = r#"
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}}}}'
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"weather.now","description":"Current weather.","inputSchema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}],"nextCursor":"page-2"}}'
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}'
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"resultType":"complete","content":[{"type":"text","text":"sunny"}],"structuredContent":{"degrees":24},"isError":false}}'
"#;
    let connection = McpConnection::connect(shell_server("demo", script))
        .await
        .unwrap();
    assert_eq!(connection.protocol_version(), CURRENT_PROTOCOL);
    assert_eq!(connection.tools().len(), 2);
    assert_eq!(
        connection.tools()[0].definition().name,
        "mcp__demo__weather_now"
    );
    assert_eq!(connection.tools()[0].remote_name(), "weather.now");
    assert_eq!(connection.tools()[0].server_name(), "demo");
    assert_eq!(connection.tools()[0].replay_safety(), ReplaySafety::Never);

    let output = connection.tools()[0]
        .execute(json!({"city": "Oslo"}), CancellationToken::new())
        .await;
    assert!(!output.is_error);
    assert_eq!(output.content, vec![ContentBlock::text("sunny")]);
    assert_eq!(
        output
            .details
            .unwrap()
            .pointer("/mcp/structured_content/degrees"),
        Some(&json!(24))
    );
}

#[tokio::test]
async fn legacy_server_falls_back_to_initialize_and_maps_rich_content() {
    let script = r#"
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}'
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"old","version":"1"}}}'
read -r initialized
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"fetch","inputSchema":{"type":"object"}}]}}'
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":"server-ping","method":"ping"}'
read -r ping_response
printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"resource_link","uri":"https://example.test/a","name":"a"}],"isError":false}}'
"#;
    let connection = McpConnection::connect(shell_server("legacy", script))
        .await
        .unwrap();
    assert_eq!(connection.protocol_version(), LEGACY_PROTOCOL);
    let output = connection.tools()[0]
        .execute(json!({}), CancellationToken::new())
        .await;
    assert!(!output.is_error);
    assert_eq!(
        output.content,
        vec![ContentBlock::text(
            "[MCP resource link: a (https://example.test/a)]"
        )]
    );
}

#[tokio::test]
async fn cancellation_is_forwarded_and_remote_calls_are_never_replayed() {
    let script = r#"
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}}}}'
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"wait","inputSchema":{"type":"object"}}]}}'
read -r call
read -r cancellation
"#;
    let connection = McpConnection::connect(shell_server("cancel", script))
        .await
        .unwrap();
    let cancellation = CancellationToken::new();
    let future = connection.tools()[0].execute(json!({}), cancellation.clone());
    tokio::pin!(future);
    let output = tokio::select! {
        output = &mut future => output,
        () = tokio::time::sleep(Duration::from_millis(20)) => {
            cancellation.cancel();
            future.await
        }
    };
    assert!(output.is_error);
    let ContentBlock::Text { text } = &output.content[0] else {
        panic!("expected text error")
    };
    assert!(text.contains("cancelled"), "{text}");
}

#[tokio::test]
async fn concurrent_calls_are_correlated_when_responses_arrive_out_of_order() {
    let script = r#"
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}}}}'
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"parallel","inputSchema":{"type":"object"}}]}}'
read -r call_three
read -r call_four
printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"resultType":"complete","content":[{"type":"text","text":"four"}]}}'
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","content":[{"type":"text","text":"three"}]}}'
"#;
    let connection = McpConnection::connect(shell_server("parallel", script))
        .await
        .unwrap();
    let tool = &connection.tools()[0];
    let (three, four) = tokio::join!(
        tool.execute(json!({"call": 3}), CancellationToken::new()),
        tool.execute(json!({"call": 4}), CancellationToken::new()),
    );
    assert_eq!(three.content, vec![ContentBlock::text("three")]);
    assert_eq!(four.content, vec![ContentBlock::text("four")]);
}

#[tokio::test]
async fn timeout_is_visible_and_server_stderr_is_bounded() {
    let script = r#"
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}}}}'
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"wait","inputSchema":{"type":"object"}}]}}'
printf '%09000d' 0 >&2
read -r call
read -r cancellation
"#;
    let mut config = shell_server("timeout", script);
    config.request_timeout = Duration::from_millis(30);
    let connection = McpConnection::connect(config).await.unwrap();
    let output = connection.tools()[0]
        .execute(json!({}), CancellationToken::new())
        .await;
    assert!(output.is_error);
    let ContentBlock::Text { text } = &output.content[0] else {
        panic!("expected text error")
    };
    assert!(text.contains("timed out"), "{text}");
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(connection.stderr_tail().len() <= 8 * 1024);
}

#[tokio::test]
async fn invalid_schema_and_required_tasks_fail_at_the_trust_boundary() {
    let invalid_schema = r#"
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}}}}'
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"bad","inputSchema":{"type":"wat"}}]}}'
"#;
    assert!(matches!(
        McpConnection::connect(shell_server("invalid", invalid_schema)).await,
        Err(McpError::InvalidTool { .. })
    ));

    let required_task = r#"
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}}}}'
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"task","inputSchema":{"type":"object"},"execution":{"taskSupport":"required"}}]}}'
"#;
    assert!(matches!(
        McpConnection::connect(shell_server("required", required_task)).await,
        Err(McpError::InvalidTool { .. })
    ));
}

#[test]
fn provider_names_are_portable_and_collisions_are_detected() {
    assert_eq!(
        exposed_name("server", "a.b-c"),
        Ok("mcp__server__a_b-c".to_owned())
    );
    assert!(exposed_name("server", "bad name").is_err());
    assert!(validate_config(&McpStdioConfig::new("bad.name", "cmd")).is_err());
}
