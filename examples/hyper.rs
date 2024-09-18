#![cfg(not(any(miri, pin_scoped_loom)))]

use std::{
    pin::pin,
    sync::{atomic::AtomicUsize, Arc},
};

use http_body_util::{BodyExt, Full};
use hyper::{
    body::{Bytes, Incoming},
    server::conn::http1::Builder as Http1Builder,
    service::service_fn,
    Request, Response,
};
use hyper_util::rt::TokioIo;
use pin_scoped::{async_fn::AsyncFnOnceRef, Scope};
use tokio::{
    net::{TcpListener, TcpStream},
    signal::ctrl_c,
};
use tokio_util::task::TaskTracker;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + 'static>> {
    let listener = TcpListener::bind("0.0.0.0:0").await?;
    println!("listening on {:?}", listener.local_addr()?);

    let server = Http1Builder::new();
    let state = State {
        requests: AtomicUsize::new(0),
    };

    let state = if std::env::var("SCOPED")? == "true" {
        scoped_loop(state, server, listener).await?
    } else {
        arced_loop(state, server, listener).await?
    };

    dbg!(state);

    Ok(())
}

async fn arced_loop(
    state: State,
    server: Http1Builder,
    listener: TcpListener,
) -> Result<State, Box<dyn std::error::Error + 'static>> {
    let state = Arc::new(state);
    let tracker = TaskTracker::new();
    let mut close = pin!(ctrl_c());

    loop {
        tokio::select! {
            _ = close.as_mut() => { break },
            res = listener.accept() => {
                let (io, _) = res?;
                let state = state.clone();
                let server = server.clone();
                tracker.spawn(async move {
                    ServerConnection(server, io).call(&state).await
                });
            }
        }
    }

    tracker.close();
    tracker.wait().await;
    Ok(Arc::into_inner(state).unwrap())
}

async fn scoped_loop(
    state: State,
    server: Http1Builder,
    listener: TcpListener,
) -> Result<State, Box<dyn std::error::Error + 'static>> {
    let scope = pin!(Scope::new(state));
    let mut close = pin!(ctrl_c());

    loop {
        tokio::select! {
            _ = close.as_mut() => { break },
            res = listener.accept() => {
                let (io, _) = res?;
                scope.as_ref().spawn(ServerConnection(server.clone(), io));
            }
        }
    }

    Ok(scope.await)
}

struct ServerConnection(Http1Builder, TcpStream);
impl AsyncFnOnceRef<State, ()> for ServerConnection {
    async fn call(self, state: &State) {
        let _: Result<(), _> = self
            .0
            .serve_connection(
                TokioIo::new(self.1),
                service_fn(|req| handle_request(state, req)),
            )
            .await;
    }
}

#[derive(Debug)]
struct State {
    requests: AtomicUsize,
}

async fn handle_request(
    state: &State,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    state
        .requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(Response::new(Full::new(
        request.into_body().collect().await?.to_bytes(),
    )))
}
