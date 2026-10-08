#![forbid(unsafe_code)]
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::platform::Identity;
use std::{
    io,
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
pub struct Framed<R> {
    inner: R,
    buffer: Vec<u8>,
    length: Option<usize>,
}
impl<R> Framed<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            length: None,
        }
    }
}
impl<R: AsyncWrite + Unpin> AsyncWrite for Framed<R> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

pub const PROTOCOL: u32 = 1;
pub const MAX_FRAME: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Budget {
    pub cpu: u32,
    pub memory_mib: u64,
    pub exclusive: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub owner: Identity,
    pub lane: String,
    pub command: String,
    pub budget: Budget,
    pub prepared: bool,
    pub workspace: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeServer {
    pub identity: Identity,
    pub output_base: String,
    pub workspace: String,
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello {
        protocol: u32,
    },
    Register {
        request: Request,
    },
    Prepared {
        lane: String,
        server: Option<NativeServer>,
        budget: Budget,
    },
    Defer,
    Running {
        child: Identity,
    },
    Preparing {
        child: Identity,
    },
    RunPhase {
        child: Identity,
    },
    Finish {
        code: i32,
        stopped: bool,
    },
    Control {
        operation: String,
        id: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub sequence: u64,
    pub request: Request,
    pub state: String,
    pub child: Option<Identity>,
    pub server: Option<NativeServer>,
    pub code: Option<i32>,
    pub cancel: bool,
}

impl Job {
    pub fn holds_capacity(&self) -> bool {
        matches!(
            self.state.as_str(),
            "preparing" | "starting" | "running" | "quarantined"
        )
    }
    pub fn terminal(&self) -> bool {
        matches!(self.state.as_str(), "finished" | "cancelled")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub daemon: Identity,
    pub drained: bool,
    pub pressure: String,
    pub cpu_capacity: u32,
    pub memory_capacity_mib: u64,
    pub legacy: Vec<Identity>,
    pub jobs: Vec<Job>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Hello { protocol: u32 },
    Queued { position: usize, reason: String },
    Prepare,
    Grant { budget: Budget },
    Resume { state: String },
    Cancel,
    Ack,
    Owned { child: Identity, state: String },
    Snapshot { snapshot: Snapshot },
    Error { message: String },
}

pub async fn send<W: AsyncWrite + Unpin, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        bail!("control frame is too large");
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn receive<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut Framed<R>,
) -> Result<T> {
    loop {
        if reader.length.is_none() && reader.buffer.len() >= 4 {
            let length = u32::from_be_bytes(reader.buffer[..4].try_into()?) as usize;
            if length > MAX_FRAME {
                bail!("control frame exceeds limit");
            }
            reader.buffer.drain(..4);
            reader.length = Some(length);
        }
        if let Some(length) = reader.length
            && reader.buffer.len() >= length
        {
            let result = serde_json::from_slice(&reader.buffer[..length]);
            reader.buffer.drain(..length);
            reader.length = None;
            return Ok(result?);
        }
        let mut chunk = [0_u8; 4096];
        let size = reader
            .inner
            .read(&mut chunk)
            .await
            .context("control connection closed")?;
        if size == 0 {
            bail!("control connection closed");
        }
        reader.buffer.extend_from_slice(&chunk[..size]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "current_thread")]
    async fn selected_reads_retain_partial_headers_and_bodies() {
        for cut in [2, 7] {
            let (mut writer, reader) = tokio::io::duplex(256);
            let mut reader = Framed::new(reader);
            let bytes = serde_json::to_vec(&Event::Ack).unwrap();
            let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
            frame.extend(bytes);
            writer.write_all(&frame[..cut]).await.unwrap();
            tokio::select! {biased; result=receive::<_,Event>(&mut reader)=>panic!("incomplete frame unexpectedly completed: {result:?}"),_=async {}=>{}}
            writer.write_all(&frame[cut..]).await.unwrap();
            assert!(matches!(
                receive::<_, Event>(&mut reader).await.unwrap(),
                Event::Ack
            ));
        }
    }
}
