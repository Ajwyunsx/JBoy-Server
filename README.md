# JBoyServer (Rust)

Rust implementation for JBoy netplay backend with:

- Room management (create/list/get/delete)
- Admin token login/verify/logout and protected room operations
- WebSocket room channel for messaging + `jboy-link-1` handshake sync

## Quick Start

```bash
cargo run
```

Default bind: `0.0.0.0:8080`

## Environment Variables

- `JBOY_BIND` (default `0.0.0.0:8080`)
- `JBOY_ADMIN_PASSWORD` (default `admin123`)
- `RUST_LOG` (default `jboy_server=info,tower_http=info,axum=info`)

## HTTP APIs

- `GET /health`
- `GET /stats` (admin required)
- `POST /admin/login`
- `GET /admin/verify` (admin required)
- `POST /admin/logout` (admin required)
- `GET /rooms`
- `GET /rooms/:room_id`
- `POST /rooms` (admin required, header `x-admin-token`)
- `DELETE /rooms/:room_id` (admin required)
- `POST /rooms/:room_id/join`
- `POST /rooms/:room_id/leave`
- `POST /rooms/:room_id/kick` (room admin or platform admin)

## WebSocket

- `GET /ws/:room_id/:player`

This route broadcasts join/leave/message envelopes inside the room channel.

Protocol messages supported:

- `{"type":"hello"...}` -> server returns `link_sync`
- `{"type":"ready","ready":true|false}` -> updates ready roster and rebroadcasts `link_sync`
- `{"type":"ping"}` -> server replies `pong`

`link_sync` payload includes:

- `protocol` (`jboy-link-1`)
- `room`
- `host`
- `players`
- `readyPlayers`
- `canStart` (true when at least two players are ready)

## LAN / Tailscale

- Keep default bind as `0.0.0.0:8080` to accept LAN and virtual LAN traffic.
- Android client supports `ws://192.168.x.x` / `ws://10.x.x.x` / `ws://100.x.x.x` and MagicDNS hostnames like `your-device.tailnet.ts.net`.
- Client will auto-fill port `8080` when omitted.

## Tests

```bash
cargo test
```

Covered scenarios:

- admin login/verify/logout flow
- room creation auth guard
- join + kick player flow
- owner leave and ownership transfer
