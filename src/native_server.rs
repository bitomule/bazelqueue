#![forbid(unsafe_code)]
use crate::{platform, protocol::NativeServer, recovery::NativeState};
use anyhow::{Context, Result, bail};
use prost::Message;
use std::{fs, net::SocketAddr, os::unix::fs::MetadataExt, path::Path, time::Duration};
use tonic::{client::Grpc, codegen::http::uri::PathAndQuery, transport::Endpoint};

#[cfg(test)]
#[path = "native_server_tests.rs"]
mod tests;

#[derive(Clone, PartialEq, Message)]
struct ServerInfo {
    #[prost(int32, tag = "1")]
    pid: i32,
    #[prost(string, tag = "2")]
    address: String,
    #[prost(string, tag = "3")]
    request_cookie: String,
    #[prost(string, tag = "4")]
    response_cookie: String,
}
#[derive(Clone, PartialEq, Message)]
struct RunRequest {
    #[prost(string, tag = "1")]
    cookie: String,
    #[prost(bytes = "vec", repeated, tag = "2")]
    arg: Vec<Vec<u8>>,
    #[prost(bool, tag = "3")]
    block_for_lock: bool,
    #[prost(string, tag = "4")]
    client_description: String,
}
#[derive(Clone, PartialEq, Message)]
struct RunResponse {
    #[prost(string, tag = "1")]
    cookie: String,
    #[prost(bytes = "vec", tag = "2")]
    standard_output: Vec<u8>,
    #[prost(bool, tag = "4")]
    finished: bool,
    #[prost(int32, tag = "5")]
    exit_code: i32,
}

#[derive(Clone, PartialEq, Message)]
struct Cookie {
    #[prost(string, tag = "1")]
    cookie: String,
}

fn server_info(server: &NativeServer) -> Result<ServerInfo> {
    if !matches!(server.version.as_str(), "8.4.2" | "9.2.0" | "unverified") {
        bail!("unsupported native server protocol");
    }
    let path = Path::new(&server.output_base).join("server/server_info.rawproto");
    let file = fs::symlink_metadata(&path)?;
    if !file.is_file() || file.uid() != platform::uid() || file.len() > 65536 {
        bail!("native server identity file is not trustworthy");
    }
    let info = ServerInfo::decode(fs::read(path)?.as_slice())?;
    if info.pid <= 0 || platform::identity(info.pid as u32).as_ref() != Some(&server.identity) {
        bail!("native server identity changed");
    }
    let address: SocketAddr = info.address.parse()?;
    if !address.ip().is_loopback() {
        bail!("native server must listen on loopback");
    }
    Ok(info)
}

/// Ping is independent of Bazel's command lock. A busy command must never be
/// mistaken for an unresponsive server just because the info probe failed.
async fn ping(server: &NativeServer) -> Result<NativeState> {
    let info = server_info(server)?;
    let channel = match Endpoint::from_shared(format!("http://{}", info.address))?
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(3))
        .connect()
        .await
    {
        Ok(channel) => channel,
        Err(_) => return Ok(NativeState::Unresponsive),
    };
    let mut grpc = Grpc::new(channel);
    grpc.ready().await?;
    let response: Result<tonic::Response<Cookie>, tonic::Status> = grpc
        .unary(
            tonic::Request::new(Cookie {
                cookie: info.request_cookie,
            }),
            PathAndQuery::from_static("/command_server.CommandServer/Ping"),
            tonic_prost::ProstCodec::default(),
        )
        .await;
    match response {
        Ok(response) if response.get_ref().cookie == info.response_cookie => Ok(NativeState::Idle),
        Ok(_) => bail!("native ping response cookie mismatch"),
        Err(error)
            if matches!(
                error.code(),
                tonic::Code::DeadlineExceeded | tonic::Code::Unavailable | tonic::Code::Cancelled
            ) =>
        {
            Ok(NativeState::Unresponsive)
        }
        Err(error) => Err(error.into()),
    }
}

pub async fn probe(server: &NativeServer) -> NativeState {
    let liveness = match tokio::time::timeout(Duration::from_secs(4), ping(server)).await {
        Ok(Ok(state)) => state,
        // A timeout is only actionable with trustworthy, unchanged metadata.
        Err(_) if server_info(server).is_ok() => NativeState::Unresponsive,
        _ => return NativeState::Unknown,
    };
    if liveness == NativeState::Unresponsive {
        return liveness;
    }
    match tokio::time::timeout(Duration::from_secs(4), idle(server)).await {
        Ok(Ok(true)) => NativeState::Idle,
        Ok(Ok(false)) => NativeState::Busy,
        // Ping worked: even a stalled Run is not permission to kill the server.
        _ => NativeState::Unknown,
    }
}

/// Called only after the coordinator has rechecked the orphan, its child,
/// connection and all other lane owners. Do not target a rediscovered server.
pub async fn unresponsive_tree(server: &NativeServer) -> Result<Option<Vec<platform::Identity>>> {
    if !matches!(server.version.as_str(), "8.4.2" | "9.2.0") {
        return Ok(None);
    }
    if !platform::bazel_servers(&server.workspace)?
        .iter()
        .any(|current| {
            current.identity == server.identity && current.output_base == server.output_base
        })
    {
        return Ok(None);
    }
    if probe(server).await != NativeState::Unresponsive {
        return Ok(None);
    }
    // Re-read the metadata immediately before the identity-checked signal.
    server_info(server)?;
    Ok(Some(platform::process_tree(&server.identity)?))
}

pub async fn idle(server: &NativeServer) -> Result<bool> {
    let info = server_info(server)?;
    let current = server.identity.clone();
    let address: SocketAddr = info.address.parse()?;
    if !address.ip().is_loopback() {
        bail!("native server must listen on loopback");
    }
    let channel = Endpoint::from_shared(format!("http://{address}"))?
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(3))
        .connect()
        .await?;
    let mut grpc = Grpc::new(channel);
    grpc.ready().await?;
    let request = RunRequest {
        cookie: info.request_cookie,
        arg: vec![
            b"info".to_vec(),
            b"server_pid".to_vec(),
            b"output_base".to_vec(),
            b"release".to_vec(),
            format!("--client_cwd={}", server.workspace).into_bytes(),
        ],
        block_for_lock: false,
        client_description: "bazelqueue native idle probe".into(),
    };
    let response = grpc
        .server_streaming(
            tonic::Request::new(request),
            PathAndQuery::from_static("/command_server.CommandServer/Run"),
            tonic_prost::ProstCodec::<RunRequest, RunResponse>::default(),
        )
        .await?;
    let mut stream = response.into_inner();
    let mut output = Vec::new();
    while let Some(response) = stream.message().await? {
        if response.cookie != info.response_cookie {
            bail!("native response cookie mismatch");
        }
        output.extend(response.standard_output);
        if output.len() > 65536 {
            bail!("native probe output exceeds limit");
        }
        if response.finished {
            if response.exit_code != 0 {
                return Ok(false);
            }
            let (base, pid) = crate::bazel::parse_info(&output)
                .context("native idle probe returned no identity")?;
            let release = String::from_utf8_lossy(&output)
                .lines()
                .find_map(|line| line.strip_prefix("release: release "))
                .unwrap_or("")
                .to_owned();
            return Ok(base == server.output_base
                && pid == current.pid
                && platform::alive(&current)
                && matches!(release.as_str(), "8.4.2" | "9.2.0"));
        }
    }
    bail!("native server did not finish the probe")
}
