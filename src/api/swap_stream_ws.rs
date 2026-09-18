//! WebSocket endpoint at `/swap-stream` — fan-out for parsed mainnet swaps.
//!
//! Distinct from `/quote-ws` (which streams *projected* prices for a
//! pair the client picks). This endpoint streams *observed* swaps as
//! they confirm on-chain, with input/output amounts, native price,
//! and USD price.
//!
//! Protocol:
//! 1. Client connects to `/swap-stream`.
//! 2. (Optional) client sends a JSON filter:
//!    ```json
//!    {
//!      "type": "subscribe",
//!      "filter": {
//!        "dex": ["Raydium V4", "Pumpup Bonding"],   // any of (optional)
//!        "mint": "9U3F…",                            // input or output (optional)
//!        "pool": "AQxK…",                            // exact pool (optional)
//!        "min_amount_usd": 10.0                       // floor on trade size (optional)
//!      }
//!    }
//!    ```
//!    Sending nothing is also OK — server treats it as "subscribe to
//!    all" after a 5-second grace period. After the first server frame
//!    (subscribed/swap) the filter is fixed for the connection.
//! 3. Server replies once with `{"type":"subscribed"}` then streams
//!    `{"type":"swap", …}` JSON frames for every matching swap.
//! 4. The server sends `{"type":"ping"}` every 30 s; clients should
//!    reply with `{"type":"pong"}` (any text frame keeps the conn alive).
//! 5. If the parser produces faster than the client drains, the
//!    underlying `tokio::sync::broadcast` lags the slow subscriber and
//!    drops messages — the connection stays open. The server emits a
//!    `{"type":"lagged","skipped":N}` frame on each detected gap.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tokio::time::Duration;
use tracing::{debug, warn};

use super::AppState;
use crate::stream::swap_stream::SwapFilter;

/// How long to wait for the optional subscribe message before
/// defaulting to no-filter "subscribe to all" mode.
const FILTER_GRACE_MS: u64 = 5_000;
/// Server-side keep-alive ping interval.
const PING_INTERVAL_SECS: u64 = 30;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    /// Configure the filter (optional). Without this, all swaps are streamed.
    Subscribe {
        #[serde(default)]
        filter: Option<SwapFilter>,
    },
    /// Pong reply to a server ping (or any client keep-alive).
    Pong,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg<'a> {
    Subscribed,
    /// Server-initiated ping. Clients may reply with `{"type":"pong"}` or
    /// any text/pong frame.
    Ping,
    /// The broadcast channel lagged this subscriber by `skipped` swaps.
    Lagged { skipped: u64 },
    Error { error: &'a str },
}

/// `GET /swap-stream` — WebSocket upgrade.
pub async fn handle_swap_stream_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_swap_stream(socket, state))
}

async fn handle_swap_stream(mut socket: WebSocket, state: Arc<AppState>) {
    debug!("swap-stream client connected");

    let bcast_tx = match state.swap_broadcast.as_ref() {
        Some(tx) => tx,
        None => {
            let _ = send_text(
                &mut socket,
                &serde_json::to_string(&ServerMsg::Error {
                    error: "swap stream is disabled on this server",
                })
                .unwrap_or_else(|_| "{}".into()),
            )
            .await;
            return;
        }
    };

    // Wait briefly for the optional Subscribe message.
    let filter = match wait_for_filter(&mut socket).await {
        Ok(f) => f,
        Err(msg) => {
            let _ = send_text(
                &mut socket,
                &serde_json::to_string(&ServerMsg::Error { error: &msg })
                    .unwrap_or_else(|_| "{}".into()),
            )
            .await;
            return;
        }
    };

    if send_text(
        &mut socket,
        &serde_json::to_string(&ServerMsg::Subscribed).unwrap_or_else(|_| "{}".into()),
    )
    .await
    .is_err()
    {
        return;
    }

    let mut rx = bcast_tx.subscribe();
    let mut ping_interval = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
    ping_interval.tick().await; // skip the immediate first tick

    loop {
        tokio::select! {
            biased;

            // Inbound: client message (close, pong, ignore).
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(p))) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    Some(Ok(_)) => { /* pong / text are fine — keep going */ }
                    Some(Err(e)) => {
                        debug!(error = %e, "swap-stream WS error, closing");
                        break;
                    }
                }
            }

            // Broadcast: parsed swap.
            recv = rx.recv() => {
                match recv {
                    Ok(swap) => {
                        if !filter.matches(&swap) {
                            continue;
                        }
                        match serde_json::to_string(&*swap) {
                            Ok(text) => {
                                if socket.send(Message::Text(text.into())).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => warn!(error = %e, "swap serialize"),
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        let lagged = serde_json::to_string(&ServerMsg::Lagged { skipped: n })
                            .unwrap_or_else(|_| "{}".into());
                        let _ = socket.send(Message::Text(lagged.into())).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }

            // Keep-alive: emit a ping frame periodically.
            _ = ping_interval.tick() => {
                let ping = serde_json::to_string(&ServerMsg::Ping)
                    .unwrap_or_else(|_| "{}".into());
                if socket.send(Message::Text(ping.into())).await.is_err() {
                    break;
                }
            }
        }
    }
    debug!("swap-stream client disconnected");
}

/// Wait up to `FILTER_GRACE_MS` for an optional `subscribe` message.
/// Returns the filter (or default if no msg arrived in the window).
async fn wait_for_filter(socket: &mut WebSocket) -> Result<SwapFilter, String> {
    let timeout = tokio::time::timeout(
        Duration::from_millis(FILTER_GRACE_MS),
        socket.recv(),
    )
    .await;
    match timeout {
        // Timeout — no message; default filter (allow all).
        Err(_) => Ok(SwapFilter::default()),
        // Connection closed / disconnected.
        Ok(None) | Ok(Some(Ok(Message::Close(_)))) => Err("client disconnected".into()),
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<ClientMsg>(&text) {
            Ok(ClientMsg::Subscribe { filter }) => Ok(filter.unwrap_or_default()),
            Ok(ClientMsg::Pong) => Ok(SwapFilter::default()),
            Err(e) => Err(format!("invalid subscribe: {e}")),
        },
        Ok(Some(Ok(Message::Ping(p)))) => {
            let _ = socket.send(Message::Pong(p)).await;
            // Treat the ping as "no filter sent yet" and default through.
            Ok(SwapFilter::default())
        }
        Ok(Some(Ok(_))) => Ok(SwapFilter::default()),
        Ok(Some(Err(e))) => Err(format!("ws recv: {e}")),
    }
}

async fn send_text(socket: &mut WebSocket, body: &str) -> Result<(), axum::Error> {
    socket.send(Message::Text(body.to_string().into())).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::swap_stream::Swap;

    #[test]
    fn test_subscribe_msg_parses() {
        let json = r#"{"type":"subscribe","filter":{"dex":["Pumpup Bonding"],"min_amount_usd":1.5}}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            ClientMsg::Subscribe { filter: Some(f) } => {
                assert_eq!(f.dex, vec!["Pumpup Bonding"]);
                assert_eq!(f.min_amount_usd, Some(1.5));
            }
            _ => panic!("expected subscribe"),
        }
    }

    #[test]
    fn test_subscribe_msg_no_filter() {
        let json = r#"{"type":"subscribe"}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            ClientMsg::Subscribe { filter: None } => (),
            _ => panic!("expected subscribe with no filter"),
        }
    }

    #[test]
    fn test_pong_msg_parses() {
        let json = r#"{"type":"pong"}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        matches!(msg, ClientMsg::Pong);
    }

    #[test]
    fn test_server_msg_serializes() {
        assert_eq!(
            serde_json::to_string(&ServerMsg::Subscribed).unwrap(),
            r#"{"type":"subscribed"}"#
        );
        assert_eq!(
            serde_json::to_string(&ServerMsg::Ping).unwrap(),
            r#"{"type":"ping"}"#
        );
        assert_eq!(
            serde_json::to_string(&ServerMsg::Lagged { skipped: 42 }).unwrap(),
            r#"{"type":"lagged","skipped":42}"#
        );
    }

    /// `Swap` already has `#[serde(rename = "type")] kind: &'static str = "swap"`.
    /// Confirm the wire frame matches.
    #[test]
    fn test_swap_wire_frame_starts_with_type_swap() {
        use crate::constants::{SOL_NATIVE_MINT, USDC_MINT};
        use crate::stream::swap_stream::SwapSide;
        let s = Swap {
            kind: "swap",
            signature: "abc".into(),
            slot: 1,
            block_time: Some(123),
            dex: "Orca".into(),
            pool: "POOL".into(),
            user: "USER".into(),
            input: SwapSide::new(SOL_NATIVE_MINT, 1_000_000, 9),
            output: SwapSide::new(USDC_MINT, 100_000, 6),
            price_native: Some("100".into()),
            price_native_inverted: Some("0.01".into()),
            price_usd: Some("1.0".into()),
            amount_usd: Some("0.10".into()),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""type":"swap""#));
        assert!(json.contains(r#""dex":"Orca""#));
        assert!(json.contains(r#""price_usd":"1.0""#));
    }
}
