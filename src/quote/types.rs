use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use crate::error::{TradeError, TradeResult};

/// Parsed from GET /quote query parameters.
#[derive(Debug, Clone)]
pub struct QuoteRequest {
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount: u64,
    pub slippage_bps: u16,
    pub only_direct_routes: bool,
    pub exclude_dexes: Vec<String>,
    pub dexes: Vec<String>,
    pub max_accounts: u8,
}

impl QuoteRequest {
    /// Parse from raw query string parameters.
    pub fn from_params(params: &QuoteParams) -> TradeResult<Self> {
        let input_mint = Pubkey::from_str(&params.input)
            .map_err(|_| TradeError::Validation(format!("invalid input: {}", params.input)))?;
        let output_mint = Pubkey::from_str(&params.output)
            .map_err(|_| TradeError::Validation(format!("invalid output: {}", params.output)))?;

        if input_mint == output_mint {
            return Err(TradeError::Validation("input and output must be different".into()));
        }

        let amount = params.amount.parse::<u64>()
            .map_err(|_| TradeError::Validation(format!("invalid amount: {}", params.amount)))?;
        if amount == 0 {
            return Err(TradeError::Validation("amount must be > 0".into()));
        }

        // The router executes exact-input swaps only; quoting `amount` as an
        // input when the caller meant an output would be silently wrong.
        if let Some(mode) = params.mode.as_deref() {
            if !mode.eq_ignore_ascii_case("ExactIn") {
                return Err(TradeError::Validation(format!("unsupported mode {mode}: only ExactIn is supported")));
            }
        }

        let slippage_bps = params.slippage.unwrap_or(50);
        if slippage_bps > 10_000 {
            return Err(TradeError::Validation(format!("slippage must be <= 10000, got {slippage_bps}")));
        }

        let exclude_dexes = params.exclude.as_ref()
            .map(|s| s.split(',').map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).collect())
            .unwrap_or_default();

        let dexes = params.dexes.as_ref()
            .map(|s| s.split(',').map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).collect())
            .unwrap_or_default();

        let max_accounts = params.max_accounts.unwrap_or(64);

        Ok(Self {
            input_mint,
            output_mint,
            amount,
            slippage_bps,
            only_direct_routes: params.direct_only.unwrap_or(true),
            exclude_dexes,
            dexes,
            max_accounts,
        })
    }
}

/// Raw query parameters from the HTTP request.
#[derive(Debug, Deserialize)]
pub struct QuoteParams {
    pub input: String,
    pub output: String,
    pub amount: String,
    pub slippage: Option<u16>,
    pub direct_only: Option<bool>,
    pub exclude: Option<String>,
    pub dexes: Option<String>,
    pub max_accounts: Option<u8>,
    pub mode: Option<String>,
}

/// Platform fee information included in quote responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformFee {
    pub amount: String,
    pub fee_bps: u16,
    pub fee_token: String,
    pub side: String,
}

/// Quote response. All numeric fields are strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteResponse {
    pub input_token: String,
    pub amount_in: String,
    pub output_token: String,
    pub amount_out: String,
    pub minimum_out: String,
    pub mode: String,
    pub slippage_bps: u16,
    pub price_impact: String,
    pub routes: Vec<RouteStep>,
    pub slot: u64,
    pub quote_time_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_fee: Option<PlatformFee>,
}

/// Single route step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteStep {
    pub pool: PoolRoute,
    pub percent: u8,
}

/// Info about a single swap pool within a route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolRoute {
    pub pool_address: String,
    pub dex: String,
    pub input_token: String,
    pub output_token: String,
    pub amount_in: String,
    pub amount_out: String,
    pub fee: String,
    pub fee_token: String,
}

/// Compute other_amount_threshold from out_amount and slippage_bps.
/// threshold = out_amount * (10000 - slippage_bps) / 10000
pub fn compute_threshold(out_amount: u64, slippage_bps: u16) -> u64 {
    let bps = slippage_bps.min(10_000) as u128;
    let out = out_amount as u128;
    ((out * (10_000 - bps)) / 10_000) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_threshold_basic() {
        // 1000 tokens, 50 bps (0.5%) slippage
        // threshold = 1000 * 9950 / 10000 = 995
        assert_eq!(compute_threshold(1000, 50), 995);
    }

    #[test]
    fn test_compute_threshold_zero_slippage() {
        assert_eq!(compute_threshold(1000, 0), 1000);
    }

    #[test]
    fn test_compute_threshold_100_percent_slippage() {
        assert_eq!(compute_threshold(1000, 10_000), 0);
    }

    #[test]
    fn test_compute_threshold_large_amount() {
        // 1 billion tokens, 100 bps (1%)
        let out = 1_000_000_000u64;
        let threshold = compute_threshold(out, 100);
        assert_eq!(threshold, 990_000_000);
    }

    #[test]
    fn test_compute_threshold_zero_amount() {
        assert_eq!(compute_threshold(0, 50), 0);
    }

    #[test]
    fn test_compute_threshold_clamp_excess_bps() {
        // bps > 10000 should be clamped
        assert_eq!(compute_threshold(1000, 15_000), 0);
    }

    #[test]
    fn test_quote_request_rejects_exact_out() {
        let params = |mode: Option<&str>| QuoteParams {
            input: "So11111111111111111111111111111111111111112".to_string(),
            output: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            amount: "1000000".to_string(),
            slippage: None,
            direct_only: None,
            exclude: None,
            dexes: None,
            max_accounts: None,
            mode: mode.map(str::to_string),
        };
        assert!(QuoteRequest::from_params(&params(Some("ExactOut"))).is_err(), "amount would be read as an input");
        assert!(QuoteRequest::from_params(&params(Some("ExactIn"))).is_ok());
        assert!(QuoteRequest::from_params(&params(None)).is_ok());
    }

    #[test]
    fn test_quote_request_from_params_basic() {
        let sol = "So11111111111111111111111111111111111111112";
        let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let params = QuoteParams {
            input: sol.to_string(),
            output: usdc.to_string(),
            amount: "1000000000".to_string(),
            slippage: Some(50),
            direct_only: None,
            exclude: None,
            dexes: None,
            max_accounts: None,
            mode: None,
        };
        let req = QuoteRequest::from_params(&params).unwrap();
        assert_eq!(req.amount, 1_000_000_000);
        assert_eq!(req.slippage_bps, 50);
        assert!(req.only_direct_routes);
        assert_eq!(req.max_accounts, 64);
    }

    #[test]
    fn test_quote_request_invalid_mint() {
        let params = QuoteParams {
            input: "invalid".to_string(),
            output: "So11111111111111111111111111111111111111112".to_string(),
            amount: "1000".to_string(),
            slippage: None,
            direct_only: None,
            exclude: None,
            dexes: None,
            max_accounts: None,
            mode: None,
        };
        assert!(QuoteRequest::from_params(&params).is_err());
    }

    #[test]
    fn test_quote_request_zero_amount() {
        let sol = "So11111111111111111111111111111111111111112";
        let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let params = QuoteParams {
            input: sol.to_string(),
            output: usdc.to_string(),
            amount: "0".to_string(),
            slippage: None,
            direct_only: None,
            exclude: None,
            dexes: None,
            max_accounts: None,
            mode: None,
        };
        assert!(QuoteRequest::from_params(&params).is_err());
    }

    #[test]
    fn test_quote_request_same_mints() {
        let sol = "So11111111111111111111111111111111111111112";
        let params = QuoteParams {
            input: sol.to_string(),
            output: sol.to_string(),
            amount: "1000".to_string(),
            slippage: None,
            direct_only: None,
            exclude: None,
            dexes: None,
            max_accounts: None,
            mode: None,
        };
        assert!(QuoteRequest::from_params(&params).is_err());
    }

    #[test]
    fn test_quote_request_slippage_too_high() {
        let sol = "So11111111111111111111111111111111111111112";
        let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let params = QuoteParams {
            input: sol.to_string(),
            output: usdc.to_string(),
            amount: "1000".to_string(),
            slippage: Some(10_001),
            direct_only: None,
            exclude: None,
            dexes: None,
            max_accounts: None,
            mode: None,
        };
        assert!(QuoteRequest::from_params(&params).is_err());
    }

    #[test]
    fn test_quote_request_exclude_dexes_parsing() {
        let sol = "So11111111111111111111111111111111111111112";
        let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let params = QuoteParams {
            input: sol.to_string(),
            output: usdc.to_string(),
            amount: "1000".to_string(),
            slippage: None,
            direct_only: None,
            exclude: Some("Orca, Raydium CPMM".to_string()),
            dexes: None,
            max_accounts: None,
            mode: None,
        };
        let req = QuoteRequest::from_params(&params).unwrap();
        assert_eq!(req.exclude_dexes.len(), 2);
        assert_eq!(req.exclude_dexes[0], "Orca");
        assert_eq!(req.exclude_dexes[1], "Raydium CPMM");
    }

    #[test]
    fn test_quote_response_serialization() {
        let resp = QuoteResponse {
            input_token: "So11111111111111111111111111111111111111112".to_string(),
            amount_in: "1000000000".to_string(),
            output_token: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            amount_out: "162500000".to_string(),
            minimum_out: "161687500".to_string(),
            mode: "ExactIn".to_string(),
            slippage_bps: 50,
            price_impact: "0.05".to_string(),
            routes: vec![RouteStep {
                pool: PoolRoute {
                    pool_address: "HJPjoW...".to_string(),
                    dex: "Raydium CPMM".to_string(),
                    input_token: "So11111111111111111111111111111111111111112".to_string(),
                    output_token: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
                    amount_in: "1000000000".to_string(),
                    amount_out: "162500000".to_string(),
                    fee: "250000".to_string(),
                    fee_token: "So11111111111111111111111111111111111111112".to_string(),
                },
                percent: 100,
            }],
            slot: 408947310,
            quote_time_ms: 0.012,
            platform_fee: None,
        };

        let json = serde_json::to_string(&resp).unwrap();
        // platform_fee should be omitted when None (skip_serializing_if)
        assert!(!json.contains("\"platform_fee\""));
        assert!(json.contains("\"input_token\""));
        assert!(json.contains("\"amount_in\""));
        assert!(json.contains("\"amount_out\""));
        assert!(json.contains("\"minimum_out\""));
        assert!(json.contains("\"mode\""));
        assert!(json.contains("\"slippage_bps\""));
        assert!(json.contains("\"routes\""));
        assert!(json.contains("\"slot\""));
        assert!(json.contains("\"quote_time_ms\""));

        // Roundtrip
        let parsed: QuoteResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.amount_out, "162500000");
        assert_eq!(parsed.slippage_bps, 50);
    }

    #[test]
    fn test_quote_request_defaults() {
        let sol = "So11111111111111111111111111111111111111112";
        let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let params = QuoteParams {
            input: sol.to_string(),
            output: usdc.to_string(),
            amount: "1000".to_string(),
            slippage: None,
            direct_only: None,
            exclude: None,
            dexes: None,
            max_accounts: None,
            mode: None,
        };
        let req = QuoteRequest::from_params(&params).unwrap();
        assert_eq!(req.slippage_bps, 50);
        assert!(req.only_direct_routes);
        assert_eq!(req.max_accounts, 64);
        assert!(req.exclude_dexes.is_empty());
        assert!(req.dexes.is_empty());
    }

    #[test]
    fn test_platform_fee_serialization_roundtrip() {
        let fee = PlatformFee {
            amount: "5000".to_string(),
            fee_bps: 50,
            fee_token: "So11111111111111111111111111111111111111112".to_string(),
            side: "input".to_string(),
        };
        let json = serde_json::to_string(&fee).unwrap();
        assert!(json.contains("\"amount\":\"5000\""));
        assert!(json.contains("\"fee_bps\":50"));
        assert!(json.contains("\"side\":\"input\""));

        let parsed: PlatformFee = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.amount, "5000");
        assert_eq!(parsed.fee_bps, 50);
        assert_eq!(parsed.side, "input");
    }

    // ── Slippage enforcement: compute_threshold edge cases ──

    #[test]
    fn test_compute_threshold_1_bps_slippage() {
        // 1 bps on 10000 tokens: 10000 * (10000 - 1) / 10000 = 10000 * 9999 / 10000 = 9999
        assert_eq!(compute_threshold(10_000, 1), 9_999);
    }

    #[test]
    fn test_compute_threshold_max_bps_slippage() {
        // 10000 bps = 100% slippage → threshold = 0
        assert_eq!(compute_threshold(1_000_000, 10_000), 0);
    }

    #[test]
    fn test_compute_threshold_preserves_precision_large_amount() {
        // 1_000_000_000_000 tokens with 50 bps:
        // threshold = 1_000_000_000_000 * (10000 - 50) / 10000 = 1_000_000_000_000 * 9950 / 10000 = 995_000_000_000
        assert_eq!(compute_threshold(1_000_000_000_000, 50), 995_000_000_000);
    }

    #[test]
    fn test_compute_threshold_u64_max_no_overflow() {
        // u64::MAX with 100 bps should not overflow (uses u128 internally)
        let result = compute_threshold(u64::MAX, 100);
        // u64::MAX * 9900 / 10000 — should be ~99% of u64::MAX
        let expected = ((u64::MAX as u128) * 9_900 / 10_000) as u64;
        assert_eq!(result, expected);
    }

    #[test]
    fn test_compute_threshold_small_amount_rounds_down() {
        // 3 tokens with 5000 bps (50%): 3 * 5000 / 10000 = 1 (integer division rounds down)
        assert_eq!(compute_threshold(3, 5_000), 1);
    }

    #[test]
    fn test_quote_response_with_platform_fee() {
        let resp = QuoteResponse {
            input_token: "So11111111111111111111111111111111111111112".to_string(),
            amount_in: "1000000".to_string(),
            output_token: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            amount_out: "162500".to_string(),
            minimum_out: "161687".to_string(),
            mode: "ExactIn".to_string(),
            slippage_bps: 50,
            price_impact: "0.05".to_string(),
            routes: vec![],
            slot: 0,
            quote_time_ms: 0.01,
            platform_fee: Some(PlatformFee {
                amount: "5000".to_string(),
                fee_bps: 50,
                fee_token: "So11111111111111111111111111111111111111112".to_string(),
                side: "input".to_string(),
            }),
        };

        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"platform_fee\""));
        assert!(json.contains("\"fee_bps\":50"));

        let parsed: QuoteResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.platform_fee.is_some());
        let pf = parsed.platform_fee.unwrap();
        assert_eq!(pf.amount, "5000");
        assert_eq!(pf.fee_bps, 50);
    }
}
