use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use futures::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};
use tracing::{info, warn};
use uuid::Uuid;

const ADMIN_HEADER_TOKEN: &str = "x-admin-token";
const ADMIN_SESSION_HOURS: i64 = 24;

#[derive(Clone)]
struct AppState {
    rooms: Arc<RwLock<HashMap<String, Room>>>,
    admin_sessions: Arc<RwLock<HashMap<String, AdminSession>>>,
    room_channels: Arc<RwLock<HashMap<String, broadcast::Sender<String>>>>,
    room_ready: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    admin_password: Arc<String>,
}

#[derive(Debug, Clone)]
struct AdminSession {
    username: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Room {
    id: String,
    name: String,
    owner: String,
    admins: Vec<String>,
    max_players: usize,
    players: Vec<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct AdminLoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct AdminLoginResponse {
    token: String,
    role: &'static str,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct AdminVerifyResponse {
    valid: bool,
    username: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct AdminLogoutResponse {
    ok: bool,
}

#[derive(Debug, Deserialize)]
struct CreateRoomRequest {
    name: String,
    owner: String,
    max_players: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct PlayerActionRequest {
    player: String,
}

#[derive(Debug, Deserialize)]
struct KickPlayerRequest {
    requester: Option<String>,
    target: String,
}

#[derive(Debug, Serialize)]
struct ApiError {
    message: String,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    timestamp: DateTime<Utc>,
    room_count: usize,
}

#[derive(Debug, Serialize)]
struct StatsResponse {
    timestamp: DateTime<Utc>,
    room_count: usize,
    connected_players: usize,
    active_admin_sessions: usize,
}

#[derive(Debug, Serialize)]
struct WsEnvelope<'a> {
    event: &'a str,
    room_id: &'a str,
    player: &'a str,
    payload: &'a str,
    ts: DateTime<Utc>,
}

fn create_state(admin_password: String) -> AppState {
    AppState {
        rooms: Arc::new(RwLock::new(HashMap::new())),
        admin_sessions: Arc::new(RwLock::new(HashMap::new())),
        room_channels: Arc::new(RwLock::new(HashMap::new())),
        room_ready: Arc::new(RwLock::new(HashMap::new())),
        admin_password: Arc::new(admin_password),
    }
}

fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/stats", get(get_stats))
        .route("/admin/login", post(admin_login))
        .route("/admin/verify", get(admin_verify))
        .route("/admin/logout", post(admin_logout))
        .route("/rooms", get(list_rooms).post(create_room))
        .route("/rooms/:room_id", get(get_room).delete(delete_room))
        .route("/rooms/:room_id/join", post(join_room))
        .route("/rooms/:room_id/leave", post(leave_room))
        .route("/rooms/:room_id/kick", post(kick_player))
        .route("/ws/:room_id/:player", get(ws_room))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "jboy_server=info,tower_http=info,axum=info".to_string()),
        )
        .init();

    let admin_password =
        std::env::var("JBOY_ADMIN_PASSWORD").unwrap_or_else(|_| "admin123".to_string());
    let bind = std::env::var("JBOY_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

    let state = create_state(admin_password);
    let app = create_router(state);

    let addr: SocketAddr = bind
        .parse()
        .expect("JBOY_BIND invalid. Example: 0.0.0.0:8080");

    info!("JBoy server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind TCP listener");

    axum::serve(listener, app)
        .await
        .expect("Axum server exited unexpectedly");
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let room_count = state.rooms.read().await.len();
    Json(HealthResponse {
        status: "ok",
        timestamp: Utc::now(),
        room_count,
    })
}

async fn get_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<StatsResponse>, (StatusCode, Json<ApiError>)> {
    ensure_admin(&state, &headers).await?;

    let rooms = state.rooms.read().await;
    let room_count = rooms.len();
    let connected_players: usize = rooms.values().map(|r| r.players.len()).sum();
    drop(rooms);

    let active_admin_sessions = count_active_admin_sessions(&state).await;

    Ok(Json(StatsResponse {
        timestamp: Utc::now(),
        room_count,
        connected_players,
        active_admin_sessions,
    }))
}

async fn admin_login(
    State(state): State<AppState>,
    Json(req): Json<AdminLoginRequest>,
) -> Result<Json<AdminLoginResponse>, (StatusCode, Json<ApiError>)> {
    let name_ok = req.username.trim() == "admin";
    let pass_ok = req.password == *state.admin_password;

    if !name_ok || !pass_ok {
        return Err(error_json(
            StatusCode::UNAUTHORIZED,
            "管理员账号或密码错误",
        ));
    }

    let token = Uuid::new_v4().to_string();
    let now = Utc::now();
    let expires_at = now + Duration::hours(ADMIN_SESSION_HOURS);
    let session = AdminSession {
        username: req.username.trim().to_string(),
        created_at: now,
        expires_at,
    };

    state
        .admin_sessions
        .write()
        .await
        .insert(token.clone(), session);
    info!("Admin login success");

    Ok(Json(AdminLoginResponse {
        token,
        role: "admin",
        expires_at,
    }))
}

async fn admin_verify(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AdminVerifyResponse>, (StatusCode, Json<ApiError>)> {
    let session = ensure_admin(&state, &headers).await?;
    Ok(Json(AdminVerifyResponse {
        valid: true,
        username: session.username,
        created_at: session.created_at,
        expires_at: session.expires_at,
    }))
}

async fn admin_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AdminLogoutResponse>, (StatusCode, Json<ApiError>)> {
    let token = extract_admin_token(&headers)
        .ok_or_else(|| error_json(StatusCode::UNAUTHORIZED, "缺少管理员令牌"))?;

    let removed = state.admin_sessions.write().await.remove(&token).is_some();
    if !removed {
        return Err(error_json(StatusCode::FORBIDDEN, "管理员令牌无效"));
    }

    Ok(Json(AdminLogoutResponse { ok: true }))
}

async fn list_rooms(State(state): State<AppState>) -> Json<Vec<Room>> {
    let mut rooms: Vec<Room> = state.rooms.read().await.values().cloned().collect();
    rooms.sort_by_key(|r| r.created_at);
    Json(rooms)
}

async fn get_room(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
) -> Result<Json<Room>, (StatusCode, Json<ApiError>)> {
    let rooms = state.rooms.read().await;
    match rooms.get(&room_id) {
        Some(room) => Ok(Json(room.clone())),
        None => Err(error_json(StatusCode::NOT_FOUND, "房间不存在")),
    }
}

async fn create_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateRoomRequest>,
) -> Result<(StatusCode, Json<Room>), (StatusCode, Json<ApiError>)> {
    ensure_admin(&state, &headers).await?;

    let room_id = Uuid::new_v4().to_string();
    let max_players = req.max_players.unwrap_or(2).clamp(2, 8);
    let owner = req.owner.trim().to_string();

    if req.name.trim().is_empty() {
        return Err(error_json(StatusCode::BAD_REQUEST, "房间名不能为空"));
    }
    if owner.is_empty() {
        return Err(error_json(StatusCode::BAD_REQUEST, "房主不能为空"));
    }

    let room = Room {
        id: room_id.clone(),
        name: req.name.trim().to_string(),
        owner: owner.clone(),
        admins: vec![owner.clone()],
        max_players,
        players: vec![owner],
        created_at: Utc::now(),
    };

    state.rooms.write().await.insert(room_id.clone(), room.clone());
    let (tx, _) = broadcast::channel(128);
    state.room_channels.write().await.insert(room_id.clone(), tx);
    state
        .room_ready
        .write()
        .await
        .insert(room_id.clone(), HashSet::new());

    info!("Room created: {} ({})", room.name, room.id);
    Ok((StatusCode::CREATED, Json(room)))
}

async fn delete_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    ensure_admin(&state, &headers).await?;

    let removed = state.rooms.write().await.remove(&room_id).is_some();
    state.room_channels.write().await.remove(&room_id);
    state.room_ready.write().await.remove(&room_id);

    if removed {
        info!("Room deleted: {}", room_id);
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(error_json(StatusCode::NOT_FOUND, "房间不存在"))
    }
}

async fn join_room(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    Json(req): Json<PlayerActionRequest>,
) -> Result<Json<Room>, (StatusCode, Json<ApiError>)> {
    let player = req.player.trim().to_string();
    if player.is_empty() {
        return Err(error_json(StatusCode::BAD_REQUEST, "玩家名不能为空"));
    }

    let mut rooms = state.rooms.write().await;
    let room = rooms
        .get_mut(&room_id)
        .ok_or_else(|| error_json(StatusCode::NOT_FOUND, "房间不存在"))?;

    if !room.players.contains(&player) && room.players.len() >= room.max_players {
        return Err(error_json(StatusCode::CONFLICT, "房间已满"));
    }

    if !room.players.contains(&player) {
        room.players.push(player.clone());
    }

    if room.owner.is_empty() {
        room.owner = player.clone();
    }
    if room.admins.is_empty() {
        room.admins.push(room.owner.clone());
    }

    info!("Player joined room {}: {}", room_id, player);
    Ok(Json(room.clone()))
}

async fn leave_room(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    Json(req): Json<PlayerActionRequest>,
) -> Result<Json<Room>, (StatusCode, Json<ApiError>)> {
    let player = req.player.trim().to_string();
    if player.is_empty() {
        return Err(error_json(StatusCode::BAD_REQUEST, "玩家名不能为空"));
    }

    if let Some(ready_set) = state.room_ready.write().await.get_mut(&room_id) {
        ready_set.remove(&player);
    }

    let (room_snapshot, room_deleted) = {
        let mut rooms = state.rooms.write().await;
        let room = rooms
            .get_mut(&room_id)
            .ok_or_else(|| error_json(StatusCode::NOT_FOUND, "房间不存在"))?;

        room.players.retain(|p| p != &player);
        room.admins.retain(|a| a != &player);

        if room.players.is_empty() {
            room.owner.clear();
            room.admins.clear();
        } else if room.owner == player {
            room.owner = room.players[0].clone();
            if !room.admins.contains(&room.owner) {
                room.admins.push(room.owner.clone());
            }
        }

        if room.admins.is_empty() && !room.owner.is_empty() {
            room.admins.push(room.owner.clone());
        }

        let snapshot = room.clone();
        let should_delete = room.players.is_empty();
        if should_delete {
            rooms.remove(&room_id);
        }
        (snapshot, should_delete)
    };

    if room_deleted {
        state.room_channels.write().await.remove(&room_id);
        state.room_ready.write().await.remove(&room_id);
        info!("Room auto-deleted after leave: {}", room_id);
    }

    info!("Player left room {}: {}", room_id, player);
    Ok(Json(room_snapshot))
}

async fn kick_player(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    Json(req): Json<KickPlayerRequest>,
) -> Result<Json<Room>, (StatusCode, Json<ApiError>)> {
    let target = req.target.trim().to_string();
    if target.is_empty() {
        return Err(error_json(StatusCode::BAD_REQUEST, "目标玩家不能为空"));
    }
    let requester = req.requester.unwrap_or_default().trim().to_string();

    let room_snapshot = {
        let rooms = state.rooms.read().await;
        rooms
            .get(&room_id)
            .cloned()
            .ok_or_else(|| error_json(StatusCode::NOT_FOUND, "房间不存在"))?
    };

    ensure_room_admin_or_platform_admin(&state, &headers, &room_snapshot, &requester).await?;

    if let Some(ready_set) = state.room_ready.write().await.get_mut(&room_id) {
        ready_set.remove(&target);
    }

    let (room_snapshot, room_deleted) = {
        let mut rooms = state.rooms.write().await;
        let room = rooms
            .get_mut(&room_id)
            .ok_or_else(|| error_json(StatusCode::NOT_FOUND, "房间不存在"))?;

        if !room.players.contains(&target) {
            return Err(error_json(StatusCode::NOT_FOUND, "目标玩家不在房间中"));
        }

        room.players.retain(|p| p != &target);
        room.admins.retain(|a| a != &target);

        if room.players.is_empty() {
            room.owner.clear();
            room.admins.clear();
        } else if room.owner == target {
            room.owner = room.players[0].clone();
            if !room.admins.contains(&room.owner) {
                room.admins.push(room.owner.clone());
            }
        }

        if room.admins.is_empty() && !room.owner.is_empty() {
            room.admins.push(room.owner.clone());
        }

        let snapshot = room.clone();
        let should_delete = room.players.is_empty();
        if should_delete {
            rooms.remove(&room_id);
        }
        (snapshot, should_delete)
    };

    if room_deleted {
        state.room_channels.write().await.remove(&room_id);
        state.room_ready.write().await.remove(&room_id);
        info!("Room auto-deleted after kick: {}", room_id);
    }

    info!("Player kicked room {}: {}", room_id, target);
    Ok(Json(room_snapshot))
}

async fn ws_room(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path((room_id, player)): Path<(String, String)>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    if player.trim().is_empty() {
        return Err(error_json(StatusCode::BAD_REQUEST, "玩家名不能为空"));
    }

    let mut auto_created = false;
    {
        let mut rooms = state.rooms.write().await;
        if !rooms.contains_key(&room_id) {
            let owner = player.trim().to_string();
            let room = Room {
                id: room_id.clone(),
                name: format!("Room {}", room_id),
                owner: owner.clone(),
                admins: vec![owner.clone()],
                max_players: 4,
                players: vec![owner],
                created_at: Utc::now(),
            };
            rooms.insert(room_id.clone(), room);
            auto_created = true;
        }
    }
    if auto_created {
        let mut channels = state.room_channels.write().await;
        channels.entry(room_id.clone()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(128);
            tx
        });
        let mut ready = state.room_ready.write().await;
        ready.entry(room_id.clone()).or_insert_with(HashSet::new);
        info!("Auto-created room via websocket: {}", room_id);
    }

    Ok(ws.on_upgrade(move |socket| handle_ws(state, room_id, player, socket)))
}

async fn handle_ws(state: AppState, room_id: String, player: String, socket: WebSocket) {
    let tx = {
        let mut channels = state.room_channels.write().await;
        if let Some(sender) = channels.get(&room_id) {
            sender.clone()
        } else {
            let (sender, _) = broadcast::channel(128);
            channels.insert(room_id.clone(), sender.clone());
            sender
        }
    };

    {
        let mut rooms = state.rooms.write().await;
        if let Some(room) = rooms.get_mut(&room_id) {
            if !room.players.contains(&player) && room.players.len() < room.max_players {
                room.players.push(player.clone());
            }
            if room.owner.is_empty() {
                room.owner = player.clone();
            }
            if room.admins.is_empty() {
                room.admins.push(room.owner.clone());
            }
        }
    }
    {
        let mut ready = state.room_ready.write().await;
        ready.entry(room_id.clone()).or_insert_with(HashSet::new);
    }

    let join_msg = serde_json::to_string(&WsEnvelope {
        event: "join",
        room_id: &room_id,
        player: &player,
        payload: "connected",
        ts: Utc::now(),
    })
    .unwrap_or_else(|_| "{\"event\":\"join\"}".to_string());
    let _ = tx.send(join_msg);
    if let Some(sync) = build_link_sync_payload(&state, &room_id).await {
        let sync_msg = serde_json::to_string(&WsEnvelope {
            event: "protocol",
            room_id: &room_id,
            player: &player,
            payload: &sync,
            ts: Utc::now(),
        })
        .unwrap_or_else(|_| sync);
        let _ = tx.send(sync_msg);
    }

    let (mut sender, mut receiver) = socket.split();
    let mut rx = tx.subscribe();

    let player_send = player.clone();
    let room_send = room_id.clone();
    let tx_send = tx.clone();
    let state_recv = state.clone();

    let send_task = tokio::spawn(async move {
        while let Ok(payload) = rx.recv().await {
            if sender.send(Message::Text(payload)).await.is_err() {
                break;
            }
        }
    });

    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(text) => {
                    let parsed = serde_json::from_str::<serde_json::Value>(&text).ok();
                    let kind = parsed
                        .as_ref()
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    match kind {
                        "hello" => {
                            if let Some(sync) = build_link_sync_payload(&state_recv, &room_send).await {
                                let packet = serde_json::to_string(&WsEnvelope {
                                    event: "protocol",
                                    room_id: &room_send,
                                    player: &player_send,
                                    payload: &sync,
                                    ts: Utc::now(),
                                })
                                .unwrap_or_else(|_| sync);
                                let _ = tx_send.send(packet);
                            }
                        }
                        "ready" => {
                            let is_ready = parsed
                                .as_ref()
                                .and_then(|v| v.get("ready"))
                                .and_then(|v| v.as_bool())
                                .unwrap_or(true);

                            {
                                let mut ready = state_recv.room_ready.write().await;
                                let room_ready = ready.entry(room_send.clone()).or_insert_with(HashSet::new);
                                if is_ready {
                                    room_ready.insert(player_send.clone());
                                } else {
                                    room_ready.remove(&player_send);
                                }
                            }

                            if let Some(sync) = build_link_sync_payload(&state_recv, &room_send).await {
                                let packet = serde_json::to_string(&WsEnvelope {
                                    event: "protocol",
                                    room_id: &room_send,
                                    player: &player_send,
                                    payload: &sync,
                                    ts: Utc::now(),
                                })
                                .unwrap_or_else(|_| sync);
                                let _ = tx_send.send(packet);
                            }
                        }
                        "ping" => {
                            let pong = serde_json::json!({
                                "type": "pong",
                                "room": &room_send,
                                "player": &player_send,
                                "ts": Utc::now().timestamp_millis()
                            })
                            .to_string();
                            let packet = serde_json::to_string(&WsEnvelope {
                                event: "protocol",
                                room_id: &room_send,
                                player: &player_send,
                                payload: &pong,
                                ts: Utc::now(),
                            })
                            .unwrap_or_else(|_| pong);
                            let _ = tx_send.send(packet);
                        }
                        _ => {
                            let packet = serde_json::to_string(&WsEnvelope {
                                event: "message",
                                room_id: &room_send,
                                player: &player_send,
                                payload: &text,
                                ts: Utc::now(),
                            })
                            .unwrap_or_else(|_| text.to_string());
                            let _ = tx_send.send(packet);
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    let leave_msg = serde_json::to_string(&WsEnvelope {
        event: "leave",
        room_id: &room_id,
        player: &player,
        payload: "disconnected",
        ts: Utc::now(),
    })
    .unwrap_or_else(|_| "{\"event\":\"leave\"}".to_string());
    let _ = tx.send(leave_msg);

    {
        let mut ready = state.room_ready.write().await;
        if let Some(room_ready) = ready.get_mut(&room_id) {
            room_ready.remove(&player);
        }
    }

    let room_deleted = {
        let mut rooms = state.rooms.write().await;
        let mut should_delete = false;
        if let Some(room) = rooms.get_mut(&room_id) {
            room.players.retain(|p| p != &player);
            room.admins.retain(|a| a != &player);
            if room.players.is_empty() {
                room.owner.clear();
                room.admins.clear();
                should_delete = true;
            } else if room.owner == player {
                room.owner = room.players[0].clone();
                if !room.admins.contains(&room.owner) {
                    room.admins.push(room.owner.clone());
                }
            }

            if room.admins.is_empty() && !room.owner.is_empty() {
                room.admins.push(room.owner.clone());
            }

            if should_delete {
                rooms.remove(&room_id);
            }
        }
        should_delete
    };

    if room_deleted {
        let room_closed = serde_json::to_string(&WsEnvelope {
            event: "room_closed",
            room_id: &room_id,
            player: &player,
            payload: "empty",
            ts: Utc::now(),
        })
        .unwrap_or_else(|_| "{\"event\":\"room_closed\"}".to_string());
        let _ = tx.send(room_closed);

        state.room_channels.write().await.remove(&room_id);
        state.room_ready.write().await.remove(&room_id);
        info!("Room auto-deleted after websocket disconnect: {}", room_id);
    } else if let Some(sync) = build_link_sync_payload(&state, &room_id).await {
        let sync_msg = serde_json::to_string(&WsEnvelope {
            event: "protocol",
            room_id: &room_id,
            player: &player,
            payload: &sync,
            ts: Utc::now(),
        })
        .unwrap_or_else(|_| sync);
        let _ = tx.send(sync_msg);
    }

    info!("WebSocket disconnected room={} player={}", room_id, player);
}

async fn build_link_sync_payload(state: &AppState, room_id: &str) -> Option<String> {
    let room = {
        let rooms = state.rooms.read().await;
        rooms.get(room_id).cloned()
    }?;

    let ready_players = {
        let ready = state.room_ready.read().await;
        ready
            .get(room_id)
            .map(|set| set.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };

    let can_start = room.players.len() >= 2 && ready_players.len() >= 2;
    Some(
        serde_json::json!({
            "type": "link_sync",
            "protocol": "jboy-link-1",
            "room": room.id,
            "host": room.owner,
            "players": room.players,
            "readyPlayers": ready_players,
            "canStart": can_start
        })
        .to_string(),
    )
}

async fn ensure_admin(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AdminSession, (StatusCode, Json<ApiError>)> {
    let token = extract_admin_token(headers)
        .ok_or_else(|| error_json(StatusCode::UNAUTHORIZED, "缺少管理员令牌"))?;
    validate_admin_token(state, &token)
        .await
        .ok_or_else(|| error_json(StatusCode::FORBIDDEN, "管理员令牌无效或已过期"))
}

async fn ensure_room_admin_or_platform_admin(
    state: &AppState,
    headers: &HeaderMap,
    room: &Room,
    requester: &str,
) -> Result<(), (StatusCode, Json<ApiError>)> {
    let requester_is_room_admin = !requester.is_empty() && room.admins.iter().any(|a| a == requester);
    if requester_is_room_admin {
        return Ok(());
    }

    let token = extract_admin_token(headers).unwrap_or_default();
    if !token.is_empty() && validate_admin_token(state, &token).await.is_some() {
        return Ok(());
    }

    Err(error_json(StatusCode::FORBIDDEN, "需要房间管理员或平台管理员权限"))
}

fn extract_admin_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(ADMIN_HEADER_TOKEN)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

async fn validate_admin_token(state: &AppState, token: &str) -> Option<AdminSession> {
    let now = Utc::now();

    {
        let sessions = state.admin_sessions.read().await;
        if let Some(session) = sessions.get(token) {
            if session.expires_at > now {
                return Some(session.clone());
            }
        } else {
            return None;
        }
    }

    warn!("Admin token expired, removing");
    state.admin_sessions.write().await.remove(token);
    None
}

async fn count_active_admin_sessions(state: &AppState) -> usize {
    let now = Utc::now();
    let mut sessions = state.admin_sessions.write().await;
    sessions.retain(|_, session| session.expires_at > now);
    sessions.len()
}

fn error_json(status: StatusCode, msg: &str) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            message: msg.to_string(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    fn test_app() -> Router {
        create_router(create_state("secret".to_string()))
    }

    async fn parse_json(resp: axum::response::Response) -> Value {
        let body = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body failed");
        serde_json::from_slice(&body).expect("invalid json body")
    }

    async fn post_json(
        app: &Router,
        uri: &str,
        body: Value,
        admin_token: Option<&str>,
    ) -> axum::response::Response {
        let payload = serde_json::to_vec(&body).expect("serialize body failed");
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");

        if let Some(token) = admin_token {
            builder = builder.header(ADMIN_HEADER_TOKEN, token);
        }

        let req = builder.body(Body::from(payload)).expect("build request failed");
        app.clone().oneshot(req).await.expect("request failed")
    }

    async fn get_with_token(
        app: &Router,
        uri: &str,
        admin_token: Option<&str>,
    ) -> axum::response::Response {
        let mut builder = Request::builder().method("GET").uri(uri);
        if let Some(token) = admin_token {
            builder = builder.header(ADMIN_HEADER_TOKEN, token);
        }
        let req = builder.body(Body::empty()).expect("build request failed");
        app.clone().oneshot(req).await.expect("request failed")
    }

    #[tokio::test]
    async fn admin_login_verify_logout_flow() {
        let app = test_app();

        let login_resp = post_json(
            &app,
            "/admin/login",
            json!({"username": "admin", "password": "secret"}),
            None,
        )
        .await;
        assert_eq!(login_resp.status(), StatusCode::OK);

        let login_json = parse_json(login_resp).await;
        let token = login_json["token"].as_str().expect("token missing").to_string();

        let verify_resp = get_with_token(&app, "/admin/verify", Some(&token)).await;
        assert_eq!(verify_resp.status(), StatusCode::OK);
        let verify_json = parse_json(verify_resp).await;
        assert_eq!(verify_json["valid"], json!(true));
        assert_eq!(verify_json["username"], json!("admin"));

        let logout_resp = post_json(&app, "/admin/logout", json!({}), Some(&token)).await;
        assert_eq!(logout_resp.status(), StatusCode::OK);

        let verify_again = get_with_token(&app, "/admin/verify", Some(&token)).await;
        assert_eq!(verify_again.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn create_room_requires_admin() {
        let app = test_app();

        let no_token_resp = post_json(
            &app,
            "/rooms",
            json!({"name": "room-a", "owner": "alice"}),
            None,
        )
        .await;
        assert_eq!(no_token_resp.status(), StatusCode::UNAUTHORIZED);

        let login_resp = post_json(
            &app,
            "/admin/login",
            json!({"username": "admin", "password": "secret"}),
            None,
        )
        .await;
        let login_json = parse_json(login_resp).await;
        let token = login_json["token"].as_str().expect("token missing");

        let create_resp = post_json(
            &app,
            "/rooms",
            json!({"name": "room-b", "owner": "alice", "max_players": 3}),
            Some(token),
        )
        .await;
        assert_eq!(create_resp.status(), StatusCode::CREATED);

        let room_json = parse_json(create_resp).await;
        assert_eq!(room_json["name"], json!("room-b"));
        assert_eq!(room_json["owner"], json!("alice"));
        assert_eq!(room_json["admins"], json!(["alice"]));
    }

    #[tokio::test]
    async fn join_and_kick_player_flow() {
        let app = test_app();

        let login_resp = post_json(
            &app,
            "/admin/login",
            json!({"username": "admin", "password": "secret"}),
            None,
        )
        .await;
        let login_json = parse_json(login_resp).await;
        let token = login_json["token"].as_str().expect("token missing").to_string();

        let create_resp = post_json(
            &app,
            "/rooms",
            json!({"name": "room-c", "owner": "alice", "max_players": 4}),
            Some(&token),
        )
        .await;
        let room_json = parse_json(create_resp).await;
        let room_id = room_json["id"].as_str().expect("room id missing").to_string();

        let join_bob = post_json(
            &app,
            &format!("/rooms/{}/join", room_id),
            json!({"player": "bob"}),
            None,
        )
        .await;
        assert_eq!(join_bob.status(), StatusCode::OK);

        let join_charlie = post_json(
            &app,
            &format!("/rooms/{}/join", room_id),
            json!({"player": "charlie"}),
            None,
        )
        .await;
        assert_eq!(join_charlie.status(), StatusCode::OK);

        let kick_resp = post_json(
            &app,
            &format!("/rooms/{}/kick", room_id),
            json!({"requester": "alice", "target": "charlie"}),
            Some(&token),
        )
        .await;
        assert_eq!(kick_resp.status(), StatusCode::OK);

        let kicked_room = parse_json(kick_resp).await;
        let players = kicked_room["players"]
            .as_array()
            .expect("players should be array");
        assert!(players.iter().any(|p| p == "alice"));
        assert!(players.iter().any(|p| p == "bob"));
        assert!(!players.iter().any(|p| p == "charlie"));
    }

    #[tokio::test]
    async fn owner_leave_transfers_owner() {
        let app = test_app();

        let login_resp = post_json(
            &app,
            "/admin/login",
            json!({"username": "admin", "password": "secret"}),
            None,
        )
        .await;
        let login_json = parse_json(login_resp).await;
        let token = login_json["token"].as_str().expect("token missing").to_string();

        let create_resp = post_json(
            &app,
            "/rooms",
            json!({"name": "room-d", "owner": "alice", "max_players": 4}),
            Some(&token),
        )
        .await;
        let room_json = parse_json(create_resp).await;
        let room_id = room_json["id"].as_str().expect("room id missing").to_string();

        let _ = post_json(
            &app,
            &format!("/rooms/{}/join", room_id),
            json!({"player": "bob"}),
            None,
        )
        .await;

        let leave_resp = post_json(
            &app,
            &format!("/rooms/{}/leave", room_id),
            json!({"player": "alice"}),
            None,
        )
        .await;
        assert_eq!(leave_resp.status(), StatusCode::OK);

        let room_after = parse_json(leave_resp).await;
        assert_eq!(room_after["owner"], json!("bob"));
        assert_eq!(room_after["admins"], json!(["bob"]));
    }
}
