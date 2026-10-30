use super::*;
use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{net::TcpListener, sync::Notify};
use tonic::{
    Request, Response, Status,
    body::Body,
    codegen::{
        Service, http,
        tokio_stream::{self, wrappers::TcpListenerStream},
    },
    server::{NamedService, ServerStreamingService, UnaryService},
};

type Reply<T> = Pin<Box<dyn Future<Output = Result<Response<T>, Status>> + Send>>;

#[derive(Clone, Copy)]
enum Mode {
    Idle,
    Busy,
    WrongCookie,
    HungPing,
    HungRun,
}

#[derive(Clone)]
struct Mock {
    mode: Mode,
    entered: Arc<Notify>,
    base: String,
}
impl UnaryService<Cookie> for Mock {
    type Response = Cookie;
    type Future = Reply<Cookie>;
    fn call(&mut self, request: Request<Cookie>) -> Self::Future {
        let mock = self.clone();
        Box::pin(async move {
            assert_eq!(request.into_inner().cookie, "request");
            if matches!(mock.mode, Mode::HungPing) {
                mock.entered.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(Response::new(Cookie {
                cookie: if matches!(mock.mode, Mode::WrongCookie) {
                    "wrong"
                } else {
                    "response"
                }
                .into(),
            }))
        })
    }
}
impl ServerStreamingService<RunRequest> for Mock {
    type Response = RunResponse;
    type ResponseStream = tokio_stream::Iter<std::vec::IntoIter<Result<RunResponse, Status>>>;
    type Future = Reply<Self::ResponseStream>;
    fn call(&mut self, request: Request<RunRequest>) -> Self::Future {
        let mock = self.clone();
        Box::pin(async move {
            let request = request.into_inner();
            assert_eq!(request.cookie, "request");
            assert!(!request.block_for_lock);
            if matches!(mock.mode, Mode::HungRun) {
                mock.entered.notify_one();
                std::future::pending::<()>().await;
            }
            let response = RunResponse {
                cookie: "response".into(),
                finished: true,
                exit_code: if matches!(mock.mode, Mode::Busy) {
                    9
                } else {
                    0
                },
                standard_output: format!(
                    "server_pid: {}\noutput_base: {}\nrelease: release 8.4.2\n",
                    std::process::id(),
                    mock.base
                )
                .into_bytes(),
            };
            Ok(Response::new(tokio_stream::iter(vec![Ok(response)])))
        })
    }
}
impl NamedService for Mock {
    const NAME: &'static str = "command_server.CommandServer";
}
impl Service<http::Request<Body>> for Mock {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let mock = self.clone();
        Box::pin(async move {
            if request.uri().path().ends_with("/Ping") {
                Ok(
                    tonic::server::Grpc::new(tonic_prost::ProstCodec::<Cookie, Cookie>::default())
                        .unary(mock, request)
                        .await,
                )
            } else {
                Ok(tonic::server::Grpc::new(
                    tonic_prost::ProstCodec::<RunResponse, RunRequest>::default(),
                )
                .server_streaming(mock, request)
                .await)
            }
        })
    }
}

async fn fixture(
    mode: Mode,
) -> (
    tempfile::TempDir,
    NativeServer,
    Arc<Notify>,
    tokio::task::JoinHandle<()>,
) {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let identity = platform::current_identity().unwrap();
    fs::create_dir(root.path().join("server")).unwrap();
    fs::write(
        root.path().join("server/server_info.rawproto"),
        ServerInfo {
            pid: identity.pid as i32,
            address,
            request_cookie: "request".into(),
            response_cookie: "response".into(),
        }
        .encode_to_vec(),
    )
    .unwrap();
    let base = root.path().to_string_lossy().into_owned();
    let entered = Arc::new(Notify::new());
    let mock = Mock {
        mode,
        entered: entered.clone(),
        base: base.clone(),
    };
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .serve_with_incoming(mock, TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    (
        root,
        NativeServer {
            identity,
            output_base: base.clone(),
            workspace: base,
            version: "8.4.2".into(),
        },
        entered,
        task,
    )
}

#[tokio::test]
async fn authenticated_idle_busy_and_wrong_cookie_have_distinct_outcomes() {
    for (mode, expected) in [
        (Mode::Idle, NativeState::Idle),
        (Mode::Busy, NativeState::Busy),
        (Mode::WrongCookie, NativeState::Unknown),
    ] {
        let (_root, server, _, task) = fixture(mode).await;
        assert_eq!(probe(&server).await, expected);
        task.abort();
    }
}

#[tokio::test]
async fn reused_pid_and_changed_metadata_never_prove_idleness() {
    let (_root, mut server, _, task) = fixture(Mode::Idle).await;
    server.identity.birth = "reused".into();
    assert_eq!(probe(&server).await, NativeState::Unknown);
    assert!(!can_retire(&server).await.unwrap());
    task.abort();
}

#[tokio::test]
async fn timeouts_distinguish_a_hung_ping_from_a_responsive_server() {
    for (mode, expected) in [
        (Mode::HungPing, NativeState::Unresponsive),
        (Mode::HungRun, NativeState::Unknown),
    ] {
        let (_root, server, entered, task) = fixture(mode).await;
        let probing = tokio::spawn(async move { probe(&server).await });
        entered.notified().await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(probing.await.unwrap(), expected);
        task.abort();
        tokio::time::resume();
    }
}
