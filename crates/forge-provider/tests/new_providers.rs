//! Mock-SSE tests for the Responses and Anthropic protocol adapters,
//! mirroring the exact event shapes observed on live endpoints.

use forge_core::message::Message;
use forge_core::traits::{ModelProvider, ModelRequest, ProviderEvent};
use forge_provider::{AnthropicProvider, ResponsesProvider};
use futures::StreamExt;
use std::io::{Read, Write};
use std::net::TcpListener;

async fn serve_once(body: &'static str, status_line: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::task::spawn_blocking(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf);
        let resp = format!(
            "{status_line}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes());
        let _ = sock.flush();
        let _ = sock.shutdown(std::net::Shutdown::Both);
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    format!("http://{addr}")
}

fn req() -> ModelRequest {
    ModelRequest {
        messages: vec![Message::user("hi")],
        tools: vec![],
        model: "m".into(),
        temperature: None,
        max_tokens: 64,
        stream_reasoning: false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn responses_text_and_usage() {
    let body: &'static str = Box::leak(
        "event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"delta\":\"He\"}\n\n\
         event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"delta\":\"llo\"}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":3,\"total_tokens\":13}}}\n\n\
         data: {\"error\":{\"message\":\"upstream returned empty content\",\"type\":\"server_error\"}}\n\n\
         data: [DONE]\n\n"
            .to_string()
            .into_boxed_str(),
    );
    let url = serve_once(body, "HTTP/1.1 200 OK").await;
    let p = ResponsesProvider::new(url);
    let stream = p.stream(req(), "sk").await.expect("stream ok");
    let mut stream = Box::pin(stream);

    let mut text = String::new();
    let mut usage = None;
    let mut saw_done = false;
    while let Some(ev) = stream.next().await {
        match ev {
            ProviderEvent::MessageDelta { delta } => text.push_str(&delta),
            ProviderEvent::Usage { usage: u } => usage = Some(u),
            ProviderEvent::ProviderError { message } => {
                // Trailing gateway error after content must NOT surface.
                panic!("trailing error leaked: {message}");
            }
            ProviderEvent::Done => {
                saw_done = true;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(text, "Hello");
    assert_eq!(usage.map(|u| u.total_tokens), Some(13));
    assert!(saw_done);
}

#[tokio::test(flavor = "multi_thread")]
async fn responses_tool_call_reassembly() {
    let body: &'static str = Box::leak(
        "event: response.output_item.added\n\
         data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_x\",\"name\":\"shell\"}}\n\n\
         event: response.function_call_arguments.delta\n\
         data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"co\"}\n\n\
         event: response.function_call_arguments.delta\n\
         data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"mmand\\\":\\\"ls\\\"}\"}\n\n\
         event: response.function_call_arguments.done\n\
         data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":5,\"total_tokens\":25}}}\n\n\
         data: [DONE]\n\n"
            .to_string()
            .into_boxed_str(),
    );
    let url = serve_once(body, "HTTP/1.1 200 OK").await;
    let p = ResponsesProvider::new(url);
    let stream = p.stream(req(), "sk").await.expect("stream ok");
    let mut stream = Box::pin(stream);
    let mut calls = Vec::new();
    while let Some(ev) = stream.next().await {
        if let ProviderEvent::ToolCalls { calls: c } = ev {
            calls = c;
        }
    }
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_x");
    assert_eq!(calls[0].name, "shell");
    assert_eq!(calls[0].arguments["command"], "ls");
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_text_thinking_and_tools() {
    let body: &'static str = Box::leak(
        "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"shell\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"dir\\\"}\"}}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n\
         data: [DONE]\n\n"
            .to_string()
            .into_boxed_str(),
    );
    let url = serve_once(body, "HTTP/1.1 200 OK").await;
    let p = AnthropicProvider::new(url);
    let stream = p.stream(req(), "sk").await.expect("stream ok");
    let mut stream = Box::pin(stream);

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    let mut usage = None;
    while let Some(ev) = stream.next().await {
        match ev {
            ProviderEvent::MessageDelta { delta } => text.push_str(&delta),
            ProviderEvent::ReasoningDelta { delta } => reasoning.push_str(&delta),
            ProviderEvent::ToolCalls { calls: c } => calls = c,
            ProviderEvent::Usage { usage: u } => usage = Some(u),
            ProviderEvent::Done => break,
            _ => {}
        }
    }
    assert_eq!(text, "Hi");
    assert_eq!(reasoning, "hmm");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "toolu_1");
    assert_eq!(calls[0].arguments["command"], "dir");
    assert_eq!(usage.map(|u| u.total_tokens), Some(20)); // 11 + 9
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_error_frame_surfaces() {
    let body: &'static str = Box::leak(
        "event: error\n\
         data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n"
            .to_string()
            .into_boxed_str(),
    );
    let url = serve_once(body, "HTTP/1.1 200 OK").await;
    let p = AnthropicProvider::new(url);
    let stream = p.stream(req(), "sk").await.expect("stream ok");
    let mut stream = Box::pin(stream);
    let mut err = None;
    while let Some(ev) = stream.next().await {
        if let ProviderEvent::ProviderError { message } = ev {
            err = Some(message);
        }
    }
    assert_eq!(err.as_deref(), Some("Overloaded"));
}
