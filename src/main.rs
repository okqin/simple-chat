use anyhow::Result;
use dashmap::DashMap;
use derive_more::Display;
use futures::SinkExt;
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_stream::StreamExt;
use tokio_util::codec::{Framed, LinesCodec};
use tracing::{debug, error, info, instrument, warn};
use tracing_subscriber::{filter::LevelFilter, fmt::format::FmtSpan, EnvFilter};

const MAX_CHANNELS: usize = 500;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(LevelFilter::INFO.into()))
        .with_span_events(FmtSpan::CLOSE)
        .init();

    let state = Arc::new(Shared::new());

    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:8080".to_string());

    let listener = TcpListener::bind(&addr).await?;

    info!("Server listening on: {}", addr);

    loop {
        let (stream, addr) = listener.accept().await?;
        let state_clone = Arc::clone(&state);
        tokio::spawn(async move {
            debug!("Accepted connection from: {}", addr);
            if let Err(e) = handle_connection(state_clone, stream, addr).await {
                error!("Error handling connection: {}", e);
            }
        });
    }
}

type Tx = mpsc::Sender<Arc<Message>>;
type Rx = mpsc::Receiver<Arc<Message>>;

#[derive(Debug, Display)]
enum Message {
    #[display("[{_0} joined the chat 😆]")]
    UserJoined(String),

    #[display("[{_0} left the chat 😢]")]
    UserLeft(String),

    #[display("{username}: {msg}")]
    Chat { username: String, msg: String },
}

impl Message {
    fn chat(username: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::Chat {
            username: username.into(),
            msg: msg.into(),
        }
    }
}

#[derive(Debug, Default)]
struct Shared {
    peers: DashMap<SocketAddr, Tx>,
}

impl Shared {
    fn new() -> Self {
        Self::default()
    }

    async fn broadcast(&self, sender: SocketAddr, message: Arc<Message>) {
        for peer in self.peers.iter() {
            if peer.key() != &sender {
                if let Err(e) = peer.value().send(message.clone()).await {
                    error!("Failed to send message to {}: {}", peer.key(), e);
                    self.peers.remove(peer.key());
                }
            }
        }
    }
}

#[derive(Debug)]
struct Peer {
    lines: Framed<TcpStream, LinesCodec>,
    rx: Rx,
}

impl Peer {
    async fn new(
        state: Arc<Shared>,
        lines: Framed<TcpStream, LinesCodec>,
        addr: SocketAddr,
    ) -> Self {
        let (tx, rx) = mpsc::channel(MAX_CHANNELS);

        state.peers.insert(addr, tx);

        Self { lines, rx }
    }
}

#[instrument(skip(state, stream))]
async fn handle_connection(state: Arc<Shared>, stream: TcpStream, addr: SocketAddr) -> Result<()> {
    let mut lines = Framed::new(stream, LinesCodec::new());

    lines.send("Please input your username:").await?;

    let username = match lines.next().await {
        Some(Ok(line)) => line,
        _ => {
            warn!("Failed to read username from {}, Client disconnected", addr);
            return Ok(());
        }
    };

    let mut peer = Peer::new(state.clone(), lines, addr).await;

    let msg = Arc::new(Message::UserJoined(username.clone()));
    info!("{}", msg);
    state.broadcast(addr, msg).await;

    loop {
        tokio::select! {
            result = peer.lines.next() => {
                match result {
                    Some(Ok(msg)) => {
                        let msg = Arc::new(Message::chat(&username, msg));
                        state.broadcast(addr, msg).await;
                    }
                    Some(Err(e)) => {
                        warn!("Failed to read messages for {}: {}", &username, e);
                    }
                    None => break,
                }
            }
            Some(msg) = peer.rx.recv() => {
                peer.lines.send(msg.to_string()).await?;
            }
        }
    }

    state.peers.remove(&addr);

    let msg = Arc::new(Message::UserLeft(username.clone()));
    info!("{} disconnected", addr);
    state.broadcast(addr, msg).await;

    Ok(())
}
