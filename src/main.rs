use std::{
    collections::HashMap,
    env,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Extension, Json, Router,
    extract::{
        ConnectInfo, Path, Request, State, WebSocketUpgrade,
        ws::{CloseFrame, Message as WsMessage, WebSocket},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
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

mod wire;

const ROOM_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ROOMS: usize = 32;
const MAX_PEERS: usize = 8;
const MAX_MESSAGE_BYTES: usize = 256 * 1024;
const MAX_ROOM_UPDATE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ROOM_BLOCKS: usize = 50_000;
const MAX_QUEUE_BYTES: usize = MAX_ROOM_UPDATE_BYTES + 64 * 1024;
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
type Rooms = Arc<Mutex<HashMap<String, Arc<Mutex<Room>>>>>;
type ApiResult = Result<Response, StatusCode>;

#[derive(Clone)]
struct App {
    rooms: Rooms,
    origin: Option<String>,
    admissions: Arc<Mutex<HashMap<IpAddr, Admission>>>,
    connections: Arc<Semaphore>,
}

#[derive(Default)]
struct Admission {
    creations: Rate,
    upgrades: Rate,
}

struct Rate {
    since: Instant,
    messages: usize,
    bytes: usize,
}

impl Default for Rate {
    fn default() -> Self {
        Self {
            since: Instant::now(),
            messages: 0,
            bytes: 0,
        }
    }
}

impl Rate {
    fn allow(
        &mut self,
        bytes: usize,
        max_messages: usize,
        max_bytes: usize,
        window: Duration,
        now: Instant,
    ) -> bool {
        if now.saturating_duration_since(self.since) >= window {
            self.since = now;
            self.messages = 0;
            self.bytes = 0;
        }
        if self.messages >= max_messages || bytes > max_bytes.saturating_sub(self.bytes) {
            return false;
        }
        self.messages += 1;
        self.bytes += bytes;
        true
    }
}

struct QueuedMessage {
    message: WsMessage,
    _permit: OwnedSemaphorePermit,
}

struct Peer {
    tx: mpsc::Sender<QueuedMessage>,
    budget: Arc<Semaphore>,
    cancel: Arc<Notify>,
    client_id: Option<yrs::block::ClientID>,
    traffic: Rate,
}

impl Peer {
    fn new() -> (Self, mpsc::Receiver<QueuedMessage>) {
        let (tx, rx) = mpsc::channel(16);
        (
            Self {
                tx,
                budget: Arc::new(Semaphore::new(MAX_QUEUE_BYTES)),
                cancel: Arc::default(),
                client_id: None,
                traffic: Rate::default(),
            },
            rx,
        )
    }

    fn send(&self, message: WsMessage) -> Result<(), ()> {
        let bytes = match &message {
            WsMessage::Binary(data) => data.len(),
            _ => 0,
        };
        let result = self
            .budget
            .clone()
            .try_acquire_many_owned(bytes as u32)
            .map_err(|_| ())
            .and_then(|permit| {
                self.tx
                    .try_send(QueuedMessage {
                        message,
                        _permit: permit,
                    })
                    .map_err(|_| ())
            });
        if result.is_err() {
            self.cancel.notify_one();
        }
        result
    }
}

struct Room {
    host_secret: String,
    awareness: Awareness,
    peers: HashMap<Uuid, Peer>,
    idle_since: Option<Instant>,
    closed: bool,
    update_bytes: usize,
    update_blocks: usize,
    traffic: Rate,
}

impl Room {
    fn allow_traffic(&mut self, peer_id: Uuid, bytes: usize) -> Result<(), ()> {
        let now = Instant::now();
        let peer = self.peers.get_mut(&peer_id).ok_or(())?;
        if !peer
            .traffic
            .allow(bytes, 120, 1024 * 1024, Duration::from_secs(1), now)
            || !self
                .traffic
                .allow(bytes, 400, 2 * 1024 * 1024, Duration::from_secs(1), now)
        {
            return Err(());
        }
        Ok(())
    }

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
            update_bytes: 0,
            update_blocks: 0,
            traffic: Rate::default(),
        }
    }

    fn expired(&self, now: Instant) -> bool {
        self.peers.is_empty()
            && self
                .idle_since
                .is_some_and(|since| now.saturating_duration_since(since) >= ROOM_TTL)
    }

    fn broadcast(&self, data: Vec<u8>, except: Option<Uuid>) {
        let message = WsMessage::Binary(data.into());
        for (id, peer) in &self.peers {
            if Some(*id) != except {
                let _ = peer.send(message.clone());
            }
        }
    }

    fn close(&mut self) {
        self.closed = true;
        for peer in self.peers.values() {
            let _ = peer.send(WsMessage::Close(Some(CloseFrame {
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
    fn admit(&self, ip: IpAddr, creating: bool) -> Result<(), StatusCode> {
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let mut admissions = self.admissions.lock().unwrap();
        admissions.retain(|_, a| {
            now.duration_since(a.creations.since) < window
                || now.duration_since(a.upgrades.since) < window
        });
        if !admissions.contains_key(&ip) && admissions.len() >= 1024 {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        let admission = admissions.entry(ip).or_default();
        let (rate, maximum) = if creating {
            (&mut admission.creations, 10)
        } else {
            (&mut admission.upgrades, 30)
        };
        if rate.allow(0, maximum, 0, window, now) {
            Ok(())
        } else {
            Err(StatusCode::TOO_MANY_REQUESTS)
        }
    }

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

type Remote = Option<Extension<ConnectInfo<SocketAddr>>>;

fn remote_ip(remote: Remote) -> IpAddr {
    remote
        .map(|Extension(ConnectInfo(addr))| addr.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]))
}

async fn create_room(State(app): State<App>, remote: Remote, headers: HeaderMap) -> ApiResult {
    app.check_origin(&headers)?;
    app.admit(remote_ip(remote), true)?;
    let mut rooms = app.rooms.lock().unwrap();
    if rooms.len() >= MAX_ROOMS {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
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
    rooms.insert(id.clone(), Arc::new(Mutex::new(room)));
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
    remote: Remote,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult {
    app.check_origin(&headers)?;
    app.admit(remote_ip(remote), false)?;
    let room = app.lookup(&id)?;
    if room.lock().unwrap().peers.len() >= MAX_PEERS {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    let permit = app
        .connections
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    Ok(ws
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_QUEUE_BYTES)
        .on_upgrade(move |socket| connect(socket, room, permit))
        .into_response())
}

fn handle_message(room: &mut Room, peer_id: Uuid, data: &[u8]) -> Result<(), ()> {
    if room.closed {
        return Err(());
    }
    if data.len() > MAX_MESSAGE_BYTES {
        return Err(());
    }
    room.allow_traffic(peer_id, data.len())?;
    let cost = wire::validate(data)?;
    if cost.bytes > MAX_ROOM_UPDATE_BYTES - room.update_bytes
        || cost.blocks > MAX_ROOM_BLOCKS - room.update_blocks
    {
        return Err(());
    }
    // Count all accepted update bytes, including pending and deleted CRDT history.
    room.update_bytes += cost.bytes;
    room.update_blocks += cost.blocks;
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
                    peer.send(WsMessage::Binary(response.encode_v1().into()))?;
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
                // Awareness retains tombstones after disconnect. Bound those as well as live peers.
                if room.awareness.meta(client_id).is_none() && room.awareness.iter().count() >= 256
                {
                    return Err(());
                }
                if room
                    .awareness
                    .meta(client_id)
                    .is_some_and(|(clock, _)| clock >= 16 * 1024 * 1024)
                {
                    return Err(());
                }
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
                    peer.send(WsMessage::Binary(
                        Message::Awareness(update).encode_v1().into(),
                    ))?;
                }
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

async fn connect(mut socket: WebSocket, room: Arc<Mutex<Room>>, _permit: OwnedSemaphorePermit) {
    let id = Uuid::new_v4();
    let (peer, mut rx) = Peer::new();
    let cancel = peer.cancel.clone();
    let accepted = {
        let mut room = room.lock().unwrap();
        if room.closed || room.expired(Instant::now()) || room.peers.len() >= MAX_PEERS {
            false
        } else {
            let mut encoder = EncoderV1::new();
            if DefaultProtocol
                .start(&room.awareness, &mut encoder)
                .is_err()
            {
                return;
            }
            if peer
                .send(WsMessage::Binary(encoder.to_vec().into()))
                .is_err()
            {
                return;
            }
            room.idle_since = None;
            room.peers.insert(id, peer);
            true
        }
    };
    if !accepted {
        let _ = socket
            .send(WsMessage::Close(Some(CloseFrame {
                code: 4003,
                reason: "room_unavailable".into(),
            })))
            .await;
        return;
    }
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    let mut last_seen = Instant::now();
    loop {
        tokio::select! {
            _ = cancel.notified() => break,
            outgoing = rx.recv() => {
                let Some(outgoing) = outgoing else { break };
                let closing = matches!(outgoing.message, WsMessage::Close(_));
                if !send_socket(&mut socket, outgoing.message).await || closing { break; }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(WsMessage::Binary(data))) => {
                        last_seen = Instant::now();
                        let result = handle_message(&mut room.lock().unwrap(), id, &data);
                        if result.is_err() {
                            send_socket(&mut socket, WsMessage::Close(Some(CloseFrame { code: 4002, reason: "input_limit".into() }))).await;
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Pong(data))) => {
                        if room.lock().unwrap().allow_traffic(id, data.len()).is_err() { break; }
                        last_seen = Instant::now();
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        if room.lock().unwrap().allow_traffic(id, data.len()).is_err() { break; }
                        last_seen = Instant::now();
                        if !send_socket(&mut socket, WsMessage::Pong(data)).await { break; }
                    }
                    Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(WsMessage::Text(_))) => break,
                }
            }
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > Duration::from_secs(45) { break; }
                if !send_socket(&mut socket, WsMessage::Ping(Vec::new().into())).await { break; }
            }
        }
    }
    room.lock().unwrap().disconnect(id);
}

async fn send_socket(socket: &mut WebSocket, message: WsMessage) -> bool {
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, socket.send(message)).await,
        Ok(Ok(()))
    )
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; worker-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    response
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
        .layer(middleware::from_fn(security_headers))
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
        admissions: Arc::default(),
        connections: Arc::new(Semaphore::new(MAX_ROOMS * MAX_PEERS)),
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
    axum::serve(
        listener,
        router(app).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
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
            admissions: Arc::default(),
            connections: Arc::new(Semaphore::new(MAX_ROOMS * MAX_PEERS)),
        }
    }

    #[test]
    fn rate_limits_reset_at_boundary_and_isolate_ips() {
        let mut rate = Rate::default();
        let now = rate.since;
        let second = Duration::from_secs(1);
        assert!(rate.allow(10, 2, 20, second, now));
        assert!(rate.allow(10, 2, 20, second, now));
        assert!(!rate.allow(1, 2, 20, second, now));
        assert!(rate.allow(20, 2, 20, second, now + second));
        assert!(!rate.allow(1, 2, 20, second, now + second));
        let app = app();
        let first = IpAddr::from([127, 0, 0, 1]);
        for _ in 0..10 {
            assert!(app.admit(first, true).is_ok());
        }
        assert_eq!(app.admit(first, true), Err(StatusCode::TOO_MANY_REQUESTS));
        assert!(app.admit(IpAddr::from([127, 0, 0, 2]), true).is_ok());
        assert!(app.admit(first, false).is_ok());
    }

    #[tokio::test]
    async fn creation_cap_is_atomic_and_headers_cover_errors() {
        let app = app();
        for i in 0..MAX_ROOMS {
            app.rooms
                .lock()
                .unwrap()
                .insert(i.to_string(), Arc::new(Mutex::new(Room::new())));
        }
        let response = router(app.clone())
            .oneshot(
                Request::post("/api/rooms")
                    .header("origin", "http://localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(app.rooms.lock().unwrap().len(), MAX_ROOMS);
        assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "DENY");
        assert!(
            response.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
    }

    #[tokio::test]
    async fn queue_bounds_cancel_slow_peers_and_release_byte_budget() {
        let (peer, mut rx) = Peer::new();
        peer.send(WsMessage::Binary(vec![0; MAX_QUEUE_BYTES].into()))
            .unwrap();
        assert!(peer.send(WsMessage::Binary(vec![1].into())).is_err());
        tokio::time::timeout(Duration::from_millis(100), peer.cancel.notified())
            .await
            .unwrap();
        drop(rx.recv().await.unwrap());
        assert_eq!(peer.budget.available_permits(), MAX_QUEUE_BYTES);
        for _ in 0..16 {
            peer.send(WsMessage::Binary(vec![1].into())).unwrap();
        }
        assert!(peer.send(WsMessage::Binary(vec![1].into())).is_err());
    }

    #[test]
    fn update_budget_rejects_before_mutating_document() {
        use yrs::{ReadTxn, StateVector, Text};
        let doc = Doc::new();
        let text = doc.get_or_insert_text("code");
        let mut txn = doc.transact_mut();
        text.insert(&mut txn, 0, "must not be applied");
        let update = txn.encode_state_as_update_v1(&StateVector::default());
        let packet = Message::Sync(SyncMessage::Update(update)).encode_v1();
        let mut room = Room::new();
        let id = Uuid::new_v4();
        let (peer, _rx) = Peer::new();
        room.peers.insert(id, peer);
        room.update_bytes = MAX_ROOM_UPDATE_BYTES;
        let before = room
            .awareness
            .doc()
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        assert!(handle_message(&mut room, id, &packet).is_err());
        assert_eq!(
            before,
            room.awareness
                .doc()
                .transact()
                .encode_state_as_update_v1(&StateVector::default())
        );
    }

    #[test]
    fn expiration_boundary_and_connected_room() {
        let mut room = Room::new();
        let start = room.idle_since.unwrap();
        assert!(!room.expired(start + ROOM_TTL - Duration::from_nanos(1)));
        assert!(room.expired(start + ROOM_TTL));
        assert!(room.expired(start + ROOM_TTL + Duration::from_secs(1)));
        let (peer, _) = Peer::new();
        room.peers.insert(Uuid::new_v4(), peer);
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
            let (peer, _) = Peer::new();
            room.peers.insert(id, peer);
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
