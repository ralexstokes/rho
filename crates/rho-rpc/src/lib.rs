//! Versioned JSON Lines RPC messages, framing, and the concurrent server shell.

mod codec;
mod message;
mod server;

pub use codec::{CodecError, decode_client_line, encode_server_line};
pub use message::{
    ClientMessage, ClientRequest, ClientResponse, ErrorObject, ResponsePayload, RpcId, ServerEvent,
    ServerMessage, ServerRequest, ServerResponse, VERSION,
};
pub use server::{ConnectionClosed, HandlerFuture, RpcHandler, RpcSender, ServeError, serve};
