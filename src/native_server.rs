#![forbid(unsafe_code)]
use crate::{platform, protocol::NativeServer};
use anyhow::{Context, Result, bail};
use prost::Message;
use std::{fs, net::SocketAddr, os::unix::fs::MetadataExt, path::Path, time::Duration};
use tonic::{client::Grpc, codegen::http::uri::PathAndQuery, transport::Endpoint};

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

pub async fn idle(server: &NativeServer) -> Result<bool> {
    if !matches!(server.version.as_str(), "8.4.2" | "9.2.0" | "unverified") {
        bail!("unsupported native server protocol");
    }
    let path = Path::new(&server.output_base).join("server/server_info.rawproto");
    let file = fs::symlink_metadata(&path)?;
    if !file.is_file() || file.uid() != platform::uid() || file.len() > 65536 {
        bail!("native server identity file is not trustworthy");
    }
    let info = ServerInfo::decode(fs::read(path)?.as_slice())?;
    let current =
        platform::identity(info.pid as u32).context("native server identity unavailable")?;
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
            let (_, pid) = crate::bazel::parse_info(&output)
                .context("native idle probe returned no identity")?;
            let release = String::from_utf8_lossy(&output)
                .lines()
                .find_map(|line| line.strip_prefix("release: release "))
                .unwrap_or("")
                .to_owned();
            return Ok(pid == current.pid
                && platform::alive(&current)
                && matches!(release.as_str(), "8.4.2" | "9.2.0"));
        }
    }
    bail!("native server did not finish the probe")
}
