use std::collections::VecDeque;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::http::Identity;
use crate::ot::{HistoryOperation, ShareJsOperation, decode_packed_utf8, history_snapshot_text};
use crate::socket_client::{SocketClientError, SocketIo09Session};

const DEFAULT_ACK_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketScheme {
    V1,
    V2,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RealtimeEvent {
    pub name: String,
    pub args: Vec<Value>,
}

/// The single, lossless stream of events for one realtime session.
///
/// The Socket.IO bridge writes to a bounded channel and awaits capacity rather than dropping an
/// event. During the V2 handshake, events which arrive before `joinProjectResponse` are retained
/// here and delivered to the sync actor afterwards in their original order.
pub struct RealtimeEventStream {
    buffered: VecDeque<RealtimeEvent>,
    receiver: mpsc::Receiver<RealtimeEvent>,
}

impl RealtimeEventStream {
    fn new(receiver: mpsc::Receiver<RealtimeEvent>) -> Self {
        Self {
            buffered: VecDeque::new(),
            receiver,
        }
    }

    fn retain_before_join(&mut self, events: VecDeque<RealtimeEvent>) {
        self.buffered.extend(events);
    }

    pub async fn recv(&mut self) -> Option<RealtimeEvent> {
        match self.buffered.pop_front() {
            Some(event) => Some(event),
            None => self.receiver.recv().await,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSnapshot {
    pub document_id: String,
    pub text: String,
    pub lines: Vec<String>,
    pub version: u64,
    pub updates: Vec<Value>,
    pub ranges: Value,
    pub ot_type: String,
    pub raw_snapshot: Value,
    pub read_only_reason: Option<String>,
}

#[derive(Debug, Error)]
pub enum RealtimeError {
    #[error(transparent)]
    Transport(#[from] SocketClientError),
    #[error("Overleaf realtime service returned an error for {event}: {message}")]
    Remote { event: String, message: String },
    #[error("invalid {0} response from Overleaf realtime service")]
    InvalidResponse(&'static str),
    #[error("unsupported Overleaf OT type: {0}")]
    UnsupportedOtType(String),
    #[error("Overleaf realtime event stream closed")]
    EventStreamClosed,
    #[error("Overleaf v2 joinProjectResponse timed out")]
    JoinProjectTimeout,
    #[error("history-OT snapshot could not be materialized: {0}")]
    HistorySnapshot(String),
}

pub struct OverleafRealtimeSession {
    transport: Arc<SocketIo09Session>,
    scheme: SocketScheme,
    project_id: String,
    project: Value,
    public_id: Arc<RwLock<Option<String>>>,
    // Install the sole event receiver before the Socket.IO bridge starts. `joinProject` gives us
    // a point-in-time model, but project events can be emitted immediately afterwards while the
    // native replica is bootstrapping files. This bounded stream applies backpressure instead of
    // retaining a lossy broadcast tail or adding a reconciliation fallback.
    startup_events: Mutex<Option<RealtimeEventStream>>,
    event_task: JoinHandle<()>,
    ack_timeout: Duration,
}

impl OverleafRealtimeSession {
    pub async fn connect_and_join(
        server_url: &str,
        identity: &Identity,
        project_id: &str,
    ) -> Result<Self, RealtimeError> {
        Self::connect_and_join_with_timeout(server_url, identity, project_id, DEFAULT_ACK_TIMEOUT)
            .await
    }

    pub async fn connect_and_join_with_timeout(
        server_url: &str,
        identity: &Identity,
        project_id: &str,
        ack_timeout: Duration,
    ) -> Result<Self, RealtimeError> {
        let mut last_error = None;
        for scheme in [SocketScheme::V1, SocketScheme::V2] {
            match Self::connect_scheme(server_url, identity, project_id, scheme, ack_timeout).await
            {
                Ok(session) => return Ok(session),
                Err(error) => {
                    last_error = Some(error);
                    if scheme == SocketScheme::V1 {
                        continue;
                    }
                }
            }
        }
        Err(last_error.unwrap_or(RealtimeError::EventStreamClosed))
    }

    async fn connect_scheme(
        server_url: &str,
        identity: &Identity,
        project_id: &str,
        scheme: SocketScheme,
        ack_timeout: Duration,
    ) -> Result<Self, RealtimeError> {
        let query = match scheme {
            SocketScheme::V1 => Vec::new(),
            SocketScheme::V2 => vec![("projectId", project_id.to_owned())],
        };
        let transport = Arc::new(
            SocketIo09Session::connect(server_url, &query, &identity.cookies, ack_timeout).await?,
        );
        let (event_sender, event_receiver) = mpsc::channel(1024);
        let mut startup_events = RealtimeEventStream::new(event_receiver);
        let public_id = Arc::new(RwLock::new(None));
        let event_task = spawn_event_bridge(transport.clone(), event_sender, public_id.clone());

        let project_result = async {
            match scheme {
                SocketScheme::V1 => {
                let values = emit_overleaf_ack(
                    &transport,
                    "joinProject",
                    vec![json!({ "project_id": project_id })],
                    ack_timeout,
                )
                .await?;
                let project = values
                    .first()
                    .cloned()
                    .ok_or(RealtimeError::InvalidResponse("v1 joinProject"))?;
                if !project.get("rootFolder").is_some_and(Value::is_array) {
                    return Err(RealtimeError::InvalidResponse("v1 joinProject"));
                    }
                    Ok(project)
                }
                SocketScheme::V2 => {
                    let deadline = tokio::time::sleep(ack_timeout);
                    tokio::pin!(deadline);
                    let mut events_before_join = VecDeque::new();
                    let result = loop {
                        tokio::select! {
                            _ = &mut deadline => {
                                break Err(RealtimeError::JoinProjectTimeout);
                            }
                            event = startup_events.recv() => {
                                let Some(event) = event else {
                                    break Err(RealtimeError::EventStreamClosed);
                                };
                                if event.name != "joinProjectResponse" {
                                    events_before_join.push_back(event);
                                    continue;
                                }
                                let (project, parsed_public_id) = parse_join_project_response(&event.args)
                                    .ok_or(RealtimeError::InvalidResponse("v2 joinProjectResponse"))?;
                                if let Some(parsed_public_id) = parsed_public_id {
                                    *public_id.write().unwrap() = Some(parsed_public_id);
                                }
                                break Ok(project);
                            }
                        }
                    };
                    startup_events.retain_before_join(events_before_join);
                    result
                }
            }
        }
        .await;
        let project = match project_result {
            Ok(project) => project,
            Err(error) => {
                transport.disconnect();
                event_task.abort();
                return Err(error);
            }
        };

        Ok(Self {
            transport,
            scheme,
            project_id: project_id.into(),
            project,
            public_id,
            startup_events: Mutex::new(Some(startup_events)),
            event_task,
            ack_timeout,
        })
    }

    pub fn scheme(&self) -> SocketScheme {
        self.scheme
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn project(&self) -> &Value {
        &self.project
    }

    pub fn public_id(&self) -> Option<String> {
        self.public_id.read().unwrap().clone()
    }

    /// Returns the receiver that was installed before the Socket.IO bridge began receiving.
    /// It is consumed exactly once by the native sync actor during connection/bootstrap.
    pub fn take_startup_events(&self) -> Result<RealtimeEventStream, RealtimeError> {
        self.startup_events
            .lock()
            .unwrap()
            .take()
            .ok_or(RealtimeError::EventStreamClosed)
    }

    pub async fn join_document(
        &self,
        document_id: &str,
        from_version: Option<u64>,
        age: Option<u64>,
    ) -> Result<DocumentSnapshot, RealtimeError> {
        let mut options = json!({ "encodeRanges": true, "supportsHistoryOT": true });
        if let Some(age) = age {
            options["age"] = json!(age);
        }
        let mut args = vec![json!(document_id)];
        if let Some(from_version) = from_version {
            args.push(json!(from_version));
        }
        args.push(options);
        let values = emit_overleaf_ack(&self.transport, "joinDoc", args, self.ack_timeout).await?;
        if values.len() < 2 {
            return Err(RealtimeError::InvalidResponse("joinDoc"));
        }
        let raw_snapshot = values[0].clone();
        let version = values[1]
            .as_u64()
            .ok_or(RealtimeError::InvalidResponse("joinDoc version"))?;
        let updates = values
            .get(2)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let ranges = values.get(3).cloned().unwrap_or(Value::Null);
        let ot_type = values
            .get(4)
            .and_then(Value::as_str)
            .unwrap_or("sharejs-text-ot")
            .to_owned();
        let (text, lines, read_only_reason) = match ot_type.as_str() {
            "sharejs-text-ot" => {
                let packed_lines = raw_snapshot
                    .as_array()
                    .ok_or(RealtimeError::InvalidResponse("ShareJS joinDoc snapshot"))?;
                let lines = packed_lines
                    .iter()
                    .map(|line| {
                        line.as_str()
                            .map(decode_packed_utf8)
                            .ok_or(RealtimeError::InvalidResponse("ShareJS document line"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                (lines.join("\n"), lines, None)
            }
            "history-ot" => {
                let text = history_snapshot_text(&raw_snapshot)
                    .map_err(|error| RealtimeError::HistorySnapshot(error.to_string()))?;
                let read_only_reason = history_snapshot_has_tracked_deletes(&raw_snapshot).then(|| {
                    "Tracked deletions are present; content-only local editing is disabled until native range editing is available.".into()
                });
                (
                    text.clone(),
                    text.split('\n').map(str::to_owned).collect(),
                    read_only_reason,
                )
            }
            unsupported => return Err(RealtimeError::UnsupportedOtType(unsupported.into())),
        };
        Ok(DocumentSnapshot {
            document_id: document_id.into(),
            text,
            lines,
            version,
            updates,
            ranges,
            ot_type,
            raw_snapshot,
            read_only_reason,
        })
    }

    pub async fn leave_document(&self, document_id: &str) -> Result<(), RealtimeError> {
        emit_overleaf_ack(
            &self.transport,
            "leaveDoc",
            vec![json!(document_id)],
            self.ack_timeout,
        )
        .await?;
        Ok(())
    }

    pub async fn apply_sharejs_update(
        &self,
        document_id: &str,
        version: u64,
        operations: &[ShareJsOperation],
    ) -> Result<(), RealtimeError> {
        self.apply_update(
            document_id,
            json!({ "doc": document_id, "v": version, "op": operations }),
        )
        .await
    }

    pub async fn apply_history_update(
        &self,
        document_id: &str,
        version: u64,
        operations: &[HistoryOperation],
    ) -> Result<(), RealtimeError> {
        self.apply_update(
            document_id,
            json!({ "doc": document_id, "v": version, "op": operations }),
        )
        .await
    }

    pub async fn apply_update(
        &self,
        document_id: &str,
        update: Value,
    ) -> Result<(), RealtimeError> {
        emit_overleaf_ack(
            &self.transport,
            "applyOtUpdate",
            vec![json!(document_id), update],
            self.ack_timeout,
        )
        .await?;
        Ok(())
    }

    pub async fn update_position(&self, cursor: Value) -> Result<(), RealtimeError> {
        emit_overleaf_ack(
            &self.transport,
            "clientTracking.updatePosition",
            vec![cursor],
            self.ack_timeout,
        )
        .await?;
        Ok(())
    }

    pub async fn connected_users(&self) -> Result<Vec<Value>, RealtimeError> {
        let values = emit_overleaf_ack(
            &self.transport,
            "clientTracking.getConnectedUsers",
            vec![],
            self.ack_timeout,
        )
        .await?;
        Ok(values
            .first()
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub fn disconnect(&self) {
        self.transport.disconnect();
        self.event_task.abort();
    }
}

impl Drop for OverleafRealtimeSession {
    fn drop(&mut self) {
        self.transport.disconnect();
        self.event_task.abort();
    }
}

async fn emit_overleaf_ack(
    transport: &SocketIo09Session,
    event: &str,
    args: Vec<Value>,
    timeout: Duration,
) -> Result<Vec<Value>, RealtimeError> {
    let mut values = transport.emit_with_ack(event, args, timeout).await?;
    if values.is_empty() {
        return Ok(values);
    }
    let error = values.remove(0);
    if !error.is_null() {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| error.to_string());
        return Err(RealtimeError::Remote {
            event: event.into(),
            message,
        });
    }
    Ok(values)
}

fn spawn_event_bridge(
    transport: Arc<SocketIo09Session>,
    events: mpsc::Sender<RealtimeEvent>,
    public_id: Arc<RwLock<Option<String>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = transport.next_event().await {
            let (name, args) = event.into_parts();
            if name == "connectionAccepted"
                && let Some(id) = args.iter().rev().find_map(Value::as_str)
            {
                *public_id.write().unwrap() = Some(id.into());
            }
            if events.send(RealtimeEvent { name, args }).await.is_err() {
                return;
            }
        }
        let _ = events
            .send(RealtimeEvent {
                name: "disconnect".into(),
                args: Vec::new(),
            })
            .await;
    })
}

fn parse_join_project_response(args: &[Value]) -> Option<(Value, Option<String>)> {
    let values = args
        .iter()
        .flat_map(|value| {
            value
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![value.clone()])
        })
        .collect::<Vec<_>>();
    let objects = values
        .iter()
        .filter(|value| value.is_object())
        .collect::<Vec<_>>();
    let project = objects
        .iter()
        .filter_map(|value| value.get("project"))
        .chain(
            objects
                .iter()
                .filter_map(|value| value.pointer("/data/project")),
        )
        .chain(objects.iter().copied())
        .find(|candidate| candidate.get("rootFolder").is_some_and(Value::is_array))?
        .clone();
    let public_id = objects
        .iter()
        .find_map(|value| {
            value
                .get("publicId")
                .or_else(|| value.get("public_id"))
                .or_else(|| value.pointer("/data/publicId"))
                .and_then(Value::as_str)
        })
        .or_else(|| values.iter().find_map(Value::as_str))
        .map(str::to_owned);
    Some((project, public_id))
}

fn history_snapshot_has_tracked_deletes(snapshot: &Value) -> bool {
    snapshot
        .get("trackedChanges")
        .and_then(Value::as_array)
        .is_some_and(|changes| {
            changes.iter().any(|change| {
                change.pointer("/tracking/type").and_then(Value::as_str) == Some("delete")
            })
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_tungstenite::tokio::accept_async;
    use async_tungstenite::tungstenite::Message;
    use futures::StreamExt as _;
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

    async fn serve_handshake(listener: &TcpListener, session_id: &str) -> String {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut stream).await;
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
        request
    }

    #[test]
    fn parses_all_hosted_join_project_response_shapes() {
        let project = json!({ "rootFolder": [], "name": "Paper" });
        assert_eq!(
            parse_join_project_response(&[json!({
                "data": { "project": project, "publicId": "client-1" }
            })]),
            Some((project.clone(), Some("client-1".into())))
        );
        assert_eq!(
            parse_join_project_response(&[json!([project.clone(), "client-2"])]),
            Some((project, Some("client-2".into())))
        );
    }

    #[tokio::test]
    async fn event_stream_backpressures_without_dropping_events() {
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(RealtimeEvent {
                name: "first".into(),
                args: Vec::new(),
            })
            .await
            .unwrap();

        let second = sender.send(RealtimeEvent {
            name: "second".into(),
            args: Vec::new(),
        });
        tokio::pin!(second);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut second)
                .await
                .is_err()
        );

        let mut events = RealtimeEventStream::new(receiver);
        assert_eq!(events.recv().await.unwrap().name, "first");
        second.await.unwrap();
        assert_eq!(events.recv().await.unwrap().name, "second");
    }

    #[tokio::test]
    async fn retains_file_events_emitted_immediately_after_join_project() {
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

            let join = loop {
                let message = websocket.next().await.unwrap().unwrap();
                let Message::Text(payload) = message else {
                    continue;
                };
                let packet = decode_payload(&payload).unwrap().remove(0);
                if packet.event.as_deref() == Some("joinProject") {
                    break packet;
                }
            };
            let mut ack = Packet::new(PacketType::Ack);
            ack.ack_id = join.id;
            ack.args = vec![Value::Null, json!({ "rootFolder": [] })];
            websocket
                .send(Message::Text(encode_packet(&ack).unwrap().into()))
                .await
                .unwrap();
            // This is the exact hosted shape: parent folder ID followed by the new file entity.
            // Send it before `connect_and_join` returns to verify that bootstrap cannot lose it.
            websocket
                .send(Message::Text(
                    encode_packet(&Packet::event(
                        "reciveNewFile",
                        vec![
                            json!("root-folder"),
                            json!({ "_id": "figure-1", "name": "figure.png" }),
                            json!("upload"),
                        ],
                    ))
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();

            while let Some(message) = websocket.next().await {
                let Ok(Message::Text(payload)) = message else {
                    continue;
                };
                if decode_payload(&payload)
                    .unwrap()
                    .iter()
                    .any(|packet| packet.packet_type == PacketType::Disconnect)
                {
                    break;
                }
            }
        });

        let identity = Identity {
            cookies: "session=active".into(),
            csrf_token: "csrf".into(),
            user_id: "user-1".into(),
            user_email: "researcher@example.com".into(),
        };
        let session = OverleafRealtimeSession::connect_and_join_with_timeout(
            &url,
            &identity,
            "paper-1",
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let mut events = session.take_startup_events().unwrap();
        let event = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.name == "reciveNewFile" {
                    return event;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(event.args[0], json!("root-folder"));
        assert_eq!(event.args[1]["_id"], json!("figure-1"));
        session.disconnect();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn falls_back_to_v2_and_joins_a_sharejs_document() {
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server_listener = listener.clone();
        let server = tokio::spawn(async move {
            let v1_request = serve_handshake(&server_listener, "v1-session").await;
            assert!(!v1_request.contains("projectId="));
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
            loop {
                let message = websocket.next().await.unwrap().unwrap();
                let Message::Text(payload) = message else {
                    continue;
                };
                let packet = decode_payload(&payload).unwrap().remove(0);
                if packet.event.as_deref() == Some("joinProject") {
                    break;
                }
            }
            websocket
                .send(Message::Text(
                    encode_packet(&Packet::new(PacketType::Disconnect))
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();

            let v2_request = serve_handshake(&server_listener, "v2-session").await;
            assert!(v2_request.contains("projectId=paper-1"));
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
                    encode_packet(&Packet::event(
                        "reciveNewFile",
                        vec![
                            json!("root-folder"),
                            json!({ "_id": "figure-before-join", "name": "figure.png" }),
                        ],
                    ))
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            websocket
                .send(Message::Text(
                    encode_packet(&Packet::event(
                        "joinProjectResponse",
                        vec![json!({
                            "project": { "name": "Native Paper", "rootFolder": [] },
                            "publicId": "native-client"
                        })],
                    ))
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();

            loop {
                let message = websocket.next().await.unwrap().unwrap();
                let Message::Text(payload) = message else {
                    continue;
                };
                let packet = decode_payload(&payload).unwrap().remove(0);
                if packet.event.as_deref() != Some("joinDoc") {
                    continue;
                }
                assert_eq!(packet.args[0], json!("doc-1"));
                let mut ack = Packet::new(PacketType::Ack);
                ack.ack_id = packet.id;
                ack.args = vec![
                    Value::Null,
                    json!(["\\documentclass{article}", "Native realtime"]),
                    json!(7),
                    json!([]),
                    Value::Null,
                    json!("sharejs-text-ot"),
                ];
                websocket
                    .send(Message::Text(encode_packet(&ack).unwrap().into()))
                    .await
                    .unwrap();
                break;
            }
        });

        let identity = Identity {
            cookies: "session=active".into(),
            csrf_token: "csrf".into(),
            user_id: "user-1".into(),
            user_email: "researcher@example.com".into(),
        };
        let session = OverleafRealtimeSession::connect_and_join_with_timeout(
            &url,
            &identity,
            "paper-1",
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(session.scheme(), SocketScheme::V2);
        assert_eq!(session.public_id().as_deref(), Some("native-client"));
        assert_eq!(session.project()["name"], json!("Native Paper"));
        let mut events = session.take_startup_events().unwrap();
        let event = events.recv().await.unwrap();
        assert_eq!(event.name, "reciveNewFile");
        assert_eq!(event.args[1]["_id"], json!("figure-before-join"));
        let snapshot = session.join_document("doc-1", None, None).await.unwrap();
        assert_eq!(snapshot.version, 7);
        assert_eq!(snapshot.text, "\\documentclass{article}\nNative realtime");
        session.disconnect();
        server.await.unwrap();
    }
}
