//! Integration tests for the LLM gateway against a mock OpenAI-compatible
//! server: chat, tool calls, retries, timeouts, streaming, and error paths.

use std::time::Duration;

use lemon_agent::config::LlmConfig;
use lemon_agent::error::{Error, ErrorCode};
use lemon_agent::llm::{LLMClient, Message, Role, ToolDefinition};
use serde_json::json;

fn config(base_url: &str) -> LlmConfig {
    LlmConfig {
        api_key: "test-key".to_string(),
        base_url: base_url.to_string(),
        model: "mock-model".to_string(),
        temperature: 0.2,
        request_timeout_secs: 5,
        max_retries: 3,
        retry_base_delay_secs: 0,
    }
}

fn completion_body(content: &str, tool_calls: Option<serde_json::Value>) -> serde_json::Value {
    let mut message = json!({ "role": "assistant", "content": content });
    if let Some(calls) = tool_calls {
        message["tool_calls"] = calls;
    }
    json!({
        "id": "cmpl-1",
        "object": "chat.completion",
        "model": "mock-model",
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    })
}

fn read_tool() -> ToolDefinition {
    ToolDefinition {
        name: "read_file".to_string(),
        description: "Read a file".to_string(),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
    }
}

/// A TCP server that accepts connections but never responds, for timeout tests.
fn hanging_server() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                std::thread::sleep(Duration::from_secs(60));
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn plain_chat_roundtrip() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/chat/completions")
        .match_header("authorization", "Bearer test-key")
        .match_body(mockito::Matcher::Json(json!({
            "model": "mock-model",
            "temperature": 0.2,
            "stream": false,
            "tools": null,
            "messages": [{"role": "user", "content": "hi"}]
        })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion_body("hello back", None).to_string())
        .create_async()
        .await;

    let client = LLMClient::new(&config(&server.url())).unwrap();
    let response = client.chat(&[Message::user("hi")], &[]).await.unwrap();
    assert_eq!(response.content, "hello back");
    assert!(response.tool_calls.is_empty());
    assert_eq!(response.usage.total_tokens, 15);
    assert_eq!(response.model, "mock-model");
    mock.assert_async().await;
}

#[tokio::test]
async fn tool_calls_are_parsed() {
    let mut server = mockito::Server::new_async().await;
    let calls = json!([
        {"id": "call_1", "type": "function", "function": {
            "name": "read_file",
            "arguments": "{\"path\": \"a.txt\"}"
        }}
    ]);
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion_body("", Some(calls)).to_string())
        .create_async()
        .await;

    let client = LLMClient::new(&config(&server.url())).unwrap();
    let response = client
        .chat(&[Message::user("read a.txt")], &[read_tool()])
        .await
        .unwrap();
    assert_eq!(response.content, "");
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].id, "call_1");
    assert_eq!(response.tool_calls[0].name, "read_file");
    assert_eq!(response.tool_calls[0].arguments, json!({"path": "a.txt"}));
}

#[tokio::test]
async fn malformed_tool_arguments_fail_loudly() {
    let mut server = mockito::Server::new_async().await;
    let calls = json!([
        {"id": "call_1", "type": "function", "function": {
            "name": "read_file",
            "arguments": "not json at all"
        }}
    ]);
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion_body("", Some(calls)).to_string())
        .create_async()
        .await;

    let client = LLMClient::new(&config(&server.url())).unwrap();
    let err = client
        .chat(&[Message::user("x")], &[read_tool()])
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Llm, "{err}");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn retries_transient_errors_with_backoff() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/chat/completions")
        .with_status(429)
        .with_body("rate limited")
        .create_async()
        .await;
    server
        .mock("POST", "/chat/completions")
        .with_status(500)
        .with_body("boom")
        .create_async()
        .await;
    let ok = server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion_body("recovered", None).to_string())
        .create_async()
        .await;

    let mut cfg = config(&server.url());
    cfg.retry_base_delay_secs = 0;
    let client = LLMClient::new(&cfg).unwrap();
    let response = client.chat(&[Message::user("hi")], &[]).await.unwrap();
    assert_eq!(response.content, "recovered");
    ok.assert_async().await;
}

#[tokio::test]
async fn retries_are_exhausted_after_limit() {
    let mut server = mockito::Server::new_async().await;
    for _ in 0..5 {
        server
            .mock("POST", "/chat/completions")
            .with_status(500)
            .with_body("boom")
            .create_async()
            .await;
    }
    let mut cfg = config(&server.url());
    cfg.max_retries = 3;
    cfg.retry_base_delay_secs = 0;
    let client = LLMClient::new(&cfg).unwrap();
    let err = client.chat(&[Message::user("hi")], &[]).await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::RetryExhausted, "{err}");
}

#[tokio::test]
async fn client_errors_are_not_retried() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/chat/completions")
        .with_status(400)
        .with_body("bad request")
        .create_async()
        .await;

    let mut cfg = config(&server.url());
    cfg.max_retries = 3;
    cfg.retry_base_delay_secs = 0;
    let client = LLMClient::new(&cfg).unwrap();
    let err = client.chat(&[Message::user("hi")], &[]).await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::Llm, "{err}");
    assert!(!err.is_retryable());
    // A single mock with default expectation: exactly one call is allowed.
    mock.assert_async().await;
}

#[tokio::test]
async fn request_timeout_is_retryable_and_exhausted() {
    let url = hanging_server();
    let mut cfg = config(&url);
    cfg.request_timeout_secs = 1;
    cfg.max_retries = 2;
    cfg.retry_base_delay_secs = 0;
    let client = LLMClient::new(&cfg).unwrap();
    let err = client.chat(&[Message::user("hi")], &[]).await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::RetryExhausted, "{err}");
}

#[tokio::test]
async fn malformed_response_body_fails_loudly() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("{not json")
        .create_async()
        .await;

    let client = LLMClient::new(&config(&server.url())).unwrap();
    let err = client.chat(&[Message::user("hi")], &[]).await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::Llm, "{err}");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn streaming_accumulates_content_and_tool_calls() {
    let mut server = mockito::Server::new_async().await;
    let chunks = [
        r#"data: {"id":"1","model":"mock-model","choices":[{"delta":{"role":"assistant","content":"Hel"}}]}"#,
        r#"data: {"id":"1","model":"mock-model","choices":[{"delta":{"content":"lo "}}]}"#,
        r#"data: {"id":"1","model":"mock-model","choices":[{"delta":{"content":"world"}}]}"#,
        r#"data: {"id":"1","model":"mock-model","choices":[{"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"read_file","arguments":"{\"path\":\""}}]}}]}"#,
        r#"data: {"id":"1","model":"mock-model","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"a.txt\"}"}}]}}]}"#,
        r#"data: [DONE]"#,
    ];
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(chunks.join("\n"))
        .create_async()
        .await;

    let client = LLMClient::new(&config(&server.url())).unwrap();
    let mut deltas = String::new();
    let response = client
        .chat_stream(&[Message::user("hi")], &[read_tool()], |d| {
            deltas.push_str(&d)
        })
        .await
        .unwrap();
    assert_eq!(response.content, "Hello world");
    assert!(deltas.contains("Hello world"));
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].name, "read_file");
    assert_eq!(response.tool_calls[0].arguments, json!({"path": "a.txt"}));
    assert_eq!(response.tool_calls[0].id, "t1");
}

#[tokio::test]
async fn empty_stream_is_retried_then_exhausted() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("data: [DONE]\n")
        .create_async()
        .await;

    let mut cfg = config(&server.url());
    cfg.max_retries = 1;
    cfg.retry_base_delay_secs = 0;
    let client = LLMClient::new(&cfg).unwrap();
    let err = client
        .chat_stream(&[Message::user("hi")], &[], |_| {})
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::RetryExhausted, "{err}");
}

#[tokio::test]
async fn wire_messages_carry_tool_results() {
    let mut server = mockito::Server::new_async().await;
    let expected = json!({
        "model": "mock-model",
        "stream": false,
        "temperature": 0.2,
        "tools": null,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "file contents"}
        ]
    });
    let mock = server
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::Json(expected))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion_body("done", None).to_string())
        .create_async()
        .await;

    let client = LLMClient::new(&config(&server.url())).unwrap();
    let calls = vec![lemon_agent::kernel::event_store::ToolCall {
        id: "c1".to_string(),
        name: "read_file".to_string(),
        arguments: json!({"path": "a.txt"}),
    }];
    let messages = vec![
        Message::user("hi"),
        Message::assistant_with_tool_calls(calls),
        Message::tool_result("c1", "file contents"),
    ];
    let response = client.chat(&messages, &[]).await.unwrap();
    assert_eq!(response.content, "done");
    mock.assert_async().await;
}

#[tokio::test]
async fn api_key_is_sent_as_bearer() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/chat/completions")
        .match_header("authorization", "Bearer sekret")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion_body("ok", None).to_string())
        .create_async()
        .await;

    let mut cfg = config(&server.url());
    cfg.api_key = "sekret".to_string();
    let client = LLMClient::new(&cfg).unwrap();
    client.chat(&[Message::user("hi")], &[]).await.unwrap();
    mock.assert_async().await;
}

#[test]
fn tools_to_schema_builds_function_definitions() {
    let schema = lemon_agent::llm::client::tools_to_schema(&[read_tool()]);
    assert_eq!(schema.len(), 1);
    assert_eq!(schema[0]["type"], "function");
    assert_eq!(schema[0]["function"]["name"], "read_file");
    assert_eq!(schema[0]["function"]["parameters"]["required"][0], "path");
}

#[test]
fn message_roles_serialize_lowercase() {
    let json = serde_json::to_string(&Message::user("x")).unwrap();
    assert!(json.contains("\"role\":\"user\""));
    let json = serde_json::to_string(&Message::assistant_with_tool_calls(vec![])).unwrap();
    assert!(json.contains("\"role\":\"assistant\""));
}

#[tokio::test]
async fn connect_failure_retries_then_exhausts() {
    let mut cfg = config("http://127.0.0.1:1");
    cfg.request_timeout_secs = 2;
    cfg.max_retries = 1;
    cfg.retry_base_delay_secs = 0;
    let client = LLMClient::new(&cfg).unwrap();
    let err = client.chat(&[Message::user("hi")], &[]).await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::RetryExhausted, "{err}");
}
