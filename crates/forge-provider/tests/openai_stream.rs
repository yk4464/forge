use forge_provider::OpenAiProvider;

use forge_core::message::Message;
use forge_core::traits::{ModelProvider, ModelRequest, ProviderEvent};
use futures::StreamExt;
use std::io::{Read, Write};
use std::net::TcpListener;

/// Serve one raw SSE response on a random port; returns the base URL.
/// Runs the accept loop on a blocking thread so the tokio test runtime is
/// never blocked (a blocking accept inside a plain spawn can wedge the
/// single-threaded test runtime on Windows).
async fn serve_once(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::task::spawn_blocking(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes());
        let _ = sock.flush();
        let _ = sock.shutdown(std::net::Shutdown::Both);
    });
    // Give the listener a beat before the client connects.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    format!("http://{addr}")
}

async fn run_stream(p: &OpenAiProvider, url: String) -> (String, String, Option<i64>, bool) {
    let req = ModelRequest {
        messages: vec![Message::user("hi")],
        tools: vec![],
        model: "test-model".into(),
        temperature: None,
        max_tokens: 64,
        stream_reasoning: true,
    };
    let stream = p.stream(req, "sk-test").await.expect("stream ok");
    let mut stream = Box::pin(stream);

    let mut reasoning = String::new();
    let mut text = String::new();
    let mut usage_total = None;
    let mut done = false;
    while let Some(ev) = stream.next().await {
        match ev {
            ProviderEvent::MessageDelta { delta } => text.push_str(&delta),
            ProviderEvent::ReasoningDelta { delta } => reasoning.push_str(&delta),
            ProviderEvent::Usage { usage } => usage_total = Some(usage.total_tokens),
            ProviderEvent::Done => {
                done = true;
                break;
            }
            _ => {}
        }
    }
    let _ = url;
    (text, reasoning, usage_total, done)
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_deltas_and_usage() {
    let body: &'static str = Box::leak(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n\
         data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13}}\n\n\
         data: [DONE]\n\n"
            .to_string()
            .into_boxed_str(),
    );
    let url = serve_once(body).await;
    let p = OpenAiProvider::new(url);
    let (text, reasoning, usage, done) = run_stream(&p, p.base_url()).await;
    assert_eq!(reasoning, "think");
    assert_eq!(text, "Hello");
    assert_eq!(usage, Some(13));
    assert!(done);
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_call_fragments_reassemble() {
    let body: &'static str = Box::leak(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"co\"}}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"mmand\\\":\\\"ls\\\"}\"}}]}}]}\n\n\
         data: [DONE]\n\n"
            .to_string()
            .into_boxed_str(),
    );
    let url = serve_once(body).await;
    let p = OpenAiProvider::new(url);
    let req = ModelRequest {
        messages: vec![Message::user("run ls")],
        tools: vec![],
        model: "m".into(),
        temperature: None,
        max_tokens: 64,
        stream_reasoning: false,
    };
    let stream = p.stream(req, "sk").await.expect("stream ok");
    let mut stream = Box::pin(stream);
    let mut calls = Vec::new();
    while let Some(ev) = stream.next().await {
        if let ProviderEvent::ToolCalls { calls: c } = ev {
            calls = c;
        }
    }
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "c1");
    assert_eq!(calls[0].name, "shell");
    assert_eq!(calls[0].arguments["command"], "ls");
}

#[tokio::test(flavor = "multi_thread")]
async fn http_overflow_maps_to_context_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::task::spawn_blocking(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf);
        let body = "{\"error\":{\"message\":\"This model's maximum context length is 4096 tokens\"}}";
        let resp = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes());
        let _ = sock.flush();
        let _ = sock.shutdown(std::net::Shutdown::Both);
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let p = OpenAiProvider::new(format!("http://{addr}"));
    let req = ModelRequest {
        messages: vec![Message::user("hi")],
        tools: vec![],
        model: "m".into(),
        temperature: None,
        max_tokens: 16,
        stream_reasoning: false,
    };
    let err = match p.stream(req, "sk").await {
        Err(e) => e,
        Ok(_) => panic!("expected error"),
    };
    assert!(err.is_context_window_exceeded(), "got: {err:?}");
}

/// Serve a partial SSE response: declares a longer body than it sends and
/// closes the socket mid-stream, so the client hits a transport error.
async fn serve_truncated(partial: &'static str, declared_len: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::task::spawn_blocking(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            declared_len,
            partial
        );
        let _ = sock.write_all(resp.as_bytes());
        let _ = sock.flush();
        let _ = sock.shutdown(std::net::Shutdown::Both);
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_interruption_surfaces_as_provider_error() {
    // One complete frame, then the connection dies before [DONE]. The old
    // behavior swallowed the error and emitted Done — a truncated answer
    // presented as complete.
    let partial: &'static str = Box::leak(
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial ans\"}}]}\n\n".to_string().into_boxed_str(),
    );
    let url = serve_truncated(partial, partial.len() + 4096).await;
    let p = OpenAiProvider::new(url);
    let req = ModelRequest {
        messages: vec![Message::user("hi")],
        tools: vec![],
        model: "m".into(),
        temperature: None,
        max_tokens: 64,
        stream_reasoning: false,
    };
    let stream = p.stream(req, "sk").await.expect("stream ok");
    let mut stream = Box::pin(stream);
    let mut err = None;
    let mut saw_done = false;
    while let Some(ev) = stream.next().await {
        match ev {
            ProviderEvent::ProviderError { message } => err = Some(message),
            ProviderEvent::Done => saw_done = true,
            _ => {}
        }
    }
    assert!(saw_done, "stream must still terminate");
    let msg = err.expect("transport break must surface as ProviderError");
    assert!(msg.contains("stream interrupted"), "got: {msg}");
}
