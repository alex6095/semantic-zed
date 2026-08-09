use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_tungstenite::tokio::connect_async;
use async_tungstenite::tungstenite::client::IntoClientRequest as _;
use async_tungstenite::tungstenite::http::HeaderValue;
use async_tungstenite::tungstenite::{Error as WebSocketError, Message};
use futures::StreamExt as _;
use futures::io::{AsyncRead, AsyncWrite};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::http::canonical_server_url;
use crate::socket_io::{
    AckMode, Packet, PacketType, SocketIoCodecError, decode_payload, encode_packet,
};

#[derive(Debug, Error)]
pub enum SocketClientError {
    #[error("invalid Socket.IO origin: {0}")]
    InvalidOrigin(#[from] url::ParseError),
    #[error("Socket.IO handshake request failed: {0}")]
    HandshakeRequest(#[from] reqwest::Error),
    #[error("Socket.IO handshake rejected ({status}): {body}")]
    HandshakeRejected { status: u16, body: String },
    #[error("Socket.IO handshake did not offer websocket transport: {0}")]
    WebSocketNotOffered(String),
    #[error("invalid Socket.IO handshake response: {0}")]
    InvalidHandshake(String),
    #[error("invalid WebSocket request header: {0}")]
    InvalidHeader(String),
    #[error("WebSocket transport failed: {0}")]
    WebSocket(Box<WebSocketError>),
    #[error("Socket.IO connection timed out during {0}")]
    Timeout(&'static str),
    #[error("Socket disconnected before acknowledgement")]
    Disconnected,
    #[error("Socket.IO transport task stopped")]
    ActorStopped,
    #[error(transparent)]
    Codec(#[from] SocketIoCodecError),
}

impl From<WebSocketError> for SocketClientError {
    fn from(error: WebSocketError) -> Self {
        Self::WebSocket(Box::new(error))
    }
}

#[derive(Debug)]
pub struct IncomingEvent {
    pub name: String,
    pub args: Vec<Value>,
    acknowledgement: Option<InboundAcknowledgement>,
}

impl IncomingEvent {
    pub fn requires_acknowledgement(&self) -> bool {
        self.acknowledgement.is_some()
    }

    pub async fn acknowledge(mut self, args: Vec<Value>) -> Result<(), SocketClientError> {
        if let Some(acknowledgement) = self.acknowledgement.take() {
            acknowledgement.respond(args).await
        } else {
            Ok(())
        }
    }

    pub fn into_parts(self) -> (String, Vec<Value>) {
        (self.name, self.args)
    }
}

#[derive(Debug)]
struct InboundAcknowledgement {
    commands: mpsc::UnboundedSender<Command>,
    id: String,
    endpoint: String,
}

impl InboundAcknowledgement {
    async fn respond(self, args: Vec<Value>) -> Result<(), SocketClientError> {
        let mut packet = Packet::new(PacketType::Ack);
        packet.endpoint = self.endpoint;
        packet.ack_id = Some(self.id);
        packet.args = args;
        self.commands
            .send(Command::Send(packet))
            .map_err(|_| SocketClientError::ActorStopped)
    }
}

enum Command {
    Send(Packet),
    EmitWithAck {
        packet: Packet,
        response: oneshot::Sender<Result<Vec<Value>, SocketClientError>>,
    },
    Disconnect,
}

pub struct SocketIo09Session {
    commands: mpsc::UnboundedSender<Command>,
    events: Mutex<mpsc::UnboundedReceiver<IncomingEvent>>,
    next_ack_id: AtomicU64,
    connected: Arc<AtomicBool>,
    pub session_id: String,
    pub heartbeat_interval: Duration,
    pub close_timeout: Duration,
}

impl SocketIo09Session {
    pub async fn connect(
        server_url: &str,
        query: &[(&str, String)],
        cookies: &str,
        timeout: Duration,
    ) -> Result<Self, SocketClientError> {
        let base_url = canonical_server_url(server_url)?;
        let origin = base_url.origin().ascii_serialization();
        let mut handshake_url = base_url.join("socket.io/1/")?;
        {
            let mut query_pairs = handshake_url.query_pairs_mut();
            for (name, value) in query {
                query_pairs.append_pair(name, value);
            }
            if !query.iter().any(|(name, _)| *name == "t") {
                query_pairs.append_pair("t", &unix_millis().to_string());
            }
        }

        let http = reqwest::Client::builder()
            .redirect_policy(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()?;
        let response = http
            .get(handshake_url.clone())
            .header("Origin", &origin)
            .header("Cookie", cookies)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(SocketClientError::HandshakeRejected {
                status: status.as_u16(),
                body,
            });
        }
        let mut fields = body.split(':');
        let session_id = fields.next().unwrap_or_default();
        let heartbeat_seconds = fields.next().unwrap_or("15").parse::<u64>().ok();
        let close_seconds = fields.next().unwrap_or("60").parse::<u64>().ok();
        let transports = fields.next().unwrap_or_default();
        if session_id.is_empty() {
            return Err(SocketClientError::InvalidHandshake(body));
        }
        if !transports
            .split(',')
            .any(|transport| transport == "websocket")
        {
            return Err(SocketClientError::WebSocketNotOffered(transports.into()));
        }

        let heartbeat_interval = Duration::from_secs(heartbeat_seconds.unwrap_or(15));
        let close_timeout = Duration::from_secs(close_seconds.unwrap_or(60));
        let mut websocket_url = handshake_url;
        websocket_url
            .set_scheme(if websocket_url.scheme() == "https" {
                "wss"
            } else {
                "ws"
            })
            .map_err(|_| SocketClientError::InvalidHandshake(body.clone()))?;
        websocket_url.set_path(&format!("/socket.io/1/websocket/{session_id}"));

        let mut request = websocket_url
            .as_str()
            .into_client_request()
            .map_err(SocketClientError::from)?;
        request.headers_mut().insert(
            "Origin",
            HeaderValue::from_str(&origin)
                .map_err(|error| SocketClientError::InvalidHeader(error.to_string()))?,
        );
        request.headers_mut().insert(
            "Cookie",
            HeaderValue::from_str(cookies)
                .map_err(|error| SocketClientError::InvalidHeader(error.to_string()))?,
        );
        let (stream, _) = tokio::time::timeout(timeout, connect_async(request))
            .await
            .map_err(|_| SocketClientError::Timeout("websocket handshake"))??;

        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let connected = Arc::new(AtomicBool::new(false));
        tokio::spawn(run_socket_actor(
            stream,
            commands_tx.clone(),
            commands_rx,
            events_tx,
            ready_tx,
            connected.clone(),
        ));

        tokio::time::timeout(timeout, ready_rx)
            .await
            .map_err(|_| SocketClientError::Timeout("connect packet"))?
            .map_err(|_| SocketClientError::Disconnected)??;
        Ok(Self {
            commands: commands_tx,
            events: Mutex::new(events_rx),
            next_ack_id: AtomicU64::new(0),
            connected,
            session_id: session_id.into(),
            heartbeat_interval,
            close_timeout,
        })
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub fn emit(&self, name: &str, args: Vec<Value>) -> Result<(), SocketClientError> {
        self.commands
            .send(Command::Send(Packet::event(name, args)))
            .map_err(|_| SocketClientError::ActorStopped)
    }

    pub async fn emit_with_ack(
        &self,
        name: &str,
        args: Vec<Value>,
        timeout: Duration,
    ) -> Result<Vec<Value>, SocketClientError> {
        if !self.is_connected() {
            return Err(SocketClientError::Disconnected);
        }
        let id = self.next_ack_id.fetch_add(1, Ordering::AcqRel) + 1;
        let mut packet = Packet::event(name, args);
        packet.id = Some(id.to_string());
        packet.ack = AckMode::Data;
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(Command::EmitWithAck {
                packet,
                response: response_tx,
            })
            .map_err(|_| SocketClientError::ActorStopped)?;
        tokio::time::timeout(timeout, response_rx)
            .await
            .map_err(|_| SocketClientError::Timeout("acknowledgement"))?
            .map_err(|_| SocketClientError::Disconnected)?
    }

    pub async fn next_event(&self) -> Option<IncomingEvent> {
        self.events.lock().await.recv().await
    }

    pub fn disconnect(&self) {
        let _ = self.commands.send(Command::Disconnect);
    }
}

async fn run_socket_actor<S>(
    stream: async_tungstenite::WebSocketStream<S>,
    commands_tx: mpsc::UnboundedSender<Command>,
    mut commands_rx: mpsc::UnboundedReceiver<Command>,
    events_tx: mpsc::UnboundedSender<IncomingEvent>,
    ready_tx: oneshot::Sender<Result<(), SocketClientError>>,
    connected: Arc<AtomicBool>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut writer, mut reader) = stream.split();
    let mut pending =
        HashMap::<String, oneshot::Sender<Result<Vec<Value>, SocketClientError>>>::new();
    let mut ready_tx = Some(ready_tx);
    'actor: loop {
        tokio::select! {
            command = commands_rx.recv() => {
                let Some(command) = command else { break };
                match command {
                    Command::Send(packet) => {
                        if send_packet(&mut writer, &packet).await.is_err() { break; }
                    }
                    Command::EmitWithAck { packet, response } => {
                        let id = packet.id.clone().unwrap_or_default();
                        pending.insert(id.clone(), response);
                        if let Err(error) = send_packet(&mut writer, &packet).await {
                            if let Some(response) = pending.remove(&id) {
                                let _ = response.send(Err(error));
                            }
                            break;
                        }
                    }
                    Command::Disconnect => {
                        let _ = send_packet(&mut writer, &Packet::new(PacketType::Disconnect)).await;
                        let _ = writer.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
            message = reader.next() => {
                let Some(message) = message else { break };
                let payload = match message {
                    Ok(Message::Text(value)) => value.to_string(),
                    Ok(Message::Binary(value)) => String::from_utf8_lossy(&value).into_owned(),
                    Ok(Message::Ping(value)) => {
                        let _ = writer.send(Message::Pong(value)).await;
                        continue;
                    }
                    Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => continue,
                    Ok(Message::Close(_)) | Err(_) => break,
                };
                let packets = match decode_payload(&payload) {
                    Ok(packets) => packets,
                    Err(error) => {
                        if let Some(ready) = ready_tx.take() {
                            let _ = ready.send(Err(error.into()));
                        }
                        break;
                    }
                };
                for packet in packets {
                    match packet.packet_type {
                        PacketType::Connect => {
                            connected.store(true, Ordering::Release);
                            if let Some(ready) = ready_tx.take() {
                                let _ = ready.send(Ok(()));
                            }
                        }
                        PacketType::Heartbeat => {
                            if send_packet(&mut writer, &Packet::new(PacketType::Heartbeat)).await.is_err() {
                                break 'actor;
                            }
                        }
                        PacketType::Ack => {
                            if let Some(id) = packet.ack_id
                                && let Some(response) = pending.remove(&id)
                            {
                                let _ = response.send(Ok(packet.args));
                            }
                        }
                        PacketType::Event => {
                            let acknowledgement = match (packet.ack, packet.id) {
                                (AckMode::Data, Some(id)) => Some(InboundAcknowledgement {
                                    commands: commands_tx.clone(),
                                    id,
                                    endpoint: packet.endpoint,
                                }),
                                _ => None,
                            };
                            if let Some(name) = packet.event {
                                let _ = events_tx.send(IncomingEvent {
                                    name,
                                    args: packet.args,
                                    acknowledgement,
                                });
                            }
                        }
                        PacketType::Error => {
                            if let Some(ready) = ready_tx.take() {
                                let message = packet.reason.unwrap_or_else(|| "Socket.IO error".into());
                                let _ = ready.send(Err(SocketClientError::InvalidHandshake(message)));
                            }
                        }
                        PacketType::Disconnect => break 'actor,
                        _ => {}
                    }
                }
            }
        }
    }
    connected.store(false, Ordering::Release);
    if let Some(ready) = ready_tx.take() {
        let _ = ready.send(Err(SocketClientError::Disconnected));
    }
    for (_, response) in pending {
        let _ = response.send(Err(SocketClientError::Disconnected));
    }
}

async fn send_packet<S>(
    writer: &mut async_tungstenite::WebSocketSender<S>,
    packet: &Packet,
) -> Result<(), SocketClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let encoded = encode_packet(packet)?;
    writer.send(Message::Text(encoded.into())).await?;
    Ok(())
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_tungstenite::tokio::accept_async;
    use futures::StreamExt as _;
    use serde_json::json;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::socket_io::{Packet, PacketType, decode_payload, encode_packet};

    async fn read_http_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0u8; 2048];
        loop {
            let count = stream.read(&mut buffer).await.unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
            if request.windows(4).any(|value| value == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(request).unwrap()
    }

    async fn serve_handshake(listener: &TcpListener, session_id: &str) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut stream).await;
        assert!(request.starts_with("GET /socket.io/1/?"));
        let body = format!("{session_id}:15:60:websocket");
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn real_websocket_transport_round_trips_heartbeat_and_ack() {
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server_listener = listener.clone();
        let server = tokio::spawn(async move {
            serve_handshake(&server_listener, "native-session").await;
            let (stream, _) = server_listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            websocket
                .send(Message::Text(
                    encode_packet(&Packet::new(PacketType::Connect))
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            websocket
                .send(Message::Text(
                    encode_packet(&Packet::new(PacketType::Heartbeat))
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();

            let mut event = None;
            while event.is_none() {
                let message = websocket.next().await.unwrap().unwrap();
                let Message::Text(payload) = message else {
                    continue;
                };
                let packet = decode_payload(&payload).unwrap().remove(0);
                if packet.packet_type == PacketType::Event {
                    event = Some(packet);
                } else {
                    assert_eq!(packet.packet_type, PacketType::Heartbeat);
                }
            }
            let event = event.unwrap();
            assert_eq!(event.event.as_deref(), Some("joinProject"));
            let mut ack = Packet::new(PacketType::Ack);
            ack.ack_id = event.id;
            ack.args = vec![Value::Null, json!({ "rootFolder": [] })];
            websocket
                .send(Message::Text(encode_packet(&ack).unwrap().into()))
                .await
                .unwrap();
        });

        let session =
            SocketIo09Session::connect(&url, &[], "session=active", Duration::from_secs(2))
                .await
                .unwrap();
        assert_eq!(session.session_id, "native-session");
        let values = session
            .emit_with_ack(
                "joinProject",
                vec![json!({ "project_id": "paper-1" })],
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(values[1]["rootFolder"], json!([]));
        server.await.unwrap();
    }
}
