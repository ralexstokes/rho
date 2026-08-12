use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use rho_ai::CancellationToken;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{Mutex as AsyncMutex, oneshot},
};

use super::McpError;

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct Transport {
    shared: Arc<Shared>,
}

struct Shared {
    writer: AsyncMutex<ChildStdin>,
    pending: AsyncMutex<Pending>,
    next_id: AtomicU64,
    legacy: AtomicBool,
    stderr: Arc<Mutex<Vec<u8>>>,
}

struct Pending {
    requests: HashMap<u64, oneshot::Sender<Result<Value, McpError>>>,
    closed: Option<McpError>,
}

impl Transport {
    pub(super) fn new(stdin: ChildStdin, stdout: ChildStdout) -> Self {
        let shared = Arc::new(Shared {
            writer: AsyncMutex::new(stdin),
            pending: AsyncMutex::new(Pending {
                requests: HashMap::new(),
                closed: None,
            }),
            next_id: AtomicU64::new(1),
            legacy: AtomicBool::new(false),
            stderr: Arc::new(Mutex::new(Vec::new())),
        });
        tokio::spawn(read_responses(
            BufReader::new(stdout),
            Arc::downgrade(&shared),
        ));
        Self { shared }
    }

    pub(super) fn stderr_buffer(&self) -> Arc<Mutex<Vec<u8>>> {
        Arc::clone(&self.shared.stderr)
    }

    pub(super) fn stderr_tail(&self) -> String {
        let bytes = match self.shared.stderr.lock() {
            Ok(bytes) => bytes,
            Err(poisoned) => poisoned.into_inner(),
        };
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub(super) fn set_legacy(&self) {
        self.shared.legacy.store(true, Ordering::Release);
    }

    pub(super) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancellation: Option<CancellationToken>,
    ) -> Result<Value, McpError> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.shared.pending.lock().await;
            if let Some(error) = &pending.closed {
                return Err(error.clone());
            }
            pending.requests.insert(id, sender);
        }
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(error) = self.write(&message).await {
            self.shared.pending.lock().await.requests.remove(&id);
            return Err(error);
        }

        let response = if let Some(cancellation) = cancellation {
            tokio::select! {
                result = receiver => receive(method, result),
                () = cancellation.cancelled() => {
                    self.shared.pending.lock().await.requests.remove(&id);
                    let _ = self.cancel(id, "rho tool call was cancelled").await;
                    Err(McpError::Cancelled { method: method.to_owned() })
                }
                () = tokio::time::sleep(timeout) => {
                    self.shared.pending.lock().await.requests.remove(&id);
                    let _ = self.cancel(id, "rho MCP request timed out").await;
                    Err(McpError::Timeout { method: method.to_owned() })
                }
            }
        } else {
            tokio::select! {
                result = receiver => receive(method, result),
                () = tokio::time::sleep(timeout) => {
                    self.shared.pending.lock().await.requests.remove(&id);
                    Err(McpError::Timeout { method: method.to_owned() })
                }
            }
        }?;
        parse_response(response)
    }

    pub(super) async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        let mut message = json!({"jsonrpc": "2.0", "method": method});
        if let Some(params) = params {
            message
                .as_object_mut()
                .expect("notification is an object")
                .insert("params".to_owned(), params);
        }
        self.write(&message).await
    }

    async fn cancel(&self, id: u64, reason: &str) -> Result<(), McpError> {
        self.notify(
            "notifications/cancelled",
            Some(json!({"requestId": id, "reason": reason})),
        )
        .await
    }

    async fn write(&self, message: &Value) -> Result<(), McpError> {
        write_message(&self.shared, message).await
    }
}

async fn write_message(shared: &Shared, message: &Value) -> Result<(), McpError> {
    let mut bytes = serde_json::to_vec(message).map_err(|error| McpError::Protocol {
        message: format!("could not encode request: {error}"),
    })?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(McpError::Protocol {
            message: format!("outgoing message exceeds {MAX_FRAME_BYTES} bytes"),
        });
    }
    bytes.push(b'\n');
    let mut writer = shared.writer.lock().await;
    writer
        .write_all(&bytes)
        .await
        .map_err(|error| McpError::Transport {
            message: error.to_string(),
        })?;
    writer.flush().await.map_err(|error| McpError::Transport {
        message: error.to_string(),
    })
}

fn receive(
    method: &str,
    result: Result<Result<Value, McpError>, oneshot::error::RecvError>,
) -> Result<Value, McpError> {
    result.map_err(|_| McpError::Transport {
        message: format!("response channel closed while waiting for {method:?}"),
    })?
}

fn parse_response(response: Value) -> Result<Value, McpError> {
    if response.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
        return Err(McpError::Protocol {
            message: "response omitted jsonrpc: \"2.0\"".to_owned(),
        });
    }
    match (response.get("result"), response.get("error")) {
        (Some(result), None) if result.is_object() => Ok(result.clone()),
        (None, Some(error)) => {
            let code =
                error
                    .get("code")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| McpError::Protocol {
                        message: "JSON-RPC error omitted an integer code".to_owned(),
                    })?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| McpError::Protocol {
                    message: "JSON-RPC error omitted a string message".to_owned(),
                })?;
            Err(McpError::Remote {
                code,
                message: message.to_owned(),
                data: error.get("data").cloned(),
            })
        }
        _ => Err(McpError::Protocol {
            message: "response must contain exactly one object result or error".to_owned(),
        }),
    }
}

async fn read_responses<R>(mut reader: R, shared: Weak<Shared>)
where
    R: AsyncBufRead + Unpin,
{
    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                close(
                    &shared,
                    McpError::Transport {
                        message: "MCP server closed stdout".to_owned(),
                    },
                )
                .await;
                return;
            }
            Err(error) => {
                close(&shared, error).await;
                return;
            }
        };
        let message: Value = match serde_json::from_slice(&frame) {
            Ok(message) => message,
            Err(error) => {
                close(
                    &shared,
                    McpError::Protocol {
                        message: format!("invalid JSON from MCP server: {error}"),
                    },
                )
                .await;
                return;
            }
        };
        let Some(shared) = shared.upgrade() else {
            return;
        };
        if message.get("id").is_some()
            && (message.get("result").is_some() || message.get("error").is_some())
        {
            let Some(id) = message.get("id").and_then(Value::as_u64) else {
                close(
                    &Arc::downgrade(&shared),
                    McpError::Protocol {
                        message: "response ID must be a non-negative integer issued by rho"
                            .to_owned(),
                    },
                )
                .await;
                return;
            };
            if let Some(sender) = shared.pending.lock().await.requests.remove(&id) {
                let _ = sender.send(Ok(message));
            }
        } else if shared.legacy.load(Ordering::Acquire)
            && message.get("id").is_some()
            && message.get("method").is_some()
        {
            let id = message.get("id").cloned().expect("checked above");
            let response = if message.get("method").and_then(Value::as_str) == Some("ping") {
                json!({"jsonrpc": "2.0", "id": id, "result": {}})
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": "Method not found"}
                })
            };
            if let Err(error) = write_message(&shared, &response).await {
                close(&Arc::downgrade(&shared), error).await;
                return;
            }
        }
        // Notifications are advisory and intentionally ignored. The only legacy
        // server request rho implements is the base-protocol ping.
    }
}

async fn close(shared: &Weak<Shared>, error: McpError) {
    let Some(shared) = shared.upgrade() else {
        return;
    };
    let mut pending = shared.pending.lock().await;
    if pending.closed.is_some() {
        return;
    }
    pending.closed = Some(error.clone());
    for (_, sender) in pending.requests.drain() {
        let _ = sender.send(Err(error.clone()));
    }
}

async fn read_frame<R>(reader: &mut R) -> Result<Option<Vec<u8>>, McpError>
where
    R: AsyncBufRead + Unpin,
{
    let mut frame = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|error| McpError::Transport {
                message: error.to_string(),
            })?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            return Err(McpError::Protocol {
                message: "MCP server closed stdout with a partial frame".to_owned(),
            });
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if frame.len() + consumed > MAX_FRAME_BYTES + 1 {
            return Err(McpError::Protocol {
                message: format!("incoming message exceeds {MAX_FRAME_BYTES} bytes"),
            });
        }
        frame.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if frame.last() == Some(&b'\n') {
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            if frame.is_empty() {
                return Err(McpError::Protocol {
                    message: "MCP server emitted an empty frame".to_owned(),
                });
            }
            return Ok(Some(frame));
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::BufReader;

    use super::*;

    #[tokio::test]
    async fn framing_accepts_crlf_and_rejects_partial_or_oversized_messages() {
        let mut reader = BufReader::new(&b"{}\r\n"[..]);
        assert_eq!(read_frame(&mut reader).await.unwrap(), Some(b"{}".to_vec()));
        assert_eq!(read_frame(&mut reader).await.unwrap(), None);

        let mut partial = BufReader::new(&b"{}"[..]);
        assert!(matches!(
            read_frame(&mut partial).await,
            Err(McpError::Protocol { .. })
        ));

        let oversized = vec![b'x'; MAX_FRAME_BYTES + 2];
        let mut oversized = BufReader::new(oversized.as_slice());
        assert!(matches!(
            read_frame(&mut oversized).await,
            Err(McpError::Protocol { .. })
        ));
    }
}
