//! Named-pipe protocol shared by the ferrofs service and its UIs.
//!
//! - [`protocol`]: request/response types (versioned with [`PROTOCOL_VERSION`]).
//! - [`frame`]: length-prefixed postcard framing.
//! - [`pipe`]: the pipe server (service side) and client (UI side).

pub mod frame;
pub mod pipe;
pub mod protocol;

pub use pipe::{Client, PIPE_NAME, PipeServer, ServerTrust, StopSignal};
pub use protocol::*;

/// Errors from the pipe transport and the protocol.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the ferrofs service is not running")]
    NotRunning,
    #[error("timed out waiting for the ferrofs service")]
    Timeout,
    /// The pipe is served by a process we do not trust (see `ServerTrust`).
    #[error("refusing to talk to the pipe server: {0}")]
    UntrustedServer(String),
    #[error("protocol mismatch: {0}")]
    ProtocolMismatch(String),
    #[error("message of {0} bytes exceeds the frame limit")]
    FrameTooLarge(usize),
    #[error("invalid message: {0}")]
    Decode(String),
    #[error("cannot encode message: {0}")]
    Encode(String),
    #[error("the service sent an unexpected response")]
    UnexpectedResponse,
    #[error("{context}: {source}")]
    Win32 {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
