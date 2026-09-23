/// WebSocket quote streaming endpoint.
///
/// Clients connect to `/ws`, send a subscription message specifying the token pair
/// and amount, and receive streaming quote updates at the configured interval.
///
/// Protocol:
/// 1. Client connects to `/ws`
/// 2. Client sends JSON: `{ "input_mint": "...", "output_mint": "...", "amount": 1000000, "slippage_bps": 50, "interval_ms": 1000 }`
/// 3. Server pushes QuoteResponse JSON at the configured interval (min 100ms)
/// 4. Client can send a new subscription to change parameters
/// 5. Either side can close the connection

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{debug, warn};

use super::AppState;
use crate::quote::QuoteRequest;

/// Minimum interval between quote updates (prevents abuse).
const MIN_INTERVAL_MS: u64 = 100;
/// Default interval if not specified by client.
const DEFAULT_INTERVAL_MS: u64 = 1000;

/// WebSocket subscription message from the client.
#[derive(Debug, Deserialize)]
pub struct WsSubscription {
    /// Input token mint address (base58).
    pub input_mint: String,
    /// Output token mint address (base58).
    pub output_mint: String,
    /// Input amount in smallest token units.
    pub amount: u64,
    /// Slippage tolerance in basis points (default 50).
    pub slippage_bps: Option<u16>,
    /// Update interval in milliseconds (default 1000, min 100).
    pub interval_ms: Option<u64>,
}

/// Error response sent to the client over WebSocket.
#[derive(Debug, Serialize)]
struct WsError {
    error: String,
}

/// GET /ws — WebSocket upgrade handler.
pub async fn handle_ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws(socket, state))
}

/// Handle an established WebSocket connection.
async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    debug!("WebSocket client connected");

    // Wait for the subscription message from the client
    let sub = match wait_for_subscription(&mut socket).await {
        Some(s) => s,
        None => return, // Client disconnected or sent invalid data
    };

    // Parse the subscription into a QuoteRequest
    let (mut req, interval_ms) = match parse_subscription(&sub) {
        Ok(r) => r,
        Err(e) => {
            let _ = send_error(&mut socket, &e).await;
            return;
        }
    };

    // Stream quotes at the configured interval. A slow quote skips the ticks it
    // overran instead of bursting the backlog as identical quotes.
    let ticker_for = |ms: u64| {
        let mut t = interval(Duration::from_millis(ms));
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        t
    };
    let mut ticker = ticker_for(interval_ms);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match state.quoter.quote(&req).await {
                    Ok(resp) => {
                        match serde_json::to_string(&resp) {
                            Ok(json) => {
                                if socket.send(Message::Text(json.into())).await.is_err() {
                                    debug!("WebSocket client disconnected during send");
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!("Failed to serialize quote response: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        // Send error but don't disconnect — pool state may become
                        // available on the next tick
                        let _ = send_error(&mut socket, &format!("quote error: {e}")).await;
                    }
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // A new subscription replaces the current one; an invalid
                        // one is reported and the current one keeps streaming.
                        let parsed = serde_json::from_str::<WsSubscription>(&text)
                            .map_err(|e| format!("invalid subscription: {e}"))
                            .and_then(|sub| parse_subscription(&sub));
                        match parsed {
                            Ok((new_req, new_interval_ms)) => {
                                req = new_req;
                                ticker = ticker_for(new_interval_ms);
                            }
                            Err(e) => {
                                let _ = send_error(&mut socket, &e).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        debug!("WebSocket client disconnected");
                        break;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Wait for the initial subscription message.
async fn wait_for_subscription(socket: &mut WebSocket) -> Option<WsSubscription> {
    // Wait up to 30 seconds for the subscription message
    let timeout = tokio::time::timeout(Duration::from_secs(30), socket.recv()).await;

    match timeout {
        Ok(Some(Ok(Message::Text(text)))) => {
            match serde_json::from_str::<WsSubscription>(&text) {
                Ok(sub) => Some(sub),
                Err(e) => {
                    let _ = send_error(socket, &format!("invalid subscription: {e}")).await;
                    None
                }
            }
        }
        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
            debug!("WebSocket client disconnected before subscribing");
            None
        }
        Err(_) => {
            let _ = send_error(socket, "subscription timeout (30s)").await;
            None
        }
        _ => {
            let _ = send_error(socket, "expected text message for subscription").await;
            None
        }
    }
}

/// Parse a subscription into a QuoteRequest and interval.
fn parse_subscription(sub: &WsSubscription) -> Result<(QuoteRequest, u64), String> {
    let input_mint = Pubkey::from_str(&sub.input_mint)
        .map_err(|_| format!("invalid input_mint: {}", sub.input_mint))?;
    let output_mint = Pubkey::from_str(&sub.output_mint)
        .map_err(|_| format!("invalid output_mint: {}", sub.output_mint))?;

    if input_mint == output_mint {
        return Err("input_mint and output_mint must be different".into());
    }

    if sub.amount == 0 {
        return Err("amount must be > 0".into());
    }

    let slippage_bps = sub.slippage_bps.unwrap_or(50);
    if slippage_bps > 10_000 {
        return Err(format!("slippage_bps must be <= 10000, got {slippage_bps}"));
    }

    let interval_ms = sub.interval_ms.unwrap_or(DEFAULT_INTERVAL_MS).max(MIN_INTERVAL_MS);

    let req = QuoteRequest {
        input_mint,
        output_mint,
        amount: sub.amount,
        slippage_bps,
        only_direct_routes: false,
        exclude_dexes: vec![],
        dexes: vec![],
        max_accounts: 64,
    };

    Ok((req, interval_ms))
}

/// Send an error message over the WebSocket.
async fn send_error(socket: &mut WebSocket, msg: &str) -> Result<(), axum::Error> {
    let error = WsError {
        error: msg.to_string(),
    };
    let json = serde_json::to_string(&error).unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string());
    socket.send(Message::Text(json.into())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_subscription_valid() {
        let sub = WsSubscription {
            input_mint: "So11111111111111111111111111111111111111112".into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount: 1_000_000_000,
            slippage_bps: Some(50),
            interval_ms: Some(500),
        };

        let (req, interval_ms) = parse_subscription(&sub).unwrap();
        assert_eq!(req.amount, 1_000_000_000);
        assert_eq!(req.slippage_bps, 50);
        assert_eq!(interval_ms, 500);
        assert!(!req.only_direct_routes);
    }

    #[test]
    fn test_parse_subscription_defaults() {
        let sub = WsSubscription {
            input_mint: "So11111111111111111111111111111111111111112".into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount: 1_000_000,
            slippage_bps: None,
            interval_ms: None,
        };

        let (req, interval_ms) = parse_subscription(&sub).unwrap();
        assert_eq!(req.slippage_bps, 50); // default
        assert_eq!(interval_ms, DEFAULT_INTERVAL_MS); // default 1000
    }

    #[test]
    fn test_parse_subscription_min_interval() {
        let sub = WsSubscription {
            input_mint: "So11111111111111111111111111111111111111112".into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount: 1_000_000,
            slippage_bps: None,
            interval_ms: Some(10), // below minimum
        };

        let (_, interval_ms) = parse_subscription(&sub).unwrap();
        assert_eq!(interval_ms, MIN_INTERVAL_MS); // clamped to 100
    }

    #[test]
    fn test_parse_subscription_invalid_input_mint() {
        let sub = WsSubscription {
            input_mint: "invalid_mint".into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount: 1_000_000,
            slippage_bps: None,
            interval_ms: None,
        };

        let result = parse_subscription(&sub);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid input_mint"));
    }

    #[test]
    fn test_parse_subscription_invalid_output_mint() {
        let sub = WsSubscription {
            input_mint: "So11111111111111111111111111111111111111112".into(),
            output_mint: "not_a_pubkey".into(),
            amount: 1_000_000,
            slippage_bps: None,
            interval_ms: None,
        };

        let result = parse_subscription(&sub);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid output_mint"));
    }

    #[test]
    fn test_parse_subscription_same_mints() {
        let sub = WsSubscription {
            input_mint: "So11111111111111111111111111111111111111112".into(),
            output_mint: "So11111111111111111111111111111111111111112".into(),
            amount: 1_000_000,
            slippage_bps: None,
            interval_ms: None,
        };

        let result = parse_subscription(&sub);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("different"));
    }

    #[test]
    fn test_parse_subscription_zero_amount() {
        let sub = WsSubscription {
            input_mint: "So11111111111111111111111111111111111111112".into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount: 0,
            slippage_bps: None,
            interval_ms: None,
        };

        let result = parse_subscription(&sub);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("amount"));
    }

    #[test]
    fn test_parse_subscription_high_slippage() {
        let sub = WsSubscription {
            input_mint: "So11111111111111111111111111111111111111112".into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount: 1_000_000,
            slippage_bps: Some(20_000),
            interval_ms: None,
        };

        let result = parse_subscription(&sub);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("slippage"));
    }

    #[test]
    fn test_ws_subscription_deserialize() {
        let json = r#"{
            "input_mint": "So11111111111111111111111111111111111111112",
            "output_mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "amount": 1000000000,
            "slippage_bps": 50,
            "interval_ms": 500
        }"#;

        let sub: WsSubscription = serde_json::from_str(json).unwrap();
        assert_eq!(sub.amount, 1_000_000_000);
        assert_eq!(sub.slippage_bps, Some(50));
        assert_eq!(sub.interval_ms, Some(500));
    }

    #[test]
    fn test_ws_subscription_deserialize_minimal() {
        let json = r#"{
            "input_mint": "So11111111111111111111111111111111111111112",
            "output_mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "amount": 1000000
        }"#;

        let sub: WsSubscription = serde_json::from_str(json).unwrap();
        assert_eq!(sub.amount, 1_000_000);
        assert!(sub.slippage_bps.is_none());
        assert!(sub.interval_ms.is_none());
    }

    #[test]
    fn test_ws_error_serialize() {
        let err = WsError {
            error: "test error".to_string(),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("test error"));
    }
}
