use hyper::{
    body::Body,
    server::{conn::AddrStream, Server},
    service, Error as HyperError, Request, Response,
};
use serde::Deserialize;
use std::{
    convert::TryInto,
    env,
    error::Error,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;
use twilight_gateway_queue::{LargeBotQueue, LocalQueue, Queue};
use twilight_http::Client;

const PROCESSED: &[u8] = br#"{"message": "You're free to connect now! :)"}"#;

#[cfg(windows)]
async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");

    tokio::select! {
        _ = sigint.recv() => {},
        _ = sigterm.recv() => {},
    };
}

#[derive(Deserialize)]
struct QueryParameters {
    shard: u64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let host_raw = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let host = IpAddr::from_str(&host_raw)?;
    let port = env::var("PORT").unwrap_or_else(|_| "80".into()).parse()?;

    let big_queue: bool;

    let queue: Arc<dyn Queue> = {
        if let Ok(token) = env::var("DISCORD_TOKEN") {
            let http_client = Arc::new(Client::new(token));

            let gateway = http_client
                .gateway()
                .authed()
                .exec()
                .await
                .expect("Cannot fetch gateway information");

            let concurrency = gateway
                .model()
                .await?
                .session_start_limit
                .max_concurrency
                .try_into()
                .unwrap();

            big_queue = concurrency > 1;

            Arc::new(LargeBotQueue::new(concurrency, http_client).await)
        } else {
            big_queue = false;
            Arc::new(LocalQueue::new())
        }
    };

    let address = SocketAddr::from((host, port));

    // The closure inside `make_service_fn` is run for each connection,
    // creating a 'service' to handle requests for that specific connection.
    let service = service::make_service_fn(move |addr: &AddrStream| {
        debug!("Connection from: {:?}", addr);
        let queue = queue.clone();

        async move {
            Ok::<_, HyperError>(service::service_fn(move |request: Request<Body>| {
                let queue = queue.clone();

                let mut shard = None;

                if big_queue {
                    if let Some(query) = request.uri().query() {
                        if let Ok(params) = serde_urlencoded::from_str::<QueryParameters>(query) {
                            shard = Some(params.shard);
                        }
                    }

                    if shard.is_none() {
                        warn!(
                            "No shard id set, defaulting to 0. Will not bucket requests correctly!"
                        );
                    }
                }
                async move {
                    queue.request([shard.unwrap_or(0), 1]).await;

                    let body = Body::from(PROCESSED.to_vec());

                    Ok::<Response<Body>, HyperError>(Response::new(body))
                }
            }))
        }
    });

    let server = Server::bind(&address).serve(service);

    let graceful = server.with_graceful_shutdown(shutdown_signal());

    info!("Listening on http://{}", address);

    if let Err(why) = graceful.await {
        error!("Fatal server error: {}", why);
    }

    Ok(())
}
