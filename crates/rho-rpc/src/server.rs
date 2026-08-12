use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::mpsc,
};

use crate::{
    ClientMessage, ClientRequest, ClientResponse, CodecError, ErrorObject, RpcId, ServerEvent,
    ServerMessage, ServerRequest, ServerResponse, VERSION, decode_client_line, encode_server_line,
};

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const OUTBOUND_CAPACITY: usize = 256;

/// Type-erased handler operation.
pub type HandlerFuture<'handler, T> = Pin<Box<dyn Future<Output = T> + Send + 'handler>>;

/// Application projection behind an RPC connection.
pub trait RpcHandler: Send + Sync + 'static {
    /// Handles one client method request.
    fn request(
        &self,
        request: ClientRequest,
        outbound: RpcSender,
    ) -> HandlerFuture<'_, Result<Value, ErrorObject>>;

    /// Handles one client answer to a server-initiated request.
    fn response(
        &self,
        response: ClientResponse,
        outbound: RpcSender,
    ) -> HandlerFuture<'_, Result<(), ErrorObject>>;
}

/// Cloneable, backpressured sending side of one RPC connection.
#[derive(Clone)]
pub struct RpcSender {
    outbound: mpsc::Sender<ServerMessage>,
}

impl RpcSender {
    /// Emits an advisory event.
    pub async fn event(
        &self,
        event: impl Into<String>,
        data: Value,
    ) -> Result<(), ConnectionClosed> {
        self.send(ServerMessage::Event(ServerEvent {
            v: VERSION,
            event: event.into(),
            data,
        }))
        .await
    }

    /// Sends a request that the client answers with a response frame.
    pub async fn request(
        &self,
        id: impl Into<RpcId>,
        method: impl Into<String>,
        params: Value,
    ) -> Result<(), ConnectionClosed> {
        self.send(ServerMessage::Request(ServerRequest {
            v: VERSION,
            id: id.into(),
            method: method.into(),
            params,
        }))
        .await
    }

    async fn send(&self, message: ServerMessage) -> Result<(), ConnectionClosed> {
        self.outbound
            .send(message)
            .await
            .map_err(|_| ConnectionClosed)
    }
}

/// The peer closed its RPC connection.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("RPC connection is closed")]
pub struct ConnectionClosed;

/// RPC framing or transport failure.
#[derive(Debug, Error)]
pub enum ServeError {
    /// Reading or writing the transport failed.
    #[error("RPC transport failed: {0}")]
    Io(#[from] std::io::Error),
    /// A client frame was malformed.
    #[error(transparent)]
    Codec(#[from] CodecError),
    /// An outbound producer lost the connection.
    #[error(transparent)]
    Closed(#[from] ConnectionClosed),
}

/// Serves one full-duplex JSON Lines connection until EOF or failure.
///
/// Requests are dispatched concurrently so control commands and interaction
/// answers remain responsive while repository work or actor startup is in flight.
pub async fn serve<R, W, H>(reader: R, mut writer: W, handler: Arc<H>) -> Result<(), ServeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    H: RpcHandler,
{
    let mut reader = BufReader::new(reader);
    let (outbound, mut messages) = mpsc::channel(OUTBOUND_CAPACITY);
    let sender = RpcSender { outbound };
    loop {
        tokio::select! {
            frame = read_frame(&mut reader) => {
                let Some(frame) = frame? else {
                    return Ok(());
                };
                match decode_client_line(&frame)? {
                    ClientMessage::Request(request) => {
                        dispatch_request(Arc::clone(&handler), sender.clone(), request);
                    }
                    ClientMessage::Response(response) => {
                        if response.v != VERSION {
                            return Err(CodecError::Shape(format!(
                                "unsupported client response version {}; expected {VERSION}",
                                response.v
                            )).into());
                        }
                        dispatch_response(Arc::clone(&handler), sender.clone(), response);
                    }
                }
            }
            message = messages.recv() => {
                let Some(message) = message else {
                    return Ok(());
                };
                writer.write_all(&encode_server_line(&message)?).await?;
                writer.flush().await?;
            }
        }
    }
}

fn dispatch_request<H>(handler: Arc<H>, sender: RpcSender, request: ClientRequest)
where
    H: RpcHandler,
{
    tokio::spawn(async move {
        let id = request.id.clone();
        let response = if request.v == VERSION {
            match handler.request(request, sender.clone()).await {
                Ok(result) => ServerResponse::success(id, result),
                Err(error) => ServerResponse::failure(id, error),
            }
        } else {
            ServerResponse::failure(
                id,
                ErrorObject::new(
                    "unsupported_version",
                    format!("unsupported RPC version {}; expected {VERSION}", request.v),
                )
                .with_data(json!({"supported": [VERSION]})),
            )
        };
        let _ = sender.send(ServerMessage::Response(response)).await;
    });
}

fn dispatch_response<H>(handler: Arc<H>, sender: RpcSender, response: ClientResponse)
where
    H: RpcHandler,
{
    tokio::spawn(async move {
        let id = response.id.clone();
        if let Err(error) = handler.response(response, sender.clone()).await {
            let _ = sender
                .event(
                    "rpc.client_response_rejected",
                    json!({"id": id, "error": error}),
                )
                .await;
        }
    });
}

async fn read_frame<R>(reader: &mut R) -> Result<Option<Vec<u8>>, ServeError>
where
    R: AsyncBufRead + Unpin,
{
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            return Err(CodecError::InvalidFrame.into());
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if frame.len() + consumed > MAX_FRAME_BYTES + 1 {
            return Err(
                CodecError::Shape(format!("RPC frame exceeds {MAX_FRAME_BYTES} bytes")).into(),
            );
        }
        frame.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if frame.last() == Some(&b'\n') {
            return Ok(Some(frame));
        }
    }
}

#[cfg(test)]
mod tests;
