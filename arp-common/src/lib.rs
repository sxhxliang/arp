use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub mod http;

/// Commands exchanged between client and server.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Command {
    /// Register a new client. Sent from arpc to arps.
    Register { client_id: String },
    /// Result of the registration. Sent from arps to arpc.
    RegisterResult {
        success: bool,
        error: Option<String>,
    },
    /// Request a new proxy connection. Sent from arps to a chosen arpc.
    RequestNewProxyConn { proxy_conn_id: String },
    /// Notify the proxy listener that a new client is ready. Sent from arpc to arps.
    NewProxyConn {
        proxy_conn_id: String,
        client_id: String,
    },
}

/// Reads a command from an async reader.
/// The format is a 4-byte length prefix (u32) followed by the JSON-encoded command.
pub async fn read_command<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Command> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;

    serde_json::from_slice(&buf).map_err(|e| anyhow!("Failed to deserialize command: {}", e))
}

/// Writes a command to an async writer.
/// The format is a 4-byte length prefix (u32) followed by the JSON-encoded command.
pub async fn write_command<W: AsyncWrite + Unpin>(writer: &mut W, command: &Command) -> Result<()> {
    let buf = serde_json::to_vec(command)?;
    let len = buf.len() as u32;

    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

/// Joins two streams, copying data in both directions.
/// Optimized version using copy_bidirectional for better performance.
pub async fn join_streams<A, B>(mut a: A, mut b: B) -> std::io::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    // tokio::io::copy_bidirectional is more efficient than dual tokio::select!
    // It uses a single buffer pool and optimized copying logic
    tokio::io::copy_bidirectional(&mut a, &mut b).await?;
    Ok(())
}
