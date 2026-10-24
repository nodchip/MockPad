use std::{
    collections::HashMap,
    env,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{CloseFrame, Message as WsMessage, WebSocket},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use tokio::sync::mpsc;
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;
use yrs::{
    Doc, Map, Transact,
    encoding::read::Cursor,
    sync::{Awareness, DefaultProtocol, Message, MessageReader, Protocol, SyncMessage},
    updates::{
        decoder::DecoderV1,
        encoder::{Encode, Encoder, EncoderV1},
    },
};

const ROOM_TTL: Duration = Duration::from_secs(24 * 60 * 60);
type Rooms = Arc<Mutex<HashMap<String, Arc<Mutex<Room>>>>>;
type ApiResult = Result<Response, StatusCode>;

#[derive(Clone)]
struct App {
    rooms: Rooms,
    origin: Option<String>,
}

struct Peer {
    tx: mpsc::UnboundedSender<WsMessage>,
    client_id: Option<yrs::block::ClientID>,
}

struct Room {
    host_secret: String,
    awareness: Awareness,
    peers: HashMap<Uuid, Peer>,
    idle_since: Option<Instant>,
    closed: bool,
}

impl Room {
    fn new() -> Self {
        let doc = Doc::new();
        doc.get_or_insert_text("code");
        let settings = doc.get_or_insert_map("settings");
        {
            let mut txn = doc.transact_mut();
            settings.insert(&mut txn, "language", "plaintext");
            settings.insert(&mut txn, "tabSize", 4);
        }
        Self {
            host_secret: Uuid::new_v4().simple().to_string(),
            awareness: Awareness::new(doc),
            peers: HashMap::new(),
            idle_since: Some(Instant::now()),
            closed: false,
        }
    }

    fn expired(&self, now: Instant) -> bool {
        self.peers.is_empty()
            && self
                .idle_since
                .is_some_and(|since| now.saturating_duration_since(since) >= ROOM_TTL)
    }

    fn broadcast(&self, data: Vec<u8>, except: Option<Uuid>) {
        for (id, peer) in &self.peers {
            if Some(*id) != except {
                let _ = peer.tx.send(WsMessage::Binary(data.clone().into()));
            }
        }
    }

    fn close(&mut self) {
        self.closed = true;
        for peer in self.peers.values() {
            let _ = peer.tx.send(WsMessage::Close(Some(CloseFrame {
                code: 4000,
                reason: "room_closed".into(),
            })));
        }
        self.peers.clear();
    }

    fn disconnect(&mut self, id: Uuid) {
        if let Some(peer) = self.peers.remove(&id) {
            if let Some(client_id) = peer.client_id {
                self.awareness.remove_state(client_id);
                if let Ok(update) = self.awareness.update_with_clients([client_id]) {
                    self.broadcast(Message::Awareness(update).encode_v1(), None);
                }
            }
            if self.peers.is_empty() {
                self.idle_since = Some(Instant::now());
            }
        }
    }
}

impl App {
    fn lookup(&self, id: &str) -> Result<Arc<Mutex<Room>>, StatusCode> {
        let mut rooms = self.rooms.lock().unwrap();
        let room = rooms.get(id).cloned().ok_or(StatusCode::NOT_FOUND)?;
        let mut guard = room.lock().unwrap();
        if guard.closed || guard.expired(Instant::now()) {
            guard.close();
            rooms.remove(id);
            return Err(StatusCode::NOT_FOUND);
        }
        drop(guard);
        Ok(room)
    }

    fn clean(&self, now: Instant) {
        self.rooms.lock().unwrap().retain(|_, room| {
            let mut room = room.lock().unwrap();
            if room.expired(now) {
                room.close();
                false
            } else {
                !room.closed
            }
        });
    }

    fn check_origin(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        let expected = self.origin.clone().or_else(|| {
            headers
                .get(header::HOST)?
                .to_str()
                .ok()
                .map(|host| format!("http://{host}"))
        });
        let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
        if origin.is_some() && origin == expected.as_deref() {
            Ok(())
        } else {
            Err(StatusCode::FORBIDDEN)
        }
    }
}

fn is_host(headers: &HeaderMap, room: &Room) -> bool {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|cookies| {
            cookies.split(';').any(|cookie| {
                cookie.trim().strip_prefix("mockpad_host=") == Some(room.host_secret.as_str())
            })
        })
}

async fn create_room(State(app): State<App>, headers: HeaderMap) -> ApiResult {
    app.check_origin(&headers)?;
    let id = Uuid::new_v4().simple().to_string();
    let room = Room::new();
    let secure = if app
        .origin
        .as_deref()
        .is_some_and(|o| o.starts_with("https://"))
    {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "mockpad_host={}; Path=/api/rooms/{id}; HttpOnly; SameSite=Strict{secure}",
        room.host_secret
    );
    app.rooms
        .lock()
        .unwrap()
        .insert(id.clone(), Arc::new(Mutex::new(room)));
    let mut response = (StatusCode::CREATED, Json(json!({ "id": id }))).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    Ok(response)
}

async fn room_metadata(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    let room = app.lookup(&id)?;
    let room = room.lock().unwrap();
    if room.closed {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut response =
        Json(json!({ "id": id, "isHost": is_host(&headers, &room) })).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn close_room(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    app.check_origin(&headers)?;
    let mut rooms = app.rooms.lock().unwrap();
    let room = rooms.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    let mut room = room.lock().unwrap();
    if !is_host(&headers, &room) {
        return Err(StatusCode::FORBIDDEN);
    }
    room.close();
    drop(room);
    rooms.remove(&id);
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn upgrade(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult {
    app.check_origin(&headers)?;
    let room = app.lookup(&id)?;
    Ok(ws
        .max_message_size(1024 * 1024)
        .on_upgrade(move |socket| connect(socket, room))
        .into_response())
}

fn handle_message(room: &mut Room, peer_id: Uuid, data: &[u8]) -> Result<(), ()> {
    if room.closed {
        return Err(());
    }
    let mut decoder = DecoderV1::new(Cursor::new(data));
    for message in MessageReader::new(&mut decoder) {
        let message = message.map_err(|_| ())?;
        match message {
            Message::Sync(SyncMessage::SyncStep1(vector)) => {
                let response = DefaultProtocol
                    .handle_sync_step1(&mut room.awareness, vector)
                    .map_err(|_| ())?
                    .ok_or(())?;
                if let Some(peer) = room.peers.get(&peer_id) {
                    let _ = peer.tx.send(WsMessage::Binary(response.encode_v1().into()));
                }
            }
            Message::Sync(SyncMessage::SyncStep2(update) | SyncMessage::Update(update)) => {
                DefaultProtocol
                    .handle_message(
                        &mut room.awareness,
                        Message::Sync(SyncMessage::Update(update.clone())),
                    )
                    .map_err(|_| ())?;
                room.broadcast(
                    Message::Sync(SyncMessage::Update(update)).encode_v1(),
                    Some(peer_id),
                );
            }
            Message::Awareness(update) => {
                // Each socket owns only its own presence, including removal on disconnect.
                if update.clients.len() != 1 {
                    return Err(());
                }
                let client_id = *update.clients.keys().next().ok_or(())?;
                if room
                    .peers
                    .iter()
                    .any(|(id, p)| *id != peer_id && p.client_id == Some(client_id))
                {
                    return Err(());
                }
                let peer = room.peers.get_mut(&peer_id).ok_or(())?;
                if peer.client_id.is_some_and(|id| id != client_id) {
                    return Err(());
                }
                peer.client_id = Some(client_id);
                room.awareness
                    .apply_update(update.clone())
                    .map_err(|_| ())?;
                room.broadcast(Message::Awareness(update).encode_v1(), Some(peer_id));
            }
            Message::AwarenessQuery => {
                let update = room.awareness.update().map_err(|_| ())?;
                if let Some(peer) = room.peers.get(&peer_id) {
                    let _ = peer.tx.send(WsMessage::Binary(
                        Message::Awareness(update).encode_v1().into(),
                    ));
                }
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

async fn connect(mut socket: WebSocket, room: Arc<Mutex<Room>>) {
    let id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let accepted = {
        let mut room = room.lock().unwrap();
        if room.closed || room.expired(Instant::now()) {
            false
        } else {
            let mut encoder = EncoderV1::new();
            if DefaultProtocol
                .start(&room.awareness, &mut encoder)
                .is_err()
            {
                return;
            }
            let _ = tx.send(WsMessage::Binary(encoder.to_vec().into()));
            room.idle_since = None;
            room.peers.insert(
                id,
                Peer {
                    tx,
                    client_id: None,
                },
            );
            true
        }
    };
    if !accepted {
        let _ = socket
            .send(WsMessage::Close(Some(CloseFrame {
                code: 4000,
                reason: "room_closed".into(),
            })))
            .await;
        return;
    }
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    let mut last_seen = Instant::now();
    loop {
        tokio::select! {
            outgoing = rx.recv() => {
                let Some(message) = outgoing else { break };
                let closing = matches!(message, WsMessage::Close(_));
                if socket.send(message).await.is_err() || closing { break; }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(WsMessage::Binary(data))) => {
                        last_seen = Instant::now();
                        let result = handle_message(&mut room.lock().unwrap(), id, &data);
                        if result.is_err() { break; }
                    }
                    Some(Ok(WsMessage::Pong(_))) => last_seen = Instant::now(),
                    Some(Ok(WsMessage::Ping(data))) => {
                        last_seen = Instant::now();
                        if socket.send(WsMessage::Pong(data)).await.is_err() { break; }
                    }
                    Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > Duration::from_secs(45) { break; }
                if socket.send(WsMessage::Ping(Vec::new().into())).await.is_err() { break; }
            }
        }
    }
    room.lock().unwrap().disconnect(id);
}

fn router(app: App) -> Router {
    Router::new()
        .route("/api/rooms", post(create_room))
        .route("/api/rooms/{id}", get(room_metadata))
        .route("/api/rooms/{id}/close", post(close_room))
        .route("/api/rooms/{id}/ws", get(upgrade))
        .fallback_service(
            ServeDir::new("dist").not_found_service(ServeFile::new("dist/index.html")),
        )
        .with_state(app)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let origin = env::var("MOCKPAD_PUBLIC_ORIGIN")
        .ok()
        .map(|s| s.trim_end_matches('/').to_owned());
    let app = App {
        rooms: Arc::default(),
        origin,
    };
    let cleanup = app.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(60));
        loop {
            timer.tick().await;
            cleanup.clean(Instant::now());
        }
    });
    let addr = env::var("MOCKPAD_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("MockPad listening on http://{addr}");
    axum::serve(listener, router(app)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    fn app() -> App {
        App {
            rooms: Arc::default(),
            origin: Some("http://localhost:3000".into()),
        }
    }

    #[test]
    fn expiration_boundary_and_connected_room() {
        let mut room = Room::new();
        let start = room.idle_since.unwrap();
        assert!(!room.expired(start + ROOM_TTL - Duration::from_nanos(1)));
        assert!(room.expired(start + ROOM_TTL));
        assert!(room.expired(start + ROOM_TTL + Duration::from_secs(1)));
        let (tx, _) = mpsc::unbounded_channel();
        room.peers.insert(
            Uuid::new_v4(),
            Peer {
                tx,
                client_id: None,
            },
        );
        assert!(!room.expired(start + ROOM_TTL));
    }

    #[test]
    fn cleanup_closes_old_handles_and_does_not_restore_rooms() {
        let app = app();
        let room = Arc::new(Mutex::new(Room::new()));
        let start = room.lock().unwrap().idle_since.unwrap();
        app.rooms
            .lock()
            .unwrap()
            .insert("test".into(), room.clone());
        app.clean(start + ROOM_TTL);
        assert!(app.lookup("test").is_err());
        assert!(room.lock().unwrap().closed);
        assert!(handle_message(&mut room.lock().unwrap(), Uuid::new_v4(), &[0, 0, 1, 0]).is_err());
        assert!(self::app().lookup("test").is_err());
    }

    #[tokio::test]
    async fn admission_rejects_expiration_before_background_cleanup() {
        let app = app();
        let mut room = Room::new();
        room.idle_since = Some(Instant::now() - ROOM_TTL);
        let room = Arc::new(Mutex::new(room));
        app.rooms
            .lock()
            .unwrap()
            .insert("expired".into(), room.clone());
        let response = router(app.clone())
            .oneshot(
                Request::get("/api/rooms/expired")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(room.lock().unwrap().closed);
        assert!(app.rooms.lock().unwrap().is_empty());
    }

    #[test]
    fn only_last_disconnect_starts_a_new_idle_period() {
        let mut room = Room::new();
        room.idle_since = None;
        let first = Uuid::new_v4();
        let last = Uuid::new_v4();
        for id in [first, last] {
            let (tx, _) = mpsc::unbounded_channel();
            room.peers.insert(
                id,
                Peer {
                    tx,
                    client_id: None,
                },
            );
        }
        room.disconnect(first);
        assert!(room.idle_since.is_none());
        let before = Instant::now();
        room.disconnect(last);
        assert!(room.idle_since.unwrap() >= before);
        assert!(!room.expired(Instant::now()));
        let since = room.idle_since;
        room.disconnect(last);
        assert_eq!(room.idle_since, since);
    }

    #[tokio::test]
    async fn only_host_can_close_and_origin_is_required() {
        let app = app();
        let room = Room::new();
        let secret = room.host_secret.clone();
        app.rooms
            .lock()
            .unwrap()
            .insert("test".into(), Arc::new(Mutex::new(room)));
        let service = router(app.clone());
        for cookie in ["", "mockpad_host=wrong"] {
            let response = service
                .clone()
                .oneshot(
                    Request::post("/api/rooms/test/close")
                        .header("origin", "http://localhost:3000")
                        .header("cookie", cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(app.lookup("test").is_ok());
        }
        let response = service
            .clone()
            .oneshot(
                Request::post("/api/rooms/test/close")
                    .header("origin", "https://other.example")
                    .header("cookie", format!("mockpad_host={secret}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = service
            .oneshot(
                Request::post("/api/rooms/test/close")
                    .header("origin", "http://localhost:3000")
                    .header("cookie", format!("mockpad_host={secret}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(app.lookup("test").is_err());
    }
}
