/// AMM math for computing output amounts from pool reserves.
///
/// Supports:
/// - Constant-product AMMs (Raydium, Meteora, FluxBeam, etc.)
/// - Bonding curve AMMs (PumpFun, Meteora DBC) — virtual reserves + custom fees
/// - CLMM single-tick approximation (Orca, Raydium CLMM, PancakeSwap)
/// - ExactOut reverse math for all of the above

/// Compute output amount for a constant-product AMM (x * y = k).
///
/// Formula: out = (reserve_out * amount_in * (10000 - fee_bps)) / (reserve_in * 10000 + amount_in * (10000 - fee_bps))
///
/// Returns None if reserves are zero, amount is zero, or overflow occurs.
pub fn compute_constant_product_out(
    reserve_in: u128,
    reserve_out: u128,
    amount_in: u64,
    fee_bps: u16,
) -> Option<u64> {
    if reserve_in == 0 || reserve_out == 0 || amount_in == 0 {
        return None;
    }

    let fee_bps = fee_bps.min(10_000) as u128;
    let amount_in = amount_in as u128;

    // amount_in_after_fee = amount_in * (10000 - fee_bps)
    let amount_in_after_fee = amount_in.checked_mul(10_000 - fee_bps)?;

    // numerator = reserve_out * amount_in_after_fee
    let numerator = reserve_out.checked_mul(amount_in_after_fee)?;

    // denominator = reserve_in * 10000 + amount_in_after_fee
    let denominator = reserve_in.checked_mul(10_000)?.checked_add(amount_in_after_fee)?;

    if denominator == 0 {
        return None;
    }

    let out = numerator / denominator;

    // Ensure output fits in u64
    if out > u64::MAX as u128 {
        return None;
    }

    Some(out as u64)
}

/// Compute fee amount for a constant-product swap.
/// fee_amount = amount_in * fee_bps / 10000
pub fn compute_fee_amount(amount_in: u64, fee_bps: u16) -> u64 {
    let fee_bps = fee_bps.min(10_000) as u128;
    let amount_in = amount_in as u128;
    ((amount_in * fee_bps) / 10_000) as u64
}

/// Estimate price impact as a percentage (generic, works for constant-product AMMs).
/// price_impact = 1 - (out_amount / (amount_in * reserve_out / reserve_in))
/// Returns the percentage as a string with 2 decimal places.
pub fn estimate_price_impact(
    reserve_in: u128,
    reserve_out: u128,
    amount_in: u64,
    amount_out: u64,
) -> String {
    if reserve_in == 0 || amount_in == 0 || amount_out == 0 {
        return "0.00".to_string();
    }

    // Ideal output without slippage: amount_in * reserve_out / reserve_in
    let amount_in_128 = amount_in as u128;
    let ideal_out = amount_in_128
        .checked_mul(reserve_out)
        .and_then(|n| Some(n / reserve_in));

    match ideal_out {
        Some(ideal) if ideal > 0 => {
            let amount_out_128 = amount_out as u128;
            // price_impact_pct = (1 - actual/ideal) * 100
            // Compute in basis points for precision: (ideal - actual) * 10000 / ideal
            if amount_out_128 >= ideal {
                "0.00".to_string()
            } else {
                let impact_bps = ((ideal - amount_out_128) * 10_000) / ideal;
                // Convert basis points to percentage string
                let pct_whole = impact_bps / 100;
                let pct_frac = impact_bps % 100;
                format!("{pct_whole}.{pct_frac:02}")
            }
        }
        _ => "0.00".to_string(),
    }
}

/// Per-AMM price impact computation.
///
/// Dispatches to the appropriate impact formula based on pool type:
/// - Constant-product AMMs: standard `1 - actual/ideal` formula using reserves
/// - CLMM AMMs: same formula (approximation — real impact depends on tick depth)
/// - Bonding curve AMMs: same formula (virtual reserves behave like constant product)
///
/// For all AMM types the fundamental concept is the same (actual vs. ideal output),
/// but CLMM pools typically have lower impact for the same nominal reserves because
/// liquidity is concentrated around the current price.
///
/// Returns a formatted percentage string like "0.15".
pub fn compute_price_impact_for_type(
    pool_type: crate::pool::types::PoolType,
    amount_in: u64,
    amount_out: u64,
    reserve_in: u128,
    reserve_out: u128,
) -> String {
    use crate::pool::types::PoolType;

    match pool_type {
        // Constant-product AMMs: standard impact formula
        PoolType::RaydiumV4
        | PoolType::RaydiumCpmm
        | PoolType::RaydiumLp
        | PoolType::Meteora
        | PoolType::MeteoraDamm
        | PoolType::FluxBeam
        | PoolType::Saros
        | PoolType::Dooar
        | PoolType::PumpFunAmm => {
            estimate_price_impact(reserve_in, reserve_out, amount_in, amount_out)
        }

        // CLMM AMMs: concentrated liquidity — lower effective impact for same reserves
        // We apply a 0.5x scaling factor to the standard impact as an approximation,
        // since concentrated liquidity provides deeper effective reserves around the
        // current price compared to a full-range constant-product pool.
        PoolType::RaydiumCl
        | PoolType::Orca
        | PoolType::PancakeSwap
        | PoolType::Byreal
        | PoolType::DefiTunaFusion => {
            clmm_price_impact(reserve_in, reserve_out, amount_in, amount_out)
        }

        // Bonding curve AMMs: use virtual reserves — same formula applies
        PoolType::PumpFun | PoolType::MeteoraDbc => {
            estimate_price_impact(reserve_in, reserve_out, amount_in, amount_out)
        }

        // Unknown / unsupported: use standard formula as fallback
        _ => estimate_price_impact(reserve_in, reserve_out, amount_in, amount_out),
    }
}

/// CLMM price impact approximation.
///
/// For concentrated liquidity, the effective reserves around the current price
/// are deeper than a full-range pool with the same total reserves. We approximate
/// this by scaling the standard impact by 0.5x (concentrated liquidity provides
/// roughly 2x the effective depth at the current price vs. full-range).
///
/// This is a simplification — true CLMM impact depends on the distribution of
/// liquidity across tick ranges, which we don't model here.
fn clmm_price_impact(
    reserve_in: u128,
    reserve_out: u128,
    amount_in: u64,
    amount_out: u64,
) -> String {
    if reserve_in == 0 || amount_in == 0 || amount_out == 0 {
        return "0.00".to_string();
    }

    let amount_in_128 = amount_in as u128;
    let ideal_out = match amount_in_128.checked_mul(reserve_out) {
        Some(n) => n / reserve_in,
        None => return "0.00".to_string(),
    };

    if ideal_out == 0 {
        return "0.00".to_string();
    }

    let amount_out_128 = amount_out as u128;
    if amount_out_128 >= ideal_out {
        return "0.00".to_string();
    }

    let impact_bps = ((ideal_out - amount_out_128) * 10_000) / ideal_out;

    // Scale down by 50% for CLMM — concentrated liquidity reduces effective impact
    let adjusted_bps = impact_bps / 2;

    let pct_whole = adjusted_bps / 100;
    let pct_frac = adjusted_bps % 100;
    format!("{pct_whole}.{pct_frac:02}")
}

// ---------------------------------------------------------------------------
// P2.3: ExactOut reverse for constant product
// ---------------------------------------------------------------------------

/// Reverse constant product: compute the input amount needed to receive exactly
/// `amount_out` tokens from a constant-product AMM.
///
/// Formula: amount_in = ceil(reserve_in * amount_out * 10000 / ((reserve_out - amount_out) * (10000 - fee_bps)))
///
/// Returns None if reserves are zero, amount_out >= reserve_out, fee is 100%, or overflow.
pub fn compute_constant_product_in(
    reserve_in: u128,
    reserve_out: u128,
    amount_out: u64,
    fee_bps: u16,
) -> Option<u64> {
    if reserve_in == 0 || reserve_out == 0 || amount_out == 0 {
        return None;
    }

    let fee_bps = fee_bps.min(10_000) as u128;
    if fee_bps == 10_000 {
        return None; // 100% fee means no trade possible
    }

    let amount_out = amount_out as u128;
    if amount_out >= reserve_out {
        return None; // Cannot extract more than the reserve
    }

    // numerator = reserve_in * amount_out * 10000
    let numerator = reserve_in
        .checked_mul(amount_out)?
        .checked_mul(10_000)?;

    // denominator = (reserve_out - amount_out) * (10000 - fee_bps)
    let denominator = reserve_out
        .checked_sub(amount_out)?
        .checked_mul(10_000 - fee_bps)?;

    if denominator == 0 {
        return None;
    }

    // Ceiling division: (numerator + denominator - 1) / denominator
    let result = numerator
        .checked_add(denominator - 1)?
        .checked_div(denominator)?;

    if result > u64::MAX as u128 {
        return None;
    }

    Some(result as u64)
}

// ---------------------------------------------------------------------------
// P2.4: Bonding Curve Math
// ---------------------------------------------------------------------------

/// Compute output for a bonding curve swap (PumpFun, Meteora DBC).
///
/// These use constant-product internally but with virtual reserves (which include
/// virtual liquidity) and a flexible fee structure (numerator/denominator rather
/// than basis points).
///
/// Fee is applied before the swap: `amount_after_fee = amount_in * (fee_denominator - fee_numerator) / fee_denominator`
///
/// Then: `out = virtual_reserve_out * amount_after_fee / (virtual_reserve_in + amount_after_fee)`
///
/// Returns None if reserves are zero, amount is zero, fee_denominator is zero,
/// fee_numerator >= fee_denominator, or overflow occurs.
pub fn compute_bonding_curve_out(
    virtual_reserve_in: u128,
    virtual_reserve_out: u128,
    amount_in: u64,
    fee_numerator: u64,
    fee_denominator: u64,
) -> Option<u64> {
    if virtual_reserve_in == 0 || virtual_reserve_out == 0 || amount_in == 0 {
        return None;
    }
    if fee_denominator == 0 {
        return None;
    }

    let fee_num = fee_numerator as u128;
    let fee_den = fee_denominator as u128;

    if fee_num >= fee_den {
        // 100% fee or more — no output possible
        return None;
    }

    let amount_in = amount_in as u128;

    // amount_after_fee = amount_in * (fee_denominator - fee_numerator) / fee_denominator
    let amount_after_fee = amount_in
        .checked_mul(fee_den.checked_sub(fee_num)?)?
        .checked_div(fee_den)?;

    if amount_after_fee == 0 {
        return None;
    }

    // out = virtual_reserve_out * amount_after_fee / (virtual_reserve_in + amount_after_fee)
    let numerator = virtual_reserve_out.checked_mul(amount_after_fee)?;
    let denominator = virtual_reserve_in.checked_add(amount_after_fee)?;

    if denominator == 0 {
        return None;
    }

    let out = numerator.checked_div(denominator)?;

    if out > u64::MAX as u128 {
        return None;
    }

    Some(out as u64)
}

/// Reverse bonding curve: compute required input for a desired output amount.
///
/// Inverse of `compute_bonding_curve_out`:
/// - `pre_fee_in = virtual_reserve_in * amount_out / (virtual_reserve_out - amount_out)`
/// - `amount_in = ceil(pre_fee_in * fee_denominator / (fee_denominator - fee_numerator))`
///
/// Returns None if amount_out >= virtual_reserve_out, zero values, or overflow.
pub fn compute_bonding_curve_in(
    virtual_reserve_in: u128,
    virtual_reserve_out: u128,
    amount_out: u64,
    fee_numerator: u64,
    fee_denominator: u64,
) -> Option<u64> {
    if virtual_reserve_in == 0 || virtual_reserve_out == 0 || amount_out == 0 {
        return None;
    }
    if fee_denominator == 0 {
        return None;
    }

    let fee_num = fee_numerator as u128;
    let fee_den = fee_denominator as u128;

    if fee_num >= fee_den {
        return None; // 100% fee — impossible
    }

    let amount_out = amount_out as u128;

    if amount_out >= virtual_reserve_out {
        return None; // Cannot extract more than virtual reserve
    }

    // pre_fee_in = virtual_reserve_in * amount_out / (virtual_reserve_out - amount_out)
    let numerator = virtual_reserve_in.checked_mul(amount_out)?;
    let denominator = virtual_reserve_out.checked_sub(amount_out)?;

    if denominator == 0 {
        return None;
    }

    // Ceiling division for pre_fee_in
    let pre_fee_in = numerator
        .checked_add(denominator - 1)?
        .checked_div(denominator)?;

    // Undo fee: amount_in = ceil(pre_fee_in * fee_denominator / (fee_denominator - fee_numerator))
    let fee_factor_den = fee_den.checked_sub(fee_num)?;
    if fee_factor_den == 0 {
        return None;
    }

    let amount_in_num = pre_fee_in.checked_mul(fee_den)?;
    let amount_in = amount_in_num
        .checked_add(fee_factor_den - 1)?
        .checked_div(fee_factor_den)?;

    if amount_in > u64::MAX as u128 {
        return None;
    }

    Some(amount_in as u64)
}

// ---------------------------------------------------------------------------
// P1.3: CLMM Single-Tick Approximation
// ---------------------------------------------------------------------------

/// Q64.64 fixed-point constant: 2^64
const Q64: u128 = 1u128 << 64;

/// CLMM single-tick approximation for output amount.
///
/// Valid when the trade stays within the current tick range (true for ~90% of
/// real trades). Within a single tick range the pool behaves like constant
/// product between virtual reserves derived from sqrt_price and liquidity:
///
///   reserve_a = L * Q64 / sqrt_price   (token A, the "x" token)
///   reserve_b = L * sqrt_price / Q64   (token B, the "y" token)
///
/// For `a_to_b` (selling token A for token B):
///   new_reserve_a = reserve_a + amount_after_fee
///   new_reserve_b = reserve_b * reserve_a / new_reserve_a   (constant product)
///   amount_out = reserve_b - new_reserve_b
///
/// For `b_to_a`: swap the roles of reserve_a and reserve_b.
///
/// Parameters:
///   `sqrt_price_x64`: current sqrt price as Q64.64 fixed-point (u128)
///   `liquidity`: current tick range liquidity (u128)
///   `amount_in`: input amount in smallest token units (u64)
///   `fee_bps`: fee in basis points (u16), clamped to 10000
///   `a_to_b`: true if selling token A for token B
pub fn compute_clmm_output(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount_in: u64,
    fee_bps: u16,
    a_to_b: bool,
) -> Option<u64> {
    if sqrt_price_x64 == 0 || liquidity == 0 || amount_in == 0 {
        return None;
    }

    let fee = fee_bps.min(10_000) as u128;
    if fee == 10_000 {
        return None; // 100% fee
    }

    let amount_after_fee = (amount_in as u128)
        .checked_mul(10_000 - fee)?
        .checked_div(10_000)?;
    if amount_after_fee == 0 {
        return None;
    }

    if a_to_b {
        // Selling token A (x), buying token B (y)
        // reserve_a = L * Q64 / sqrt_price
        let reserve_a = liquidity.checked_mul(Q64)?.checked_div(sqrt_price_x64)?;
        // reserve_b = L * sqrt_price / Q64
        let reserve_b = liquidity.checked_mul(sqrt_price_x64)?.checked_div(Q64)?;

        if reserve_a == 0 || reserve_b == 0 {
            return None;
        }

        let new_reserve_a = reserve_a.checked_add(amount_after_fee)?;
        // new_reserve_b = reserve_b * reserve_a / new_reserve_a
        // (equivalent to L^2 / new_reserve_a but avoids L^2 overflow)
        let new_reserve_b = reserve_b.checked_mul(reserve_a)?.checked_div(new_reserve_a)?;

        let out = reserve_b.checked_sub(new_reserve_b)?;
        if out > u64::MAX as u128 {
            return None;
        }
        Some(out as u64)
    } else {
        // Selling token B (y), buying token A (x) — swap reserve roles
        let reserve_a = liquidity.checked_mul(Q64)?.checked_div(sqrt_price_x64)?;
        let reserve_b = liquidity.checked_mul(sqrt_price_x64)?.checked_div(Q64)?;

        if reserve_a == 0 || reserve_b == 0 {
            return None;
        }

        let new_reserve_b = reserve_b.checked_add(amount_after_fee)?;
        let new_reserve_a = reserve_a.checked_mul(reserve_b)?.checked_div(new_reserve_b)?;

        let out = reserve_a.checked_sub(new_reserve_a)?;
        if out > u64::MAX as u128 {
            return None;
        }
        Some(out as u64)
    }
}

/// CLMM reverse: compute the required input for a desired output amount.
///
/// Inverse of `compute_clmm_output`. For `a_to_b` (selling A, buying B):
///   pre_fee_in = reserve_a * amount_out / (reserve_b - amount_out)
///   amount_in = ceil(pre_fee_in * 10000 / (10000 - fee_bps))
///
/// For `b_to_a`: swap reserve roles.
///
/// Returns None if amount_out >= the output reserve, zero values, or overflow.
pub fn compute_clmm_input(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount_out: u64,
    fee_bps: u16,
    a_to_b: bool,
) -> Option<u64> {
    if sqrt_price_x64 == 0 || liquidity == 0 || amount_out == 0 {
        return None;
    }

    let fee = fee_bps.min(10_000) as u128;
    if fee == 10_000 {
        return None; // 100% fee
    }

    let amount_out = amount_out as u128;

    let reserve_a = liquidity.checked_mul(Q64)?.checked_div(sqrt_price_x64)?;
    let reserve_b = liquidity.checked_mul(sqrt_price_x64)?.checked_div(Q64)?;

    if reserve_a == 0 || reserve_b == 0 {
        return None;
    }

    let pre_fee_in = if a_to_b {
        // Selling A, buying B: pre_fee_in = reserve_a * amount_out / (reserve_b - amount_out)
        if amount_out >= reserve_b {
            return None;
        }
        let denom = reserve_b.checked_sub(amount_out)?;
        if denom == 0 {
            return None;
        }
        let num = reserve_a.checked_mul(amount_out)?;
        // Ceiling division
        num.checked_add(denom - 1)?.checked_div(denom)?
    } else {
        // Selling B, buying A: pre_fee_in = reserve_b * amount_out / (reserve_a - amount_out)
        if amount_out >= reserve_a {
            return None;
        }
        let denom = reserve_a.checked_sub(amount_out)?;
        if denom == 0 {
            return None;
        }
        let num = reserve_b.checked_mul(amount_out)?;
        // Ceiling division
        num.checked_add(denom - 1)?.checked_div(denom)?
    };

    // Undo fee: amount_in = ceil(pre_fee_in * 10000 / (10000 - fee_bps))
    let fee_factor_den = 10_000u128.checked_sub(fee)?;
    if fee_factor_den == 0 {
        return None;
    }

    let amount_in = pre_fee_in
        .checked_mul(10_000)?
        .checked_add(fee_factor_den - 1)?
        .checked_div(fee_factor_den)?;

    if amount_in > u64::MAX as u128 {
        return None;
    }

    Some(amount_in as u64)
}

// ---------------------------------------------------------------------------
// P1.3: CLMM Multi-Tick Traversal
// ---------------------------------------------------------------------------

/// Maximum number of tick boundary crossings allowed per multi-tick computation.
/// Beyond this, the trade is extremely large or liquidity is very fragmented.
const MAX_TICK_CROSSINGS: usize = 10;

/// Full CLMM output computation with multi-tick traversal.
///
/// Walks through tick ranges, consuming input amount at each range until the
/// full amount is swapped or we run out of liquidity.
///
/// Within each tick range, the pool behaves like a constant-product AMM with
/// virtual reserves derived from sqrt_price and liquidity:
///   reserve_a = L * Q64 / sqrt_price
///   reserve_b = L * sqrt_price / Q64
///
/// When a trade exhausts the liquidity in one range, it crosses a tick boundary
/// where the liquidity changes (from the `tick_liquidities` array), and the
/// computation continues in the next range.
///
/// Parameters:
///   `sqrt_price_x64`: current sqrt price as Q64.64 fixed-point (u128)
///   `liquidity`: current tick range liquidity (u128)
///   `amount_in`: input amount in smallest token units (u64)
///   `fee_bps`: fee in basis points (u16), clamped to 10000
///   `a_to_b`: true if selling token A for token B
///   `tick_liquidities`: sorted slice of (tick_index, liquidity_for_that_range).
///     Each entry represents a tick boundary and the liquidity active in the
///     range beyond that boundary (in the direction of travel). Must be sorted
///     in the direction of travel (descending ticks for a_to_b, ascending for b_to_a).
///
/// Returns: output amount, or None if insufficient liquidity or overflow.
pub fn compute_clmm_output_multi_tick(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount_in: u64,
    fee_bps: u16,
    a_to_b: bool,
    tick_liquidities: &[(i32, u128)],
) -> Option<u64> {
    if sqrt_price_x64 == 0 || amount_in == 0 {
        return None;
    }

    let fee = fee_bps.min(10_000) as u128;
    if fee == 10_000 {
        return None; // 100% fee
    }

    let amount_after_fee = (amount_in as u128)
        .checked_mul(10_000 - fee)?
        .checked_div(10_000)?;
    if amount_after_fee == 0 {
        return None;
    }

    let mut amount_remaining = amount_after_fee;
    let mut current_sqrt = sqrt_price_x64;
    let mut current_liq = liquidity;
    let mut total_output: u128 = 0;
    let mut crossings = 0;

    while amount_remaining > 0 && crossings <= MAX_TICK_CROSSINGS {
        if current_liq == 0 || current_sqrt == 0 {
            break; // No liquidity in this range
        }

        // Compute virtual reserves for the current tick range
        let (reserve_in, reserve_out) = virtual_reserves(current_sqrt, current_liq, a_to_b)?;

        if reserve_in == 0 || reserve_out == 0 {
            break;
        }

        // Compute how much output we get from swapping amount_remaining in this range.
        // Using constant product: new_reserve_in = reserve_in + amount_remaining
        // new_reserve_out = reserve_out * reserve_in / new_reserve_in
        // output = reserve_out - new_reserve_out
        let new_reserve_in = reserve_in.checked_add(amount_remaining)?;
        let new_reserve_out = reserve_out
            .checked_mul(reserve_in)?
            .checked_div(new_reserve_in)?;
        let output = reserve_out.checked_sub(new_reserve_out)?;

        // Check: does this trade consume more than ~95% of the output reserve?
        // If so, the trade is hitting the tick boundary — split it.
        // In a real CLMM, the price can't move beyond the next tick boundary.
        // We approximate: if output > 90% of reserve_out, we assume tick crossing.
        let boundary_threshold = reserve_out * 9 / 10;

        if output < boundary_threshold || crossings >= tick_liquidities.len() {
            // Trade stays within this range — done
            total_output = total_output.checked_add(output)?;
            amount_remaining = 0;
        } else {
            // Trade crosses tick boundary:
            // Consume as much as possible in this range (up to ~90% of reserve_out)
            // Then move to next tick range with updated liquidity.
            let partial_output = boundary_threshold;
            // Reverse: how much input was consumed to get partial_output?
            // input_consumed = reserve_in * partial_output / (reserve_out - partial_output)
            let denom = reserve_out.checked_sub(partial_output)?;
            if denom == 0 {
                break;
            }
            let input_consumed = reserve_in.checked_mul(partial_output)?.checked_div(denom)?;

            total_output = total_output.checked_add(partial_output)?;
            amount_remaining = amount_remaining.checked_sub(input_consumed.min(amount_remaining))?;

            // Update sqrt_price after consuming this range
            // new_sqrt_price = sqrt(new_k / new_reserve_a) = proportional shift
            // Simplified: use the next tick's liquidity
            if crossings < tick_liquidities.len() {
                current_liq = tick_liquidities[crossings].1;
                // Approximate new sqrt_price: shift proportionally
                // For a_to_b: price decreases, for b_to_a: price increases
                // Use ratio of remaining reserve: new_sqrt = sqrt * (reserve_out - partial) / reserve_out
                if a_to_b {
                    current_sqrt = current_sqrt
                        .checked_mul(denom)?
                        .checked_div(reserve_out)?;
                } else {
                    // b_to_a: price increases — sqrt_price goes up
                    let new_reserve_in_after = reserve_in.checked_add(input_consumed)?;
                    current_sqrt = current_sqrt
                        .checked_mul(new_reserve_in_after)?
                        .checked_div(reserve_in)?;
                }
            } else {
                break; // No more tick data
            }

            crossings += 1;
        }
    }

    if total_output > u64::MAX as u128 {
        return None;
    }
    Some(total_output as u64)
}

/// Compute the virtual reserves for a CLMM tick range.
/// Returns (reserve_in, reserve_out) based on direction.
fn virtual_reserves(sqrt_price_x64: u128, liquidity: u128, a_to_b: bool) -> Option<(u128, u128)> {
    let reserve_a = liquidity.checked_mul(Q64)?.checked_div(sqrt_price_x64)?;
    let reserve_b = liquidity.checked_mul(sqrt_price_x64)?.checked_div(Q64)?;

    if a_to_b {
        // Selling A, buying B: input is A, output is B
        Some((reserve_a, reserve_b))
    } else {
        // Selling B, buying A: input is B, output is A
        Some((reserve_b, reserve_a))
    }
}

/// CLMM multi-tick reverse: compute required input for a desired output amount.
///
/// Uses the single-tick approximation with the current liquidity. For ExactOut
/// across tick boundaries, fall back to the single-tick `compute_clmm_input`.
pub fn compute_clmm_input_multi_tick(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount_out: u64,
    fee_bps: u16,
    a_to_b: bool,
    tick_liquidities: &[(i32, u128)],
) -> Option<u64> {
    // For ExactOut, the multi-tick reverse is complex. Use single-tick for now,
    // which is correct for small-to-medium trades. Large ExactOut trades that
    // would cross ticks are rare in practice.
    let _ = tick_liquidities;
    compute_clmm_input(sqrt_price_x64, liquidity, amount_out, fee_bps, a_to_b)
}

/// Extract CLMM parameters (sqrt_price, liquidity, fee_bps, a_to_b, tick_liquidities)
/// from a PoolState for quoting.
///
/// Returns None for non-CLMM pool types.
pub fn extract_clmm_params(
    state: &crate::pool::types::PoolState,
    input_mint: &solana_sdk::pubkey::Pubkey,
) -> Option<ClmmParams> {
    use crate::pool::types::PoolState;

    match state {
        PoolState::RaydiumClmm {
            token_mint_0,
            token_mint_1,
            sqrt_price_x64,
            liquidity,
            fee_rate,
            ..
        } => {
            let a_to_b = *input_mint == *token_mint_0;
            if *input_mint != *token_mint_0 && *input_mint != *token_mint_1 {
                return None;
            }
            // fee_rate is hundredths of a basis point from the AmmConfig.
            Some(ClmmParams {
                sqrt_price_x64: *sqrt_price_x64,
                liquidity: *liquidity,
                fee_bps: (*fee_rate / 100).max(1),
                fee_ppm: *fee_rate as u32,
                a_to_b,
                tick_liquidities: Vec::new(),
            })
        }
        PoolState::Orca {
            token_mint_a,
            token_mint_b,
            sqrt_price_x64,
            liquidity,
            fee_rate,
            ..
        } => {
            let a_to_b = *input_mint == *token_mint_a;
            if *input_mint != *token_mint_a && *input_mint != *token_mint_b {
                return None;
            }
            // Orca fee_rate is in hundredths of a basis point (1/1_000_000).
            // Convert to basis points: fee_rate / 100
            let fee_bps = (*fee_rate / 100).min(10_000) as u16;
            Some(ClmmParams {
                sqrt_price_x64: *sqrt_price_x64,
                liquidity: *liquidity,
                fee_bps,
                fee_ppm: *fee_rate as u32,
                a_to_b,
                tick_liquidities: Vec::new(),
            })
        }
        PoolState::PancakeSwap {
            token_mint_a,
            token_mint_b,
            sqrt_price_x64,
            liquidity,
            fee_rate,
            ..
        } => {
            let a_to_b = *input_mint == *token_mint_a;
            if *input_mint != *token_mint_a && *input_mint != *token_mint_b {
                return None;
            }
            // fee_rate is hundredths of a basis point from the AmmConfig.
            Some(ClmmParams {
                sqrt_price_x64: *sqrt_price_x64,
                liquidity: *liquidity,
                fee_bps: (*fee_rate / 100).max(1),
                fee_ppm: *fee_rate as u32,
                a_to_b,
                tick_liquidities: Vec::new(),
            })
        }
        PoolState::Byreal {
            token_mint_a,
            token_mint_b,
            sqrt_price_x64,
            liquidity,
            fee_rate,
            fee,
            ..
        } => {
            let a_to_b = *input_mint == *token_mint_a;
            if *input_mint != *token_mint_a && *input_mint != *token_mint_b {
                return None;
            }
            // Raydium CLMM fork: the AmmConfig rate (hundredths of a basis
            // point), unless the pool overrides it or charges a launch decay
            // fee. A dynamic-fee pool adds a per-swap term on top of this
            // base (`quote::byreal_fee::swap_fee_ppm`).
            let now = crate::stream::chain_unix_time();
            if !fee.can_swap(now) {
                return None;
            }
            let fee_ppm = fee.base_fee_rate(*fee_rate as u32, a_to_b, now)?;
            Some(ClmmParams {
                sqrt_price_x64: *sqrt_price_x64,
                liquidity: *liquidity,
                fee_bps: (fee_ppm / 100).clamp(1, 10_000) as u16,
                fee_ppm,
                a_to_b,
                tick_liquidities: Vec::new(),
            })
        }
        PoolState::DefiTunaFusion {
            token_mint_a,
            token_mint_b,
            sqrt_price_x64,
            liquidity,
            fee_rate,
            ..
        } => {
            let a_to_b = *input_mint == *token_mint_a;
            if *input_mint != *token_mint_a && *input_mint != *token_mint_b {
                return None;
            }
            // DefiTuna fee_rate is in hundredths of a basis point
            let fee_bps = (*fee_rate / 100).min(10_000) as u16;
            Some(ClmmParams {
                sqrt_price_x64: *sqrt_price_x64,
                liquidity: *liquidity,
                fee_bps,
                fee_ppm: *fee_rate as u32,
                a_to_b,
                tick_liquidities: Vec::new(),
            })
        }
        _ => None,
    }
}

/// Parameters extracted from a CLMM pool state for quoting.
#[derive(Debug, Clone)]
pub struct ClmmParams {
    pub sqrt_price_x64: u128,
    pub liquidity: u128,
    /// Whole basis points (legacy single-range math and fee reporting).
    pub fee_bps: u16,
    /// Exact fee in parts per million, as the program applies it.
    pub fee_ppm: u32,
    pub a_to_b: bool,
    pub tick_liquidities: Vec<(i32, u128)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_product_basic_no_fee() {
        // Pool: 1000 SOL / 100000 USDC
        // Swap 1 SOL in, 0 fee
        // out = 100000 * 1 * 10000 / (1000 * 10000 + 1 * 10000)
        //     = 1000000000 / 10010000 = 99.9000... = 99
        let out = compute_constant_product_out(1000, 100_000, 1, 0).unwrap();
        assert_eq!(out, 99);
    }

    #[test]
    fn test_constant_product_with_fee() {
        // Pool: 1000 SOL / 100000 USDC, 25 bps fee (0.25%)
        // amount_in_after_fee = 1 * (10000 - 25) = 9975
        // out = 100000 * 9975 / (1000 * 10000 + 9975) = 997500000 / 10009975 = 99
        let out = compute_constant_product_out(1000, 100_000, 1, 25).unwrap();
        assert_eq!(out, 99);
    }

    #[test]
    fn test_constant_product_larger_swap() {
        // Pool: 1_000_000 / 1_000_000 (1:1), swap 10_000 in, 30 bps
        // amount_in_after_fee = 10000 * 9970 = 99700000
        // out = 1_000_000 * 99700000 / (1_000_000 * 10000 + 99700000)
        //     = 99_700_000_000_000 / 10_099_700_000 = 9871
        let out = compute_constant_product_out(1_000_000, 1_000_000, 10_000, 30).unwrap();
        assert_eq!(out, 9871);
    }

    #[test]
    fn test_constant_product_zero_input() {
        assert!(compute_constant_product_out(1000, 1000, 0, 25).is_none());
    }

    #[test]
    fn test_constant_product_zero_reserve_in() {
        assert!(compute_constant_product_out(0, 1000, 100, 25).is_none());
    }

    #[test]
    fn test_constant_product_zero_reserve_out() {
        assert!(compute_constant_product_out(1000, 0, 100, 25).is_none());
    }

    #[test]
    fn test_constant_product_large_amounts() {
        // Realistic: 50000 SOL (in lamports) vs 8M USDC (in micro-units)
        let reserve_sol: u128 = 50_000 * 1_000_000_000; // 50k SOL in lamports
        let reserve_usdc: u128 = 8_000_000 * 1_000_000; // 8M USDC in micro-units
        let amount_in: u64 = 1_000_000_000; // 1 SOL in lamports

        let out = compute_constant_product_out(reserve_sol, reserve_usdc, amount_in, 25).unwrap();
        // Expected: ~159.84 USDC (price = 160 USDC/SOL with ~0.1% impact + fee)
        assert!(out > 159_000_000, "out={out} should be > 159M");
        assert!(out < 160_000_000, "out={out} should be < 160M");
    }

    #[test]
    fn test_constant_product_max_fee() {
        // 100% fee -> 0 output
        let out = compute_constant_product_out(1000, 1000, 100, 10_000).unwrap_or(0);
        assert_eq!(out, 0);
    }

    #[test]
    fn test_constant_product_fee_clamped() {
        // Fee > 10000 should be clamped to 10000
        let out = compute_constant_product_out(1000, 1000, 100, 15_000).unwrap_or(0);
        assert_eq!(out, 0);
    }

    #[test]
    fn test_fee_amount_basic() {
        // 1000 amount, 25 bps -> 2.5, truncated to 2
        assert_eq!(compute_fee_amount(1000, 25), 2);
    }

    #[test]
    fn test_fee_amount_zero() {
        assert_eq!(compute_fee_amount(1000, 0), 0);
    }

    #[test]
    fn test_fee_amount_100_percent() {
        assert_eq!(compute_fee_amount(1000, 10_000), 1000);
    }

    #[test]
    fn test_price_impact_small() {
        // Very small swap relative to pool (0.001% of reserves)
        let impact = estimate_price_impact(10_000_000, 10_000_000, 100, 99);
        // Impact should be small (< 2%)
        let val: f64 = impact.parse().unwrap();
        assert!(val < 2.0, "impact={impact}");
        assert!(val >= 0.0, "impact should be non-negative");
    }

    #[test]
    fn test_price_impact_large() {
        // Large swap: 10% of pool
        let impact = estimate_price_impact(1_000_000, 1_000_000, 100_000, 90_910);
        let val: f64 = impact.parse().unwrap();
        assert!(val > 0.0, "impact should be > 0, got {impact}");
    }

    #[test]
    fn test_price_impact_zero_reserves() {
        assert_eq!(estimate_price_impact(0, 1000, 100, 99), "0.00");
    }

    #[test]
    fn test_price_impact_zero_amount() {
        assert_eq!(estimate_price_impact(1000, 1000, 0, 0), "0.00");
    }

    #[test]
    fn test_constant_product_symmetry() {
        // Swapping same amount back should give less (due to price impact)
        let out1 = compute_constant_product_out(10_000, 10_000, 1000, 0).unwrap();
        let out2 = compute_constant_product_out(10_000 + 1000, (10_000u128).saturating_sub(out1 as u128), out1, 0).unwrap();
        // out2 should be less than original 1000 due to price impact
        assert!(out2 < 1000, "round-trip should lose value: {out2}");
    }

    // -----------------------------------------------------------------------
    // P2.3: compute_constant_product_in tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cp_in_basic_no_fee() {
        // Forward: pool 1000/100_000, swap 10 in, 0 fee
        let out = compute_constant_product_out(1000, 100_000, 10, 0).unwrap();
        // Reverse: how much input to get that exact output?
        let needed = compute_constant_product_in(1000, 100_000, out, 0).unwrap();
        // Should be <= original input (ceiling rounds up, but forward truncates down)
        assert!(needed <= 10, "needed={needed} should be <= 10");
        // And the forward of 'needed' should give at least 'out'
        let check = compute_constant_product_out(1000, 100_000, needed, 0).unwrap();
        assert!(check >= out, "check={check} should be >= out={out}");
    }

    #[test]
    fn test_cp_in_with_fee() {
        // Forward: pool 10_000/10_000, swap 500 in, 30 bps
        let out = compute_constant_product_out(10_000, 10_000, 500, 30).unwrap();
        assert!(out > 0);
        let needed = compute_constant_product_in(10_000, 10_000, out, 30).unwrap();
        // Reverse should give back roughly the original
        assert!(needed <= 500, "needed={needed} should be <= 500");
        // Forward of 'needed' should produce at least 'out'
        let check = compute_constant_product_out(10_000, 10_000, needed, 30).unwrap();
        assert!(check >= out, "check={check} should be >= out={out}");
    }

    #[test]
    fn test_cp_in_round_trip_exact() {
        // Large pool, small trade — should round-trip within 1 lamport
        let reserve_in: u128 = 1_000_000_000;
        let reserve_out: u128 = 1_000_000_000;
        let amount_in: u64 = 1000;
        let out = compute_constant_product_out(reserve_in, reserve_out, amount_in, 25).unwrap();
        let needed = compute_constant_product_in(reserve_in, reserve_out, out, 25).unwrap();
        let diff = if needed > amount_in { needed - amount_in } else { amount_in - needed };
        assert!(diff <= 1, "round-trip diff={diff} should be <= 1");
    }

    #[test]
    fn test_cp_in_amount_out_exceeds_reserve() {
        // amount_out >= reserve_out should return None
        assert!(compute_constant_product_in(1000, 1000, 1000, 0).is_none());
        assert!(compute_constant_product_in(1000, 1000, 1001, 0).is_none());
    }

    #[test]
    fn test_cp_in_zero_values() {
        assert!(compute_constant_product_in(0, 1000, 100, 0).is_none());
        assert!(compute_constant_product_in(1000, 0, 100, 0).is_none());
        assert!(compute_constant_product_in(1000, 1000, 0, 0).is_none());
    }

    #[test]
    fn test_cp_in_100_percent_fee() {
        assert!(compute_constant_product_in(1000, 1000, 500, 10_000).is_none());
    }

    // -----------------------------------------------------------------------
    // P2.4: Bonding curve tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_bonding_curve_out_basic() {
        // Virtual reserves: 30B SOL virtual (in lamports) + 1B token virtual
        // PumpFun-like: fee 1/100 (1%)
        let v_sol: u128 = 30_000_000_000_000; // 30k SOL in lamports
        let v_token: u128 = 1_000_000_000_000_000; // 1B tokens (6 decimals)
        let amount_in: u64 = 1_000_000_000; // 1 SOL

        let out = compute_bonding_curve_out(v_sol, v_token, amount_in, 1, 100).unwrap();
        // After 1% fee: 0.99 SOL effective
        // out = v_token * 0.99_SOL / (v_sol + 0.99_SOL)
        // ~= 1e15 * 990_000_000 / (30e12 + 990_000_000) ~= 32,894,736,842
        assert!(out > 0, "output should be positive");
        assert!(out < v_token as u64, "output should be less than virtual reserve");
    }

    #[test]
    fn test_bonding_curve_out_zero_input() {
        assert!(compute_bonding_curve_out(1000, 1000, 0, 1, 100).is_none());
    }

    #[test]
    fn test_bonding_curve_out_zero_reserves() {
        assert!(compute_bonding_curve_out(0, 1000, 100, 1, 100).is_none());
        assert!(compute_bonding_curve_out(1000, 0, 100, 1, 100).is_none());
    }

    #[test]
    fn test_bonding_curve_out_no_fee() {
        // fee_numerator = 0 means no fee
        let out_no_fee = compute_bonding_curve_out(10_000, 10_000, 1000, 0, 100).unwrap();
        let out_with_fee = compute_bonding_curve_out(10_000, 10_000, 1000, 1, 100).unwrap();
        assert!(out_no_fee > out_with_fee, "no-fee output should be larger");

        // No-fee should match constant product with 0 bps
        let cp_out = compute_constant_product_out(10_000, 10_000, 1000, 0).unwrap();
        assert_eq!(out_no_fee, cp_out, "no-fee bonding curve should match constant product");
    }

    #[test]
    fn test_bonding_curve_out_100_percent_fee() {
        // fee_numerator == fee_denominator => 100% fee => None
        assert!(compute_bonding_curve_out(1000, 1000, 100, 100, 100).is_none());
        // fee_numerator > fee_denominator => also None
        assert!(compute_bonding_curve_out(1000, 1000, 100, 101, 100).is_none());
    }

    #[test]
    fn test_bonding_curve_out_fee_denominator_zero() {
        assert!(compute_bonding_curve_out(1000, 1000, 100, 1, 0).is_none());
    }

    #[test]
    fn test_bonding_curve_out_large_amounts() {
        // Large virtual reserves (realistic PumpFun scale)
        let v_in: u128 = 30_000_000_000_000; // 30k SOL
        let v_out: u128 = 1_000_000_000_000_000_000; // 1B tokens (9 decimals)
        let amount_in: u64 = u64::MAX / 2;

        let result = compute_bonding_curve_out(v_in, v_out, amount_in, 1, 100);
        // Should not overflow, should return Some
        assert!(result.is_some(), "large amount should not overflow");
        let out = result.unwrap();
        assert!(out > 0);
    }

    #[test]
    fn test_bonding_curve_in_basic() {
        // Forward then reverse: should approximately round-trip
        let v_in: u128 = 100_000;
        let v_out: u128 = 100_000;
        let amount_in: u64 = 5000;
        let fee_num: u64 = 1;
        let fee_den: u64 = 100;

        let out = compute_bonding_curve_out(v_in, v_out, amount_in, fee_num, fee_den).unwrap();
        assert!(out > 0);

        let needed = compute_bonding_curve_in(v_in, v_out, out, fee_num, fee_den).unwrap();
        // needed should be <= original amount_in
        assert!(needed <= amount_in, "needed={needed} should be <= {amount_in}");
        // Forward of 'needed' should produce at least 'out'
        let check = compute_bonding_curve_out(v_in, v_out, needed, fee_num, fee_den).unwrap();
        assert!(check >= out, "check={check} should be >= out={out}");
    }

    #[test]
    fn test_bonding_curve_in_zero_values() {
        assert!(compute_bonding_curve_in(0, 1000, 100, 1, 100).is_none());
        assert!(compute_bonding_curve_in(1000, 0, 100, 1, 100).is_none());
        assert!(compute_bonding_curve_in(1000, 1000, 0, 1, 100).is_none());
    }

    #[test]
    fn test_bonding_curve_in_amount_out_exceeds_reserve() {
        assert!(compute_bonding_curve_in(1000, 1000, 1000, 1, 100).is_none());
        assert!(compute_bonding_curve_in(1000, 1000, 1001, 1, 100).is_none());
    }

    #[test]
    fn test_bonding_curve_in_fee_denominator_zero() {
        assert!(compute_bonding_curve_in(1000, 1000, 500, 1, 0).is_none());
    }

    #[test]
    fn test_bonding_curve_in_100_percent_fee() {
        assert!(compute_bonding_curve_in(1000, 1000, 500, 100, 100).is_none());
    }

    #[test]
    fn test_bonding_curve_round_trip_large_pool() {
        // Large pool, small trade — round-trip should be tight
        let v_in: u128 = 1_000_000_000_000;
        let v_out: u128 = 1_000_000_000_000;
        let amount_in: u64 = 1_000_000;
        let fee_num: u64 = 25;
        let fee_den: u64 = 10_000;

        let out = compute_bonding_curve_out(v_in, v_out, amount_in, fee_num, fee_den).unwrap();
        let needed = compute_bonding_curve_in(v_in, v_out, out, fee_num, fee_den).unwrap();
        let diff = if needed > amount_in { needed - amount_in } else { amount_in - needed };
        assert!(diff <= 1, "round-trip diff={diff} should be <= 1 lamport");
    }

    // -----------------------------------------------------------------------
    // P1.3: CLMM single-tick tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_clmm_output_basic_a_to_b() {
        // Moderate liquidity pool, sqrt_price ~= 1.0 in Q64.64
        // sqrt_price_x64 = 1.0 * 2^64 = 18446744073709551616
        let sqrt_price_x64: u128 = Q64; // price = 1.0
        let liquidity: u128 = 1_000_000_000; // L = 1B
        let amount_in: u64 = 1_000_000; // 1M tokens
        let fee_bps: u16 = 30; // 0.3%

        let out = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, fee_bps, true).unwrap();
        assert!(out > 0, "output should be positive");
        // With price=1.0 and small trade relative to L, output ~= amount_in * (1 - fee)
        assert!(out < amount_in, "output should be less than input due to fee + impact");
        // Should be close to 997_000 (= 1M * 0.997) for a very deep pool
        assert!(out > 900_000, "out={out} should be > 900K for deep pool");
    }

    #[test]
    fn test_clmm_output_basic_b_to_a() {
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 1_000_000_000;
        let amount_in: u64 = 1_000_000;
        let fee_bps: u16 = 30;

        let out = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, fee_bps, false).unwrap();
        assert!(out > 0);
        assert!(out < amount_in);
        // With symmetric price=1.0, a_to_b and b_to_a should give same result
        let out_a = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, fee_bps, true).unwrap();
        assert_eq!(out, out_a, "symmetric pool should give same output either direction");
    }

    #[test]
    fn test_clmm_output_zero_liquidity() {
        assert!(compute_clmm_output(Q64, 0, 1000, 30, true).is_none());
    }

    #[test]
    fn test_clmm_output_zero_sqrt_price() {
        assert!(compute_clmm_output(0, 1_000_000, 1000, 30, true).is_none());
    }

    #[test]
    fn test_clmm_output_zero_amount() {
        assert!(compute_clmm_output(Q64, 1_000_000, 0, 30, true).is_none());
    }

    #[test]
    fn test_clmm_output_100_percent_fee() {
        assert!(compute_clmm_output(Q64, 1_000_000, 1000, 10_000, true).is_none());
    }

    #[test]
    fn test_clmm_output_large_liquidity() {
        // Very deep pool — output should be very close to amount_in * (1 - fee)
        let sqrt_price_x64: u128 = Q64; // price = 1.0
        let liquidity: u128 = 1_000_000_000_000_000; // huge L
        let amount_in: u64 = 1_000_000;
        let fee_bps: u16 = 30;

        let out = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, fee_bps, true).unwrap();
        // With huge L, price impact is negligible — output ~= 997_000
        let expected_no_impact = (amount_in as u128) * (10_000 - 30) / 10_000;
        let diff = if out as u128 > expected_no_impact {
            out as u128 - expected_no_impact
        } else {
            expected_no_impact - out as u128
        };
        // Should be within 0.1% of fee-only output
        assert!(
            diff < expected_no_impact / 1000,
            "out={out} should be close to {expected_no_impact}, diff={diff}"
        );
    }

    #[test]
    fn test_clmm_output_fee_comparison() {
        // Higher fee should give less output
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 1_000_000_000;
        let amount_in: u64 = 1_000_000;

        let out_low_fee = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, 10, true).unwrap();
        let out_high_fee = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, 100, true).unwrap();
        assert!(out_low_fee > out_high_fee, "low fee={out_low_fee} should be > high fee={out_high_fee}");
    }

    #[test]
    fn test_clmm_output_no_fee() {
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 1_000_000_000;
        let amount_in: u64 = 1_000;

        let out_no_fee = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, 0, true).unwrap();
        let out_with_fee = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, 30, true).unwrap();
        assert!(out_no_fee > out_with_fee, "no fee should produce more output");
    }

    #[test]
    fn test_clmm_output_asymmetric_price() {
        // sqrt_price > 1.0 means token B is more expensive relative to A
        // sqrt_price = 2.0 * Q64 means price = 4.0 (B per A)
        let sqrt_price_x64: u128 = 2 * Q64;
        let liquidity: u128 = 1_000_000_000;
        let amount_in: u64 = 1_000;

        let out_a_to_b = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, 0, true).unwrap();
        let out_b_to_a = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, 0, false).unwrap();

        // With price=4.0, selling A should give ~4x output in B
        // And selling B should give ~0.25x output in A
        // So a_to_b output > b_to_a output (for same input amount)
        assert!(out_a_to_b > out_b_to_a, "a_to_b={out_a_to_b} should be > b_to_a={out_b_to_a} at sqrt_price=2*Q64");
    }

    #[test]
    fn test_clmm_round_trip_symmetry() {
        // Swap A→B then B→A on the same pool (updating reserves conceptually)
        // The round-trip should lose value due to fee + price impact
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 1_000_000_000;
        let amount_in: u64 = 100_000;

        let out_ab = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, 30, true).unwrap();
        // Now swap out_ab back (b_to_a) on same pool
        let out_ba = compute_clmm_output(sqrt_price_x64, liquidity, out_ab, 30, false).unwrap();
        // Should get less than original due to fees + impact
        assert!(out_ba < amount_in, "round-trip should lose value: got {out_ba} from {amount_in}");
    }

    // -----------------------------------------------------------------------
    // P1.3: CLMM input (reverse) tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_clmm_input_basic_a_to_b() {
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 1_000_000_000;
        let amount_in: u64 = 1_000_000;
        let fee_bps: u16 = 30;

        // Forward
        let out = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, fee_bps, true).unwrap();
        // Reverse
        let needed = compute_clmm_input(sqrt_price_x64, liquidity, out, fee_bps, true).unwrap();
        // Ceiling rounding means needed may be slightly above original amount_in,
        // but should be very close (within a few units)
        let diff = if needed > amount_in { needed - amount_in } else { amount_in - needed };
        assert!(diff <= 3, "round-trip diff={diff} should be small (needed={needed}, original={amount_in})");
        // Forward of 'needed' should produce at least 'out'
        let check = compute_clmm_output(sqrt_price_x64, liquidity, needed, fee_bps, true).unwrap();
        assert!(check >= out, "check={check} should be >= out={out}");
    }

    #[test]
    fn test_clmm_input_basic_b_to_a() {
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 1_000_000_000;
        let amount_in: u64 = 500_000;
        let fee_bps: u16 = 30;

        let out = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, fee_bps, false).unwrap();
        let needed = compute_clmm_input(sqrt_price_x64, liquidity, out, fee_bps, false).unwrap();
        let diff = if needed > amount_in { needed - amount_in } else { amount_in - needed };
        assert!(diff <= 3, "round-trip diff={diff} should be small (needed={needed}, original={amount_in})");
        let check = compute_clmm_output(sqrt_price_x64, liquidity, needed, fee_bps, false).unwrap();
        assert!(check >= out, "check={check} should be >= out={out}");
    }

    #[test]
    fn test_clmm_input_zero_values() {
        assert!(compute_clmm_input(0, 1_000_000, 1000, 30, true).is_none());
        assert!(compute_clmm_input(Q64, 0, 1000, 30, true).is_none());
        assert!(compute_clmm_input(Q64, 1_000_000, 0, 30, true).is_none());
    }

    #[test]
    fn test_clmm_input_100_percent_fee() {
        assert!(compute_clmm_input(Q64, 1_000_000, 1000, 10_000, true).is_none());
    }

    #[test]
    fn test_clmm_input_amount_out_exceeds_reserve() {
        // With sqrt_price=Q64, L=1000: reserve_a = 1000, reserve_b = 1000
        // Requesting 1000 (= reserve_b) should fail
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 1000;
        assert!(compute_clmm_input(sqrt_price_x64, liquidity, 1000, 0, true).is_none());
        assert!(compute_clmm_input(sqrt_price_x64, liquidity, 1001, 0, true).is_none());
    }

    #[test]
    fn test_clmm_input_round_trip_tight() {
        // Large L, small trade — round-trip should be very close
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 1_000_000_000_000;
        let amount_in: u64 = 1_000_000;
        let fee_bps: u16 = 30;

        let out = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, fee_bps, true).unwrap();
        let needed = compute_clmm_input(sqrt_price_x64, liquidity, out, fee_bps, true).unwrap();
        let diff = if needed > amount_in { needed - amount_in } else { amount_in - needed };
        // Multiple ceiling divisions compound rounding — allow small tolerance
        assert!(diff <= 3, "round-trip diff={diff} should be <= 3 for deep pool (needed={needed}, original={amount_in})");
    }

    #[test]
    fn test_clmm_input_no_fee() {
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 1_000_000_000;
        let amount_in: u64 = 100_000;

        let out = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, 0, true).unwrap();
        let needed = compute_clmm_input(sqrt_price_x64, liquidity, out, 0, true).unwrap();
        let diff = if needed > amount_in { needed - amount_in } else { amount_in - needed };
        assert!(diff <= 2, "round-trip diff={diff} should be small (needed={needed}, original={amount_in})");
        // Forward of 'needed' should produce at least 'out'
        let check = compute_clmm_output(sqrt_price_x64, liquidity, needed, 0, true).unwrap();
        assert!(check >= out, "check={check} should be >= out={out}");
    }

    // -----------------------------------------------------------------------
    // P2.2: Per-AMM price impact tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_per_amm_impact_constant_product() {
        use crate::pool::types::PoolType;

        let reserve = 1_000_000u128;
        let amount_in = 100_000u64;
        let out = compute_constant_product_out(reserve, reserve, amount_in, 25).unwrap();

        // Constant-product AMMs should use the standard formula
        let impact_cpmm = compute_price_impact_for_type(
            PoolType::RaydiumCpmm, amount_in, out, reserve, reserve,
        );
        let impact_generic = estimate_price_impact(reserve, reserve, amount_in, out);
        assert_eq!(impact_cpmm, impact_generic);

        // Meteora should also use standard formula
        let impact_meteora = compute_price_impact_for_type(
            PoolType::Meteora, amount_in, out, reserve, reserve,
        );
        assert_eq!(impact_meteora, impact_generic);
    }

    #[test]
    fn test_per_amm_impact_clmm_lower_than_cp() {
        use crate::pool::types::PoolType;

        let reserve = 1_000_000u128;
        let amount_in = 100_000u64;
        let out = compute_constant_product_out(reserve, reserve, amount_in, 25).unwrap();

        let impact_cp = compute_price_impact_for_type(
            PoolType::RaydiumCpmm, amount_in, out, reserve, reserve,
        );
        let impact_clmm = compute_price_impact_for_type(
            PoolType::Orca, amount_in, out, reserve, reserve,
        );

        let cp_val: f64 = impact_cp.parse().unwrap();
        let clmm_val: f64 = impact_clmm.parse().unwrap();

        // CLMM impact should be lower (approximately half) of CP impact
        assert!(
            clmm_val < cp_val,
            "CLMM impact ({clmm_val}) should be < CP impact ({cp_val})"
        );
        // Should be approximately half
        assert!(
            clmm_val <= cp_val / 2.0 + 0.01,
            "CLMM impact should be approximately half of CP"
        );
    }

    #[test]
    fn test_per_amm_impact_bonding_curve_same_as_cp() {
        use crate::pool::types::PoolType;

        let reserve = 1_000_000u128;
        let amount_in = 100_000u64;
        let out = compute_constant_product_out(reserve, reserve, amount_in, 25).unwrap();

        let impact_cp = compute_price_impact_for_type(
            PoolType::RaydiumCpmm, amount_in, out, reserve, reserve,
        );
        let impact_pumpfun = compute_price_impact_for_type(
            PoolType::PumpFun, amount_in, out, reserve, reserve,
        );

        // Bonding curve uses same formula as constant product
        assert_eq!(impact_cp, impact_pumpfun);
    }

    #[test]
    fn test_per_amm_impact_zero_reserves() {
        use crate::pool::types::PoolType;

        let impact = compute_price_impact_for_type(PoolType::Orca, 1000, 999, 0, 1_000_000);
        assert_eq!(impact, "0.00");

        let impact = compute_price_impact_for_type(PoolType::RaydiumCpmm, 0, 0, 1_000_000, 1_000_000);
        assert_eq!(impact, "0.00");
    }

    #[test]
    fn test_per_amm_impact_all_clmm_variants_consistent() {
        use crate::pool::types::PoolType;

        let reserve = 1_000_000u128;
        let amount_in = 50_000u64;
        let out = compute_constant_product_out(reserve, reserve, amount_in, 25).unwrap();

        let impact_orca = compute_price_impact_for_type(
            PoolType::Orca, amount_in, out, reserve, reserve,
        );
        let impact_raydium_cl = compute_price_impact_for_type(
            PoolType::RaydiumCl, amount_in, out, reserve, reserve,
        );
        let impact_pancake = compute_price_impact_for_type(
            PoolType::PancakeSwap, amount_in, out, reserve, reserve,
        );

        // All CLMM variants should give the same impact
        assert_eq!(impact_orca, impact_raydium_cl, "Orca and RaydiumCl should match");
        assert_eq!(impact_orca, impact_pancake, "Orca and PancakeSwap should match");
    }

    #[test]
    fn test_per_amm_impact_all_cp_variants_consistent() {
        use crate::pool::types::PoolType;

        let reserve = 1_000_000u128;
        let amount_in = 50_000u64;
        let out = compute_constant_product_out(reserve, reserve, amount_in, 25).unwrap();

        let cp_types = [
            PoolType::RaydiumV4,
            PoolType::RaydiumCpmm,
            PoolType::RaydiumLp,
            PoolType::Meteora,
            PoolType::MeteoraDamm,
            PoolType::FluxBeam,
            PoolType::Saros,
            PoolType::Dooar,
            PoolType::PumpFunAmm,
        ];

        let expected = compute_price_impact_for_type(
            PoolType::RaydiumCpmm, amount_in, out, reserve, reserve,
        );

        for pt in &cp_types {
            let impact = compute_price_impact_for_type(*pt, amount_in, out, reserve, reserve);
            assert_eq!(
                impact, expected,
                "{:?} impact ({impact}) should match RaydiumCpmm ({expected})",
                pt
            );
        }
    }

    #[test]
    fn test_clmm_price_impact_fn_directly() {
        // Test the internal clmm_price_impact function
        let reserve = 1_000_000u128;
        let amount_in = 100_000u64;
        let out = compute_constant_product_out(reserve, reserve, amount_in, 25).unwrap();

        let impact = clmm_price_impact(reserve, reserve, amount_in, out);
        let val: f64 = impact.parse().unwrap();
        assert!(val >= 0.0, "impact should be non-negative");

        // Compare with standard
        let standard = estimate_price_impact(reserve, reserve, amount_in, out);
        let std_val: f64 = standard.parse().unwrap();
        assert!(val < std_val, "CLMM impact should be less than standard");
    }

    #[test]
    fn test_clmm_price_impact_zero_values() {
        assert_eq!(clmm_price_impact(0, 1000, 100, 99), "0.00");
        assert_eq!(clmm_price_impact(1000, 1000, 0, 0), "0.00");
        assert_eq!(clmm_price_impact(1000, 1000, 100, 0), "0.00");
    }

    #[test]
    fn test_per_amm_impact_unknown_type_uses_standard() {
        use crate::pool::types::PoolType;

        let reserve = 1_000_000u128;
        let amount_in = 50_000u64;
        let out = compute_constant_product_out(reserve, reserve, amount_in, 25).unwrap();

        // Unknown / unsupported pool type falls back to standard
        let impact_unknown = compute_price_impact_for_type(
            PoolType::Unknown, amount_in, out, reserve, reserve,
        );
        let impact_standard = estimate_price_impact(reserve, reserve, amount_in, out);
        assert_eq!(impact_unknown, impact_standard);
    }

    // ── CLMM Multi-Tick Tests ──

    #[test]
    fn test_clmm_multi_tick_single_range_matches_single_tick() {
        // When a trade fits within one range, multi-tick should give the same result
        // as single-tick (no tick crossings needed).
        let sqrt_price_x64: u128 = Q64; // price = 1.0
        let liquidity: u128 = 1_000_000_000;
        let amount_in: u64 = 1_000;
        let fee_bps: u16 = 30;

        let single = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, fee_bps, true).unwrap();
        let multi = compute_clmm_output_multi_tick(sqrt_price_x64, liquidity, amount_in, fee_bps, true, &[]).unwrap();

        assert_eq!(single, multi, "single-tick and multi-tick should match for small trades");
    }

    #[test]
    fn test_clmm_multi_tick_b_to_a_matches_single_tick() {
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 1_000_000_000;
        let amount_in: u64 = 1_000;
        let fee_bps: u16 = 30;

        let single = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, fee_bps, false).unwrap();
        let multi = compute_clmm_output_multi_tick(sqrt_price_x64, liquidity, amount_in, fee_bps, false, &[]).unwrap();

        assert_eq!(single, multi);
    }

    #[test]
    fn test_clmm_multi_tick_large_trade_with_ticks() {
        // Large trade that would cross tick boundaries
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 100_000; // Small liquidity to force crossing
        let amount_in: u64 = 500_000; // Large relative to reserves
        let fee_bps: u16 = 30;

        // Provide additional liquidity at subsequent ticks
        let tick_liq: Vec<(i32, u128)> = vec![
            (-100, 200_000),
            (-200, 150_000),
            (-300, 100_000),
        ];

        let result = compute_clmm_output_multi_tick(
            sqrt_price_x64, liquidity, amount_in, fee_bps, true, &tick_liq,
        );
        assert!(result.is_some(), "large trade with tick data should produce output");
        let out = result.unwrap();
        assert!(out > 0, "output should be positive");

        // Multi-tick output should be >= single tick when extra liquidity is available
        // (single tick would have more price impact)
        let single = compute_clmm_output(sqrt_price_x64, liquidity, amount_in, fee_bps, true);
        if let Some(single_out) = single {
            assert!(out >= single_out, "multi-tick out={out} should be >= single_tick out={single_out}");
        }
    }

    #[test]
    fn test_clmm_multi_tick_zero_liquidity_at_next_tick() {
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 10_000; // Small
        let amount_in: u64 = 100_000; // Very large relative to reserves
        let fee_bps: u16 = 0;

        // Next tick has zero liquidity — should return partial output
        let tick_liq: Vec<(i32, u128)> = vec![
            (-100, 0), // No liquidity at next tick
        ];

        let result = compute_clmm_output_multi_tick(
            sqrt_price_x64, liquidity, amount_in, fee_bps, true, &tick_liq,
        );
        assert!(result.is_some(), "should return partial output when liquidity runs out");
        let out = result.unwrap();
        assert!(out > 0, "partial output should be > 0");
    }

    #[test]
    fn test_clmm_multi_tick_max_crossings_respected() {
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 1_000; // Very small
        let amount_in: u64 = 1_000_000; // Huge relative to reserves
        let fee_bps: u16 = 0;

        // Provide many ticks with small liquidity
        let tick_liq: Vec<(i32, u128)> = (0..20)
            .map(|i| (-(i as i32 + 1) * 100, 1_000u128))
            .collect();

        let result = compute_clmm_output_multi_tick(
            sqrt_price_x64, liquidity, amount_in, fee_bps, true, &tick_liq,
        );
        assert!(result.is_some(), "should produce output even with many crossings");
    }

    #[test]
    fn test_clmm_multi_tick_zero_input() {
        assert!(compute_clmm_output_multi_tick(Q64, 1_000_000, 0, 30, true, &[]).is_none());
    }

    #[test]
    fn test_clmm_multi_tick_zero_sqrt_price() {
        assert!(compute_clmm_output_multi_tick(0, 1_000_000, 1000, 30, true, &[]).is_none());
    }

    #[test]
    fn test_clmm_multi_tick_100_percent_fee() {
        assert!(compute_clmm_output_multi_tick(Q64, 1_000_000, 1000, 10_000, true, &[]).is_none());
    }

    #[test]
    fn test_clmm_multi_tick_round_trip_within_range() {
        let sqrt_price_x64: u128 = Q64;
        let liquidity: u128 = 1_000_000_000;
        let amount_in: u64 = 100_000;
        let fee_bps: u16 = 30;

        let out_ab = compute_clmm_output_multi_tick(sqrt_price_x64, liquidity, amount_in, fee_bps, true, &[]).unwrap();
        let out_ba = compute_clmm_output_multi_tick(sqrt_price_x64, liquidity, out_ab, fee_bps, false, &[]).unwrap();
        // Round-trip should lose value due to fees + impact
        assert!(out_ba < amount_in, "round-trip should lose value: got {out_ba} from {amount_in}");
    }

    #[test]
    fn test_virtual_reserves_basic() {
        let sqrt_price = Q64; // price = 1.0
        let liquidity = 1_000_000u128;

        let (ra, rb) = virtual_reserves(sqrt_price, liquidity, true).unwrap();
        // With price=1.0: reserve_a = L, reserve_b = L
        assert_eq!(ra, liquidity);
        assert_eq!(rb, liquidity);

        let (rb2, ra2) = virtual_reserves(sqrt_price, liquidity, false).unwrap();
        assert_eq!(ra2, ra);
        assert_eq!(rb2, rb);
    }

    #[test]
    fn test_virtual_reserves_asymmetric_price() {
        // sqrt_price = 2 * Q64 => price = 4.0
        let sqrt_price = 2 * Q64;
        let liquidity = 1_000_000u128;

        let (ra, rb) = virtual_reserves(sqrt_price, liquidity, true).unwrap();
        // reserve_a = L * Q64 / (2*Q64) = L/2
        assert_eq!(ra, liquidity / 2);
        // reserve_b = L * 2*Q64 / Q64 = 2*L
        assert_eq!(rb, 2 * liquidity);
    }

    #[test]
    fn test_extract_clmm_params_raydium_clmm() {
        use crate::pool::types::PoolState;
        use solana_sdk::pubkey::Pubkey;

        let mint_0 = Pubkey::new_unique();
        let mint_1 = Pubkey::new_unique();
        let state = PoolState::RaydiumClmm {
            pool: Pubkey::new_unique(),
            amm_config: Pubkey::new_unique(),
            observation: Pubkey::new_unique(),
            token_vault_0: Pubkey::new_unique(),
            token_vault_1: Pubkey::new_unique(),
            tick_array_0: Pubkey::new_unique(),
            tick_array_1: Pubkey::new_unique(),
            tick_array_2: Pubkey::new_unique(),
            token_mint_0: mint_0,
            token_mint_1: mint_1,
            tick_current: 0,
            tick_spacing: 10,
            sqrt_price_x64: Q64,
            liquidity: 500_000,
            fee_rate: 2500, // hundredths of a bp from the AmmConfig
            fee_ext: Default::default(),
        };

        let params = extract_clmm_params(&state, &mint_0).unwrap();
        assert!(params.a_to_b);
        assert_eq!(params.sqrt_price_x64, Q64);
        assert_eq!(params.liquidity, 500_000);
        assert_eq!(params.fee_bps, 25);
        assert_eq!(params.fee_ppm, 2500);

        let params_rev = extract_clmm_params(&state, &mint_1).unwrap();
        assert!(!params_rev.a_to_b);

        // Unknown mint
        let unknown = Pubkey::new_unique();
        assert!(extract_clmm_params(&state, &unknown).is_none());
    }

    #[test]
    fn test_extract_clmm_params_orca() {
        use crate::pool::types::PoolState;
        use solana_sdk::pubkey::Pubkey;

        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let state = PoolState::Orca {
            whirlpool: Pubkey::new_unique(),
            token_vault_a: Pubkey::new_unique(),
            token_vault_b: Pubkey::new_unique(),
            oracle: Pubkey::new_unique(),
            token_mint_a: mint_a,
            token_mint_b: mint_b,
            tick_current: 0,
            tick_spacing: 64,
            sqrt_price_x64: Q64,
            liquidity: 1_000_000,
            // Orca fee_rate is in hundredths of bps (e.g., 3000 = 30 bps)
            fee_rate: 3000,
        };

        let params = extract_clmm_params(&state, &mint_a).unwrap();
        assert!(params.a_to_b);
        assert_eq!(params.fee_bps, 30); // 3000 / 100 = 30
    }

    #[test]
    fn test_extract_clmm_params_non_clmm_returns_none() {
        use crate::pool::types::PoolState;
        use solana_sdk::pubkey::Pubkey;

        let state = PoolState::PumpFunAmm {
            pool: Pubkey::new_unique(),
            base_mint: Pubkey::new_unique(),
            quote_mint: Pubkey::new_unique(),
            pool_base_vault: Pubkey::new_unique(),
            pool_quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_reserve: 1000,
            quote_reserve: 2000,
            protocol_fee_recipient: Pubkey::default(),
            buyback_accounts: Vec::new(),
            base_supply: 0,
            virtual_quote_reserve: 0,
            pamm_flags: Default::default(),
        };

        assert!(extract_clmm_params(&state, &Pubkey::new_unique()).is_none());
    }
}
