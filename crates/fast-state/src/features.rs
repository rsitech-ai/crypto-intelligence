//! Pure, deterministic fast-trigger feature aggregations.

use crate::TriggerError;

/// Converts bid/ask depth changes into the largest observed side-specific loss.
///
/// A positive result is disappearing displayed depth. Increasing depth yields
/// zero, while missingness is handled by the caller and never enters here.
pub fn depth_disappearance(
    bid_depth_change: f64,
    ask_depth_change: f64,
) -> Result<f64, TriggerError> {
    require_finite("bid_depth_change", bid_depth_change)?;
    require_finite("ask_depth_change", ask_depth_change)?;
    Ok((-bid_depth_change).max(-ask_depth_change).max(0.0))
}

/// Combines a certified-L3 cancellation rate and cancellation/trade ratio.
pub fn cancellation_burst(
    cancellation_rate_per_second: f64,
    cancellation_to_trade_ratio: f64,
) -> Result<f64, TriggerError> {
    require_nonnegative("cancellation_rate_per_second", cancellation_rate_per_second)?;
    require_nonnegative("cancellation_to_trade_ratio", cancellation_to_trade_ratio)?;
    let product = cancellation_rate_per_second * cancellation_to_trade_ratio;
    let result = product.ln_1p();
    if product.is_finite() && result.is_finite() {
        Ok(result)
    } else {
        Err(TriggerError::InvalidFeatureValue {
            feature: "cancellation_burst",
        })
    }
}

/// Point-in-time acceleration of cross-venue median absolute dispersion.
pub fn dispersion_acceleration(
    prior: f64,
    prior_event_time_ns: i64,
    current: f64,
    current_event_time_ns: i64,
) -> Result<f64, TriggerError> {
    require_nonnegative("cross_venue_median_absolute_dispersion", prior)?;
    require_nonnegative("cross_venue_median_absolute_dispersion", current)?;
    let elapsed_ns = current_event_time_ns
        .checked_sub(prior_event_time_ns)
        .filter(|elapsed| *elapsed > 0)
        .ok_or(TriggerError::InvalidFeatureHistory {
            feature: "cross_venue_median_absolute_dispersion",
        })?;
    let elapsed_seconds = elapsed_ns as f64 / 1_000_000_000.0;
    let result = (current - prior) / elapsed_seconds;
    if result.is_finite() {
        Ok(result)
    } else {
        Err(TriggerError::InvalidFeatureValue {
            feature: "cross_venue_dispersion_acceleration",
        })
    }
}

/// Confirms a price move with a simultaneous contraction in open interest.
///
/// The caller binds both inputs to their canonical feature definitions and
/// requires the same event-time end before invoking this pure calculation.
pub fn open_interest_destruction(
    aligned_price_return: f64,
    open_interest_relative_change: f64,
) -> Result<f64, TriggerError> {
    require_finite("log_return", aligned_price_return)?;
    require_finite(
        "open_interest_relative_change",
        open_interest_relative_change,
    )?;
    if open_interest_relative_change < -1.0 {
        return Err(TriggerError::InvalidFeatureValue {
            feature: "open_interest_relative_change",
        });
    }
    let result = (-open_interest_relative_change).max(0.0) * aligned_price_return.abs();
    if result.is_finite() {
        Ok(result)
    } else {
        Err(TriggerError::InvalidFeatureValue {
            feature: "open_interest_destruction",
        })
    }
}

/// Completeness-weighted liquidation pressure with OI-destruction confirmation.
///
/// `completeness` is derived by [`crate::TriggerStateBuilder`] from exact
/// per-source completeness classes and the cascade gate; it is not a free-form
/// health claim accepted at the observation boundary.
pub fn completeness_weighted_liquidation_pressure(
    velocity: f64,
    completeness: f64,
    open_interest_destruction: f64,
) -> Result<f64, TriggerError> {
    require_nonnegative("liquidation_observed_velocity", velocity)?;
    require_nonnegative("open_interest_destruction", open_interest_destruction)?;
    if !completeness.is_finite() || !(0.0..=1.0).contains(&completeness) {
        return Err(TriggerError::InvalidFeatureValue {
            feature: "liquidation_completeness",
        });
    }
    let weighted = velocity * completeness * (1.0 + open_interest_destruction);
    let result = weighted.ln_1p();
    if weighted.is_finite() && result.is_finite() {
        Ok(result)
    } else {
        Err(TriggerError::InvalidFeatureValue {
            feature: "liquidation_pressure",
        })
    }
}

pub(crate) fn validate_sweep_direction(value: f64) -> Result<f64, TriggerError> {
    require_finite("trade_print_sweep_direction", value)?;
    if (-1.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(TriggerError::InvalidFeatureValue {
            feature: "trade_print_sweep_direction",
        })
    }
}

pub(crate) fn validate_mark_index_divergence(value: f64) -> Result<f64, TriggerError> {
    require_finite("mark_index_divergence", value)?;
    if value > -1.0 {
        Ok(value)
    } else {
        Err(TriggerError::InvalidFeatureValue {
            feature: "mark_index_divergence",
        })
    }
}

pub(crate) fn require_nonnegative(feature: &'static str, value: f64) -> Result<f64, TriggerError> {
    require_finite(feature, value)?;
    if value >= 0.0 {
        Ok(value)
    } else {
        Err(TriggerError::InvalidFeatureValue { feature })
    }
}

fn require_finite(feature: &'static str, value: f64) -> Result<(), TriggerError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(TriggerError::InvalidFeatureValue { feature })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cancellation_burst, completeness_weighted_liquidation_pressure, depth_disappearance,
        dispersion_acceleration, open_interest_destruction,
    };

    #[test]
    fn pure_aggregations_reject_invalid_domains() {
        assert!(depth_disappearance(f64::NAN, 0.0).is_err());
        assert!(cancellation_burst(-1.0, 1.0).is_err());
        assert!(dispersion_acceleration(0.1, 2, 0.2, 1).is_err());
        assert!(completeness_weighted_liquidation_pressure(1.0, 1.1, 0.0).is_err());
        assert!(completeness_weighted_liquidation_pressure(1.0, 1.0, -0.1).is_err());
        assert_eq!(
            open_interest_destruction(-0.5, -0.2).expect("finite contraction"),
            0.1
        );
        assert_eq!(
            open_interest_destruction(0.5, 0.2).expect("OI expansion"),
            0.0
        );
        assert!(open_interest_destruction(0.5, -1.1).is_err());
    }
}
