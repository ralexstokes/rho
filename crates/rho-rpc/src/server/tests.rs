use std::sync::Mutex;

use serde_json::{Value, json};
use tokio::io::BufReader;

use super::*;
use crate::{ResponsePayload, RpcId};

#[derive(Default)]
struct TestHandler {
    answers: Mutex<Vec<ClientResponse>>,
}

impl RpcHandler for TestHandler {
    fn request(
        &self,
        request: ClientRequest,
        outbound: RpcSender,
    ) -> HandlerFuture<'_, Result<Value, ErrorObject>> {
        Box::pin(async move {
            outbound
                .event("test.seen", json!({"method": request.method}))
                .await
                .map_err(|error| ErrorObject::new("closed", error.to_string()))?;
            Ok(request.params)
        })
    }

    fn response(
        &self,
        response: ClientResponse,
        _: RpcSender,
    ) -> HandlerFuture<'_, Result<(), ErrorObject>> {
        self.answers.lock().unwrap().push(response);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn server_multiplexes_requests_events_responses_and_version_errors() {
    let handler = Arc::new(TestHandler::default());
    let observed = Arc::clone(&handler);
    let (client, server) = tokio::io::duplex(16 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task = tokio::spawn(serve(server_read, server_write, handler));
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut client_read = BufReader::new(client_read);

    client_write
        .write_all(b"{\"v\":1,\"id\":\"one\",\"method\":\"echo\",\"params\":{\"n\":1}}\n")
        .await
        .unwrap();
    let mut lines = Vec::new();
    for _ in 0..2 {
        let mut line = String::new();
        client_read.read_line(&mut line).await.unwrap();
        lines.push(serde_json::from_str::<Value>(&line).unwrap());
    }
    assert!(
        lines
            .iter()
            .any(|line| line.get("event") == Some(&json!("test.seen")))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.get("ok") == Some(&json!(true)))
    );

    client_write
        .write_all(
            b"{\"v\":1,\"id\":\"interaction\",\"ok\":true,\"result\":{\"answer\":\"approved\"}}\n",
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if !observed.answers.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        observed.answers.lock().unwrap()[0].payload,
        ResponsePayload::Success(json!({"answer": "approved"}))
    );

    client_write
        .write_all(b"{\"v\":99,\"id\":2,\"method\":\"echo\",\"params\":{}}\n")
        .await
        .unwrap();
    let mut line = String::new();
    client_read.read_line(&mut line).await.unwrap();
    let error: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        error.pointer("/error/code"),
        Some(&json!("unsupported_version"))
    );

    drop(client_write);
    drop(client_read);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test]
async fn malformed_or_oversized_frames_close_the_connection() {
    let handler = Arc::new(TestHandler::default());
    let (client, server) = tokio::io::duplex(1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task = tokio::spawn(serve(server_read, server_write, handler));
    let (_, mut client_write) = tokio::io::split(client);
    client_write.write_all(b"not-json\n").await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(ServeError::Codec(_))));

    let bytes = vec![b'x'; MAX_FRAME_BYTES + 2];
    let mut oversized = BufReader::new(bytes.as_slice());
    assert!(matches!(
        read_frame(&mut oversized).await,
        Err(ServeError::Codec(_))
    ));
}

#[test]
fn ids_support_strings_and_numbers() {
    assert_eq!(RpcId::from(7_u64).to_string(), "7");
    assert_eq!(RpcId::from("seven").to_string(), "seven");
}
