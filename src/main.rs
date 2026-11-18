use std::{
    collections::HashMap,
    env,
    net::{IpAddr, Ipv6Addr, SocketAddr},
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
    Doc, Map, ReadTxn, StateVector, Transact, Update,
    encoding::read::Cursor,
    sync::{Awareness, DefaultProtocol, Message, MessageReader, Protocol, SyncMessage},
    updates::{
        decoder::{Decode, DecoderV1},
        encoder::{Encode, Encoder, EncoderV1},
    },
};

mod schema;
mod wire;

const ROOM_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const UNJOINED_ROOM_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_ROOMS: usize = 32;
const MAX_ROOMS_PER_SOURCE: usize = 8;
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
    base_path: String,
    trusted_proxy: Option<IpAddr>,
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
    sync_step1_seen: bool,
    awareness_query_seen: bool,
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
                sync_step1_seen: false,
                awareness_query_seen: false,
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
    source: IpAddr,
    has_joined: bool,
    host_secret: String,
    awareness: Awareness,
    peers: HashMap<Uuid, Peer>,
    idle_since: Option<Instant>,
    closed: bool,
    update_bytes: usize,
    update_blocks: usize,
    traffic: Rate,
    schema: schema::Schema,
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

    fn new(source: IpAddr) -> Self {
        let doc = Doc::new();
        doc.get_or_insert_text("code");
        let settings = doc.get_or_insert_map("settings");
        {
            let mut txn = doc.transact_mut();
            settings.insert(&mut txn, "language", "plaintext");
            settings.insert(&mut txn, "tabSize", 4);
        }
        let initial = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let schema = wire::validate(
            &Message::Sync(SyncMessage::Update(initial)).encode_v1(),
            &schema::Schema::default(),
        )
        .expect("the initial settings follow the document schema")
        .additions;
        Self {
            source,
            has_joined: false,
            host_secret: Uuid::new_v4().simple().to_string(),
            awareness: Awareness::new(doc),
            peers: HashMap::new(),
            idle_since: Some(Instant::now()),
            closed: false,
            update_bytes: 0,
            update_blocks: 0,
            traffic: Rate::default(),
            schema,
        }
    }

    fn expired(&self, now: Instant) -> bool {
        let ttl = if self.has_joined {
            ROOM_TTL
        } else {
            UNJOINED_ROOM_TTL
        };
        self.peers.is_empty()
            && self
                .idle_since
                .is_some_and(|since| now.saturating_duration_since(since) >= ttl)
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
    fn client_ip(&self, remote: Remote, headers: &HeaderMap) -> Result<IpAddr, StatusCode> {
        let peer = remote_ip(remote).to_canonical();
        if self.trusted_proxy != Some(peer) {
            return Ok(peer);
        }
        // A trusted proxy must overwrite this single-value header, never append to it.
        let mut values = headers.get_all("x-real-ip").iter();
        let value = values.next().ok_or(StatusCode::BAD_REQUEST)?;
        if values.next().is_some() {
            return Err(StatusCode::BAD_REQUEST);
        }
        value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<IpAddr>().ok())
            .map(|ip| ip.to_canonical())
            .ok_or(StatusCode::BAD_REQUEST)
    }

    fn admit(&self, ip: IpAddr, creating: bool) -> Result<(), StatusCode> {
        let ip = quota_key(ip);
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

fn quota_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V4(ip) => IpAddr::V4(ip),
        IpAddr::V6(ip) => {
            let mut prefix = ip.octets();
            prefix[8..].fill(0);
            IpAddr::V6(Ipv6Addr::from(prefix))
        }
    }
}

async fn create_room(State(app): State<App>, remote: Remote, headers: HeaderMap) -> ApiResult {
    app.check_origin(&headers)?;
    let source = quota_key(app.client_ip(remote, &headers)?);
    app.admit(source, true)?;
    let mut rooms = app.rooms.lock().unwrap();
    if rooms.len() >= MAX_ROOMS
        || rooms
            .values()
            .filter(|room| room.lock().unwrap().source == source)
            .count()
            >= MAX_ROOMS_PER_SOURCE
    {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    let id = Uuid::new_v4().simple().to_string();
    let room = Room::new(source);
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
        "mockpad_host={}; Path={}/api/rooms/{id}; HttpOnly; SameSite=Strict{secure}",
        room.host_secret, app.base_path
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
    app.admit(app.client_ip(remote, &headers)?, false)?;
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
    let cost = wire::validate(data, &room.schema)?;
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
                let peer = room.peers.get_mut(&peer_id).ok_or(())?;
                if peer.sync_step1_seen {
                    return Err(());
                }
                peer.sync_step1_seen = true;
                let response = DefaultProtocol
                    .handle_sync_step1(&mut room.awareness, vector)
                    .map_err(|_| ())?
                    .ok_or(())?;
                if let Some(peer) = room.peers.get(&peer_id) {
                    peer.send(WsMessage::Binary(response.encode_v1().into()))?;
                }
            }
            Message::Sync(SyncMessage::SyncStep2(update) | SyncMessage::Update(update)) => {
                let canonical = {
                    let update = Update::decode_v1(&update).map_err(|_| ())?;
                    let mut txn = room.awareness.doc().transact_mut();
                    txn.apply_update(update).map_err(|_| ())?;
                    txn.encode_update_v1()
                };
                // Relay only changes actually accepted by the document. Replaying
                // forged bytes for an already-known ID must not affect a new peer.
                if canonical != Update::EMPTY_V1 {
                    room.broadcast(
                        Message::Sync(SyncMessage::Update(canonical)).encode_v1(),
                        Some(peer_id),
                    );
                }
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
                let peer = room.peers.get_mut(&peer_id).ok_or(())?;
                if peer.awareness_query_seen {
                    return Err(());
                }
                peer.awareness_query_seen = true;
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
    room.schema.extend(cost.additions);
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
            room.has_joined = true;
            room.peers.insert(id, peer);
            true
        }
    };
    if !accepted {
        close_socket(&mut socket, 4003, "room_unavailable").await;
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
                if !send_socket(&mut socket, outgoing.message).await { break; }
                if closing { finish_close(&mut socket).await; break; }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(WsMessage::Binary(data))) => {
                        last_seen = Instant::now();
                        let result = handle_message(&mut room.lock().unwrap(), id, &data);
                        if result.is_err() {
                            close_socket(&mut socket, 4002, "input_limit").await;
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

async fn close_socket(socket: &mut WebSocket, code: u16, reason: &'static str) {
    if send_socket(
        socket,
        WsMessage::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })),
    )
    .await
    {
        finish_close(socket).await;
    }
}

async fn finish_close(socket: &mut WebSocket) {
    // Allow the close acknowledgement to arrive instead of resetting TCP with
    // unread in-flight frames. Bound both the time and the work after rejection.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        for _ in 0..8 {
            match socket.recv().await {
                Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            }
        }
    })
    .await;
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
    let prefix = app.base_path.clone();
    let routes = Router::new()
        .route_service("/", ServeFile::new("dist/index.html"))
        .route_service("/r/{id}", ServeFile::new("dist/index.html"))
        .route("/api/rooms", post(create_room))
        .route("/api/rooms/{id}", get(room_metadata))
        .route("/api/rooms/{id}/close", post(close_room))
        .route("/api/rooms/{id}/ws", get(upgrade))
        .fallback_service(ServeDir::new("dist"))
        .with_state(app);
    let routes = if prefix.is_empty() {
        routes
    } else {
        Router::new().nest(&format!("{prefix}/"), routes)
    };
    routes.layer(middleware::from_fn(security_headers))
}

fn base_path(value: &str) -> Result<String, &'static str> {
    let prefix = value.trim_end_matches('/');
    if !prefix.is_empty()
        && (!prefix.starts_with('/')
            || prefix.len() > 128
            || prefix.split('/').skip(1).any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            }))
    {
        return Err("MOCKPAD_BASE_PATH must be a path such as /mockpad");
    }
    Ok(prefix.to_owned())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let origin = env::var("MOCKPAD_PUBLIC_ORIGIN")
        .ok()
        .map(|s| s.trim_end_matches('/').to_owned());
    let trusted_proxy = env::var("MOCKPAD_TRUSTED_PROXY")
        .ok()
        .map(|value| value.parse::<IpAddr>().map(|ip| ip.to_canonical()))
        .transpose()?;
    if origin
        .as_deref()
        .is_some_and(|origin| origin.starts_with("https://"))
        && trusted_proxy.is_none()
    {
        return Err(
            "HTTPS公開時はMOCKPAD_TRUSTED_PROXYとプロキシによるX-Real-IPの上書きが必要です".into(),
        );
    }
    let app = App {
        rooms: Arc::default(),
        origin,
        base_path: base_path(&env::var("MOCKPAD_BASE_PATH").unwrap_or_default())?,
        trusted_proxy,
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
            base_path: String::new(),
            trusted_proxy: None,
            admissions: Arc::default(),
            connections: Arc::new(Semaphore::new(MAX_ROOMS * MAX_PEERS)),
        }
    }

    #[test]
    fn base_path_accepts_only_unambiguous_path_segments() {
        for (value, expected) in [
            ("", ""),
            ("/", ""),
            ("/mockpad/", "/mockpad"),
            ("/tools/mock-pad", "/tools/mock-pad"),
        ] {
            assert_eq!(base_path(value).unwrap(), expected);
        }
        for value in [
            "mockpad",
            "//mockpad",
            "/a//b",
            "/../mockpad",
            "/a?b",
            "/a%b",
            "/a\r\n",
        ] {
            assert!(base_path(value).is_err());
        }
        assert!(base_path(&format!("/{}", "x".repeat(128))).is_err());
    }

    #[tokio::test]
    async fn mounted_api_scopes_management_cookie_and_rejects_unmounted_routes() {
        let mut app = app();
        app.base_path = "/mockpad".into();
        let response = router(app.clone())
            .oneshot(
                Request::post("/mockpad/api/rooms")
                    .header("origin", "http://localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cookie.contains("; Path=/mockpad/api/rooms/"));
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let path = format!("/mockpad/api/rooms/{}", metadata["id"].as_str().unwrap());
        let response = router(app.clone())
            .oneshot(
                Request::get(path)
                    .header("cookie", cookie.split(';').next().unwrap())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["isHost"],
            true
        );
        let response = router(app)
            .oneshot(
                Request::post("/api/rooms")
                    .header("origin", "http://localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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

    #[test]
    fn only_a_configured_proxy_can_supply_one_client_ip() {
        let mut app = app();
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", HeaderValue::from_static("192.0.2.1"));
        assert_eq!(
            app.client_ip(None, &headers).unwrap(),
            IpAddr::from([127, 0, 0, 1])
        );
        app.trusted_proxy = Some(IpAddr::from([127, 0, 0, 1]));
        assert_eq!(
            app.client_ip(None, &headers).unwrap(),
            IpAddr::from([192, 0, 2, 1])
        );
        let remote = Some(Extension(ConnectInfo(SocketAddr::from((
            [192, 0, 2, 2],
            1234,
        )))));
        assert_eq!(
            app.client_ip(remote, &headers).unwrap(),
            IpAddr::from([192, 0, 2, 2])
        );
        headers.append("x-real-ip", HeaderValue::from_static("192.0.2.3"));
        assert_eq!(app.client_ip(None, &headers), Err(StatusCode::BAD_REQUEST));
        for value in ["not-an-ip", "192.0.2.1, 192.0.2.2", ""] {
            headers.insert("x-real-ip", HeaderValue::from_str(value).unwrap());
            assert_eq!(app.client_ip(None, &headers), Err(StatusCode::BAD_REQUEST));
        }
        headers.clear();
        assert_eq!(app.client_ip(None, &headers), Err(StatusCode::BAD_REQUEST));
        headers.insert("x-real-ip", HeaderValue::from_static("::ffff:192.0.2.1"));
        assert_eq!(
            app.client_ip(None, &headers).unwrap(),
            IpAddr::from([192, 0, 2, 1])
        );
    }

    #[tokio::test]
    async fn retained_room_quota_is_per_source_and_recovers_after_expiry() {
        async fn create(app: &App, ip: [u8; 4]) -> Response {
            router(app.clone())
                .oneshot(
                    Request::post("/api/rooms")
                        .extension(ConnectInfo(SocketAddr::from((ip, 1234))))
                        .header("origin", "http://localhost:3000")
                        .header("x-real-ip", "203.0.113.1")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        }
        let app = app();
        for _ in 0..MAX_ROOMS_PER_SOURCE {
            assert_eq!(
                create(&app, [127, 0, 0, 1]).await.status(),
                StatusCode::CREATED
            );
        }
        assert_eq!(
            create(&app, [127, 0, 0, 1]).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            create(&app, [127, 0, 0, 2]).await.status(),
            StatusCode::CREATED
        );
        let now = Instant::now();
        for room in app.rooms.lock().unwrap().values() {
            let mut room = room.lock().unwrap();
            if room.source == IpAddr::from([127, 0, 0, 1]) {
                room.idle_since = Some(now - UNJOINED_ROOM_TTL);
            }
        }
        app.clean(now);
        assert_eq!(app.rooms.lock().unwrap().len(), 1);
        assert_eq!(
            create(&app, [127, 0, 0, 1]).await.status(),
            StatusCode::CREATED
        );
    }

    #[test]
    fn never_joined_rooms_have_a_short_expiration_boundary() {
        let mut room = Room::new(IpAddr::from([127, 0, 0, 1]));
        let start = room.idle_since.unwrap();
        assert!(!room.expired(start + UNJOINED_ROOM_TTL - Duration::from_nanos(1)));
        assert!(room.expired(start + UNJOINED_ROOM_TTL));
        room.has_joined = true;
        assert!(!room.expired(start + UNJOINED_ROOM_TTL));
    }

    #[tokio::test]
    async fn ipv6_privacy_addresses_share_limits_without_expanding_proxy_trust() {
        let mut app = app();
        let network = u128::from("2001:db8:1:2::".parse::<Ipv6Addr>().unwrap());
        for suffix in 1..=MAX_ROOMS_PER_SOURCE + 1 {
            let ip = IpAddr::V6(Ipv6Addr::from(network + suffix as u128));
            let response = router(app.clone())
                .oneshot(
                    Request::post("/api/rooms")
                        .extension(ConnectInfo(SocketAddr::new(ip, 1234)))
                        .header("origin", "http://localhost:3000")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if suffix <= MAX_ROOMS_PER_SOURCE {
                    StatusCode::CREATED
                } else {
                    StatusCode::TOO_MANY_REQUESTS
                }
            );
        }
        assert_eq!(app.admissions.lock().unwrap().len(), 1);
        let another = IpAddr::V6(Ipv6Addr::from(network + 42));
        assert!(app.admit(another, true).is_ok()); // tenth request in the shared window
        assert_eq!(app.admit(another, true), Err(StatusCode::TOO_MANY_REQUESTS));
        assert!(app.admit("2001:db8:1:3::1".parse().unwrap(), true).is_ok());
        assert_eq!(
            quota_key("::ffff:192.0.2.1".parse().unwrap()),
            quota_key("192.0.2.1".parse().unwrap())
        );

        let proxy: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        app.trusted_proxy = Some(proxy);
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", HeaderValue::from_static("2001:db8:9:1::123"));
        let peer = Some(Extension(ConnectInfo(SocketAddr::new(another, 1234))));
        assert_eq!(app.client_ip(peer, &headers).unwrap(), another);
        let peer = Some(Extension(ConnectInfo(SocketAddr::new(proxy, 1234))));
        let effective = app.client_ip(peer, &headers).unwrap();
        assert_eq!(effective, "2001:db8:9:1::123".parse::<IpAddr>().unwrap());
        assert_eq!(
            quota_key(effective),
            "2001:db8:9:1::".parse::<IpAddr>().unwrap()
        );
    }

    #[tokio::test]
    async fn full_state_and_presence_queries_are_answered_only_once() {
        let mut room = Room::new(IpAddr::from([127, 0, 0, 1]));
        let id = Uuid::new_v4();
        let (peer, mut rx) = Peer::new();
        room.peers.insert(id, peer);
        let sync = Message::Sync(SyncMessage::SyncStep1(StateVector::default())).encode_v1();
        assert!(handle_message(&mut room, id, &sync).is_ok());
        drop(rx.recv().await.unwrap());
        assert!(handle_message(&mut room, id, &sync).is_err());
        assert!(rx.try_recv().is_err());
        let query = Message::AwarenessQuery.encode_v1();
        assert!(handle_message(&mut room, id, &query).is_ok());
        drop(rx.recv().await.unwrap());
        assert!(handle_message(&mut room, id, &query).is_err());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn replayed_item_ids_do_not_rebroadcast_conflicting_bytes() {
        use yrs::Text;
        let mut room = Room::new(IpAddr::from([127, 0, 0, 1]));
        let sender = Uuid::new_v4();
        let observer = Uuid::new_v4();
        let (peer, _sender_rx) = Peer::new();
        room.peers.insert(sender, peer);
        let (peer, mut rx) = Peer::new();
        room.peers.insert(observer, peer);
        let client = (room.awareness.doc().client_id().get() as u32).wrapping_add(1);
        for (index, content) in ["safe", "evil"].into_iter().enumerate() {
            let doc = Doc::with_client_id(client as u64);
            let text = doc.get_or_insert_text("code");
            let mut txn = doc.transact_mut();
            text.insert(&mut txn, 0, content);
            let update = txn.encode_state_as_update_v1(&StateVector::default());
            assert!(
                handle_message(
                    &mut room,
                    sender,
                    &Message::Sync(SyncMessage::Update(update)).encode_v1()
                )
                .is_ok()
            );
            if index == 0 {
                assert!(rx.try_recv().is_ok());
            } else {
                assert!(rx.try_recv().is_err());
            }
        }
        use yrs::GetString;
        assert_eq!(
            room.awareness
                .doc()
                .get_or_insert_text("code")
                .get_string(&room.awareness.doc().transact()),
            "safe"
        );
    }

    #[tokio::test]
    async fn creation_cap_is_atomic_and_headers_cover_errors() {
        let app = app();
        for i in 0..MAX_ROOMS {
            app.rooms.lock().unwrap().insert(
                i.to_string(),
                Arc::new(Mutex::new(Room::new(IpAddr::from([192, 0, 2, i as u8])))),
            );
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
        let mut room = Room::new(IpAddr::from([127, 0, 0, 1]));
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
        let mut room = Room::new(IpAddr::from([127, 0, 0, 1]));
        room.has_joined = true;
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
        let room = Arc::new(Mutex::new(Room::new(IpAddr::from([127, 0, 0, 1]))));
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
        let mut room = Room::new(IpAddr::from([127, 0, 0, 1]));
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
        let mut room = Room::new(IpAddr::from([127, 0, 0, 1]));
        room.has_joined = true;
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
        let room = Room::new(IpAddr::from([127, 0, 0, 1]));
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
