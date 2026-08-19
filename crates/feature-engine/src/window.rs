//! Typed deterministic window membership, aggregation, and revision lifecycle.

use std::{collections::BTreeMap, num::NonZeroU64};

use domain::UnixNanos;
use feature_registry::{
    DurationNanos, FinalityState, WindowDefinition, WindowKind, WindowParameter,
};
use fixed_decimal::{DecimalError, FixedDecimal};
use thiserror::Error;

const MAX_SLIDING_MEMBERSHIPS: u64 = 1_024;
const MAX_LIFECYCLE_WINDOWS: usize = 4_096;
const MAX_REVISIONS_PER_WINDOW: usize = 64;
const MAX_EWMA_SAMPLES: usize = 4_096;
const CANONICAL_UTC_ANCHOR: UnixNanos = UnixNanos::new(0);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TimeWindow {
    start: UnixNanos,
    end: UnixNanos,
}

impl TimeWindow {
    pub fn try_new(start: UnixNanos, end: UnixNanos) -> Result<Self, WindowError> {
        if start.value() < 0 || end <= start {
            return Err(WindowError::InvalidTimeWindow);
        }
        Ok(Self { start, end })
    }

    pub const fn start(self) -> UnixNanos {
        self.start
    }

    pub const fn end(self) -> UnixNanos {
        self.end
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeWindowSpec {
    Tumbling {
        extent: DurationNanos,
        anchor: UnixNanos,
    },
    Sliding {
        extent: DurationNanos,
        advance: DurationNanos,
        anchor: UnixNanos,
    },
    SessionAligned {
        extent: DurationNanos,
        utc_anchor: UnixNanos,
    },
}

impl TimeWindowSpec {
    /// Builds the only runtime time-window semantics permitted by a versioned
    /// registry definition.
    ///
    /// Registry time windows use the Unix epoch as their canonical UTC anchor.
    /// Session windows carry their explicit positive UTC anchor.
    pub fn try_from_definition(definition: &WindowDefinition) -> Result<Self, WindowError> {
        match (definition.kind(), definition.parameter()) {
            (
                WindowKind::Tumbling,
                WindowParameter::Time {
                    extent,
                    advance: None,
                },
            ) => {
                validate_time_shape(*extent, CANONICAL_UTC_ANCHOR)?;
                Ok(Self::Tumbling {
                    extent: *extent,
                    anchor: CANONICAL_UTC_ANCHOR,
                })
            }
            (
                WindowKind::Sliding,
                WindowParameter::Time {
                    extent,
                    advance: Some(advance),
                },
            ) => {
                validate_time_shape(*extent, CANONICAL_UTC_ANCHOR)?;
                validate_sliding_shape(*extent, *advance)?;
                Ok(Self::Sliding {
                    extent: *extent,
                    advance: *advance,
                    anchor: CANONICAL_UTC_ANCHOR,
                })
            }
            (
                WindowKind::SessionAligned,
                WindowParameter::SessionAligned { extent, utc_anchor },
            ) => {
                validate_time_shape(*extent, *utc_anchor)?;
                Ok(Self::SessionAligned {
                    extent: *extent,
                    utc_anchor: *utc_anchor,
                })
            }
            _ => Err(WindowError::DefinitionKindMismatch),
        }
    }

    pub fn windows_for(self, event_time: UnixNanos) -> Result<Vec<TimeWindow>, WindowError> {
        if event_time.value() <= 0 {
            return Err(WindowError::InvalidEventTime);
        }
        match self {
            Self::Tumbling { extent, anchor }
            | Self::SessionAligned {
                extent,
                utc_anchor: anchor,
            } => Ok(vec![aligned_window(event_time, extent, anchor, extent)?]),
            Self::Sliding {
                extent,
                advance,
                anchor,
            } => {
                let extent_i64 =
                    i64::try_from(extent.value()).map_err(|_| WindowError::InvalidWindowSpec)?;
                let advance_i64 =
                    i64::try_from(advance.value()).map_err(|_| WindowError::InvalidWindowSpec)?;
                let delta = event_time
                    .value()
                    .checked_sub(anchor.value())
                    .ok_or(WindowError::ArithmeticOverflow)?;
                let latest_step = delta.div_euclid(advance_i64);
                let latest_start = anchor
                    .value()
                    .checked_add(
                        latest_step
                            .checked_mul(advance_i64)
                            .ok_or(WindowError::ArithmeticOverflow)?,
                    )
                    .ok_or(WindowError::ArithmeticOverflow)?;
                let memberships = extent
                    .value()
                    .checked_add(advance.value() - 1)
                    .ok_or(WindowError::ArithmeticOverflow)?
                    / advance.value();
                let mut windows = Vec::with_capacity(memberships as usize);
                for offset in (0..memberships).rev() {
                    let offset_i64 =
                        i64::try_from(offset).map_err(|_| WindowError::ArithmeticOverflow)?;
                    let start = latest_start
                        .checked_sub(
                            offset_i64
                                .checked_mul(advance_i64)
                                .ok_or(WindowError::ArithmeticOverflow)?,
                        )
                        .ok_or(WindowError::ArithmeticOverflow)?;
                    let end = start
                        .checked_add(extent_i64)
                        .ok_or(WindowError::ArithmeticOverflow)?;
                    if start >= 0 && start <= event_time.value() && event_time.value() < end {
                        windows.push(TimeWindow::try_new(
                            UnixNanos::new(start),
                            UnixNanos::new(end),
                        )?);
                    }
                }
                Ok(windows)
            }
        }
    }
}

fn validate_sliding_shape(
    extent: DurationNanos,
    advance: DurationNanos,
) -> Result<(), WindowError> {
    let extent_value = extent.value();
    let advance_value = advance.value();
    if advance_value == 0 || advance_value > extent_value {
        return Err(WindowError::InvalidWindowSpec);
    }
    let memberships = extent_value
        .checked_add(advance_value - 1)
        .ok_or(WindowError::ArithmeticOverflow)?
        / advance_value;
    if memberships > MAX_SLIDING_MEMBERSHIPS {
        return Err(WindowError::MembershipCapacity);
    }
    Ok(())
}

fn validate_time_shape(extent: DurationNanos, anchor: UnixNanos) -> Result<(), WindowError> {
    if extent.is_zero() || extent.value() > i64::MAX as u64 || anchor.value() < 0 {
        Err(WindowError::InvalidWindowSpec)
    } else {
        Ok(())
    }
}

fn aligned_window(
    event_time: UnixNanos,
    extent: DurationNanos,
    anchor: UnixNanos,
    advance: DurationNanos,
) -> Result<TimeWindow, WindowError> {
    let extent_i64 = i64::try_from(extent.value()).map_err(|_| WindowError::InvalidWindowSpec)?;
    let advance_i64 = i64::try_from(advance.value()).map_err(|_| WindowError::InvalidWindowSpec)?;
    let delta = event_time
        .value()
        .checked_sub(anchor.value())
        .ok_or(WindowError::ArithmeticOverflow)?;
    let step = delta.div_euclid(advance_i64);
    let start = anchor
        .value()
        .checked_add(
            step.checked_mul(advance_i64)
                .ok_or(WindowError::ArithmeticOverflow)?,
        )
        .ok_or(WindowError::ArithmeticOverflow)?;
    let end = start
        .checked_add(extent_i64)
        .ok_or(WindowError::ArithmeticOverflow)?;
    TimeWindow::try_new(UnixNanos::new(start), UnixNanos::new(end))
}

#[derive(Clone, Debug, PartialEq)]
pub struct HalfLifeEwmaState {
    half_life_nanos: u64,
    capacity: usize,
    samples: Vec<EwmaSample>,
    checkpoint: Option<EwmaCheckpoint>,
    compacted_through: Option<UnixNanos>,
    next_arrival_sequence: u64,
    value: Option<f64>,
    policy_id: crate::watermark::WatermarkPolicyId,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct EwmaCheckpoint {
    event_time: UnixNanos,
    value: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct EwmaSample {
    event_time: UnixNanos,
    arrival_sequence: u64,
    value: f64,
}

impl HalfLifeEwmaState {
    pub fn try_from_definition(
        definition: &WindowDefinition,
        capacity: usize,
        tracker: &crate::WatermarkTracker,
    ) -> Result<Self, WindowError> {
        let half_life = match (definition.kind(), definition.parameter()) {
            (
                WindowKind::ExponentiallyWeighted,
                WindowParameter::ExponentiallyWeighted { half_life, .. },
            ) => *half_life,
            _ => return Err(WindowError::DefinitionKindMismatch),
        };
        if half_life.is_zero() || half_life.value() > i64::MAX as u64 {
            return Err(WindowError::InvalidWindowSpec);
        }
        if capacity == 0 || capacity > MAX_EWMA_SAMPLES {
            return Err(WindowError::InvalidEwmaCapacity);
        }
        Ok(Self {
            half_life_nanos: half_life.value(),
            capacity,
            samples: Vec::new(),
            checkpoint: None,
            compacted_through: None,
            next_arrival_sequence: 0,
            value: None,
            policy_id: tracker.policy_id(),
        })
    }

    /// Applies time-aware half-life decay with bounded late-event replay.
    ///
    /// Floating-point EWMA outputs follow the specification's numerical
    /// equivalence rule; replay on the same supported runtime is bit-identical.
    pub fn update_at(&mut self, event_time: UnixNanos, input: f64) -> Result<f64, WindowError> {
        if event_time.value() <= 0 || !input.is_finite() {
            return Err(WindowError::InvalidEwmaInput);
        }
        if self
            .compacted_through
            .is_some_and(|watermark| event_time <= watermark)
        {
            return Err(WindowError::EwmaBeforeWatermark);
        }
        if self.samples.len() >= self.capacity {
            return Err(WindowError::EwmaCapacity);
        }
        let next_arrival_sequence = self
            .next_arrival_sequence
            .checked_add(1)
            .ok_or(WindowError::ArithmeticOverflow)?;
        let mut samples = self.samples.clone();
        samples.push(EwmaSample {
            event_time,
            arrival_sequence: self.next_arrival_sequence,
            value: input,
        });
        samples.sort_by_key(|sample| (sample.event_time, sample.arrival_sequence));
        let next = compute_ewma(self.half_life_nanos, self.checkpoint, &samples)?.value;
        self.samples = samples;
        self.next_arrival_sequence = next_arrival_sequence;
        self.value = Some(next);
        Ok(next)
    }

    /// Compacts the exact prefix that can no longer receive valid late events.
    ///
    /// Callers must advance this boundary from the same event-time watermark
    /// policy that governs their input stream. Samples at or before the
    /// boundary become an exact checkpoint; later attempts to rewrite that
    /// prefix fail closed.
    pub fn advance_watermark(
        &mut self,
        tracker: &crate::WatermarkTracker,
    ) -> Result<(), WindowError> {
        if tracker.policy_id() != self.policy_id {
            return Err(WindowError::ForeignWatermarkPolicy);
        }
        let watermark = tracker
            .effective_frontier()
            .ok_or(WindowError::EwmaWatermarkUnavailable)?;
        if let Some(current) = self.compacted_through {
            if watermark < current {
                return Err(WindowError::EwmaWatermarkRegression);
            }
            if watermark == current {
                return Ok(());
            }
        }

        let prefix_len = self
            .samples
            .partition_point(|sample| sample.event_time <= watermark);
        let checkpoint = if prefix_len == 0 {
            self.checkpoint
        } else {
            Some(compute_ewma(
                self.half_life_nanos,
                self.checkpoint,
                &self.samples[..prefix_len],
            )?)
        };
        self.samples.drain(..prefix_len);
        self.checkpoint = checkpoint;
        self.compacted_through = Some(watermark);
        Ok(())
    }

    pub const fn value(&self) -> Option<f64> {
        self.value
    }

    pub fn buffered_samples(&self) -> usize {
        self.samples.len()
    }

    pub const fn compacted_through(&self) -> Option<UnixNanos> {
        self.compacted_through
    }
}

fn compute_ewma(
    half_life_nanos: u64,
    checkpoint: Option<EwmaCheckpoint>,
    samples: &[EwmaSample],
) -> Result<EwmaCheckpoint, WindowError> {
    let mut iter = samples.iter();
    let (mut previous_time, mut value) = if let Some(checkpoint) = checkpoint {
        (checkpoint.event_time, checkpoint.value)
    } else {
        let first = iter.next().ok_or(WindowError::InvalidWindowState)?;
        (first.event_time, first.value)
    };
    for sample in iter {
        let delta = sample
            .event_time
            .value()
            .checked_sub(previous_time.value())
            .ok_or(WindowError::ArithmeticOverflow)?;
        let decay = 2.0_f64.powf(-(delta as f64) / half_life_nanos as f64);
        value = decay.mul_add(value, (1.0 - decay) * sample.value);
        if !value.is_finite() {
            return Err(WindowError::InvalidEwmaInput);
        }
        previous_time = sample.event_time;
    }
    Ok(EwmaCheckpoint {
        event_time: previous_time,
        value,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CountWindow {
    pub index: u64,
    pub start_event: u64,
    pub end_event: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventCountState {
    extent: NonZeroU64,
    advance: NonZeroU64,
    observed: u64,
    next_index: u64,
}

impl EventCountState {
    pub fn try_from_definition(definition: &WindowDefinition) -> Result<Self, WindowError> {
        let (extent, advance) = match (definition.kind(), definition.parameter()) {
            (WindowKind::EventCount, WindowParameter::EventCount { events, advance }) => {
                (*events, advance.unwrap_or(*events))
            }
            _ => return Err(WindowError::DefinitionKindMismatch),
        };
        if advance > extent {
            return Err(WindowError::InvalidWindowSpec);
        }
        Ok(Self {
            extent,
            advance,
            observed: 0,
            next_index: 0,
        })
    }

    pub fn observe(&mut self) -> Result<Option<CountWindow>, WindowError> {
        let observed = self
            .observed
            .checked_add(1)
            .ok_or(WindowError::ArithmeticOverflow)?;
        if observed < self.extent.get()
            || !(observed - self.extent.get()).is_multiple_of(self.advance.get())
        {
            self.observed = observed;
            return Ok(None);
        }
        let start_event = observed
            .checked_sub(self.extent.get() - 1)
            .ok_or(WindowError::ArithmeticOverflow)?;
        let window = CountWindow {
            index: self.next_index,
            start_event,
            end_event: observed,
        };
        let next_index = self
            .next_index
            .checked_add(1)
            .ok_or(WindowError::ArithmeticOverflow)?;
        self.observed = observed;
        self.next_index = next_index;
        Ok(Some(window))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThresholdKind {
    Volume,
    Notional,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThresholdWindow {
    pub kind: ThresholdKind,
    pub index: u64,
    pub total: FixedDecimal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThresholdWindowState {
    kind: ThresholdKind,
    threshold: FixedDecimal,
    accumulated: FixedDecimal,
    next_index: u64,
}

impl ThresholdWindowState {
    pub fn try_from_definition(definition: &WindowDefinition) -> Result<Self, WindowError> {
        let (kind, threshold) = match (definition.kind(), definition.parameter()) {
            (WindowKind::Volume, WindowParameter::Threshold { threshold }) => {
                (ThresholdKind::Volume, *threshold)
            }
            (WindowKind::Notional, WindowParameter::Threshold { threshold }) => {
                (ThresholdKind::Notional, *threshold)
            }
            _ => return Err(WindowError::DefinitionKindMismatch),
        };
        if !threshold.is_positive() {
            return Err(WindowError::InvalidThreshold);
        }
        Ok(Self {
            kind,
            threshold,
            accumulated: FixedDecimal::new(0, 0)?,
            next_index: 0,
        })
    }

    pub fn observe(
        &mut self,
        amount: FixedDecimal,
    ) -> Result<Option<ThresholdWindow>, WindowError> {
        if !amount.is_positive() {
            return Err(WindowError::InvalidAmount);
        }
        let accumulated = self.accumulated.checked_add(amount)?;
        if accumulated < self.threshold {
            self.accumulated = accumulated;
            return Ok(None);
        }
        let closed = ThresholdWindow {
            kind: self.kind,
            index: self.next_index,
            total: accumulated,
        };
        self.next_index = self
            .next_index
            .checked_add(1)
            .ok_or(WindowError::ArithmeticOverflow)?;
        self.accumulated = FixedDecimal::new(0, 0)?;
        Ok(Some(closed))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmissionAction {
    Provisional {
        revision: u32,
    },
    RevisedProvisional {
        replaces_revision: u32,
        revision: u32,
    },
    Final {
        revision: u32,
    },
    Corrected {
        replaces_revision: u32,
        revision: u32,
    },
    Invalid {
        revision: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowEmission {
    revision: u32,
    finality: FinalityState,
    decision_sequence: u64,
}

impl WindowEmission {
    pub const fn revision(&self) -> u32 {
        self.revision
    }

    pub const fn finality(&self) -> FinalityState {
        self.finality
    }
}

#[derive(Clone, Debug)]
pub struct WindowLifecycle {
    window_capacity: usize,
    revision_capacity: usize,
    histories: BTreeMap<TimeWindow, Vec<WindowEmission>>,
    policy_id: crate::watermark::WatermarkPolicyId,
}

impl WindowLifecycle {
    pub fn try_new(
        tracker: &crate::WatermarkTracker,
        window_capacity: usize,
        revision_capacity: usize,
    ) -> Result<Self, WindowError> {
        if window_capacity == 0
            || window_capacity > MAX_LIFECYCLE_WINDOWS
            || !(2..=MAX_REVISIONS_PER_WINDOW).contains(&revision_capacity)
        {
            return Err(WindowError::InvalidLifecycle);
        }
        Ok(Self {
            window_capacity,
            revision_capacity,
            histories: BTreeMap::new(),
            policy_id: tracker.policy_id(),
        })
    }

    /// Applies opaque, exact-window evidence from [`crate::WatermarkTracker`].
    ///
    /// This is the only API that can create provisional, final, or invalid
    /// emissions, so finality cannot bypass watermark, lateness, or health
    /// policy.
    pub fn apply(
        &mut self,
        decision: crate::FinalizationDecision,
    ) -> Result<EmissionAction, WindowError> {
        if decision.policy_id() != self.policy_id {
            return Err(WindowError::ForeignWatermarkPolicy);
        }
        let window = decision.window();
        let state = decision.state();
        if !self.histories.contains_key(&window) {
            if self.histories.len() >= self.window_capacity {
                return Err(WindowError::WindowCapacity);
            }
            let finality = match state {
                crate::Finalization::Provisional => FinalityState::Provisional,
                crate::Finalization::Final => FinalityState::Final,
                crate::Finalization::Invalid => FinalityState::Invalid,
            };
            self.histories.insert(
                window,
                vec![WindowEmission {
                    revision: 1,
                    finality,
                    decision_sequence: decision.evaluation_sequence(),
                }],
            );
            return Ok(match state {
                crate::Finalization::Provisional => EmissionAction::Provisional { revision: 1 },
                crate::Finalization::Final => EmissionAction::Final { revision: 1 },
                crate::Finalization::Invalid => EmissionAction::Invalid { revision: 1 },
            });
        }

        let history = self
            .histories
            .get_mut(&window)
            .ok_or(WindowError::UnknownWindow)?;
        let previous = history
            .last()
            .copied()
            .ok_or(WindowError::InvalidWindowState)?;
        if decision.evaluation_sequence() <= previous.decision_sequence {
            return Err(WindowError::StaleFinalizationDecision);
        }
        match state {
            crate::Finalization::Provisional => {
                if previous.finality() != FinalityState::Provisional {
                    return Err(WindowError::InvalidTransition);
                }
                let revision = append_revision(
                    history,
                    self.revision_capacity,
                    FinalityState::Provisional,
                    decision.evaluation_sequence(),
                )?;
                Ok(EmissionAction::RevisedProvisional {
                    replaces_revision: previous.revision(),
                    revision,
                })
            }
            crate::Finalization::Final => {
                if previous.finality() != FinalityState::Provisional {
                    return Err(WindowError::InvalidTransition);
                }
                let revision = append_revision(
                    history,
                    self.revision_capacity,
                    FinalityState::Final,
                    decision.evaluation_sequence(),
                )?;
                Ok(EmissionAction::Final { revision })
            }
            crate::Finalization::Invalid => {
                if previous.finality() == FinalityState::Invalid {
                    return Err(WindowError::InvalidTransition);
                }
                let revision = append_revision(
                    history,
                    self.revision_capacity,
                    FinalityState::Invalid,
                    decision.evaluation_sequence(),
                )?;
                Ok(EmissionAction::Invalid { revision })
            }
        }
    }

    pub fn correct(
        &mut self,
        decision: crate::CorrectionDecision,
    ) -> Result<EmissionAction, WindowError> {
        if decision.policy_id() != self.policy_id {
            return Err(WindowError::ForeignWatermarkPolicy);
        }
        let window = decision.window();
        let history = self
            .histories
            .get_mut(&window)
            .ok_or(WindowError::UnknownWindow)?;
        let Some(previous) = history.last().copied() else {
            return Err(WindowError::InvalidTransition);
        };
        if !matches!(
            previous.finality(),
            FinalityState::Final | FinalityState::Corrected | FinalityState::Invalid
        ) {
            return Err(WindowError::InvalidTransition);
        }
        if decision.evaluation_sequence() <= previous.decision_sequence {
            return Err(WindowError::StaleFinalizationDecision);
        }
        let revision = append_revision(
            history,
            self.revision_capacity,
            FinalityState::Corrected,
            decision.evaluation_sequence(),
        )?;
        Ok(EmissionAction::Corrected {
            replaces_revision: previous.revision(),
            revision,
        })
    }

    pub fn history(&self, window: TimeWindow) -> Option<&[WindowEmission]> {
        self.histories.get(&window).map(Vec::as_slice)
    }
}

fn append_revision(
    history: &mut Vec<WindowEmission>,
    revision_capacity: usize,
    finality: FinalityState,
    decision_sequence: u64,
) -> Result<u32, WindowError> {
    if history.len() >= revision_capacity {
        return Err(WindowError::RevisionCapacity);
    }
    let revision = history
        .last()
        .ok_or(WindowError::InvalidTransition)?
        .revision
        .checked_add(1)
        .ok_or(WindowError::ArithmeticOverflow)?;
    history.push(WindowEmission {
        revision,
        finality,
        decision_sequence,
    });
    Ok(revision)
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WindowError {
    #[error("invalid time window")]
    InvalidTimeWindow,
    #[error("invalid time-window specification")]
    InvalidWindowSpec,
    #[error("event time must be positive")]
    InvalidEventTime,
    #[error("sliding-window membership exceeds the configured bound")]
    MembershipCapacity,
    #[error("registry window kind and parameter do not match this engine")]
    DefinitionKindMismatch,
    #[error("EWMA input must have a positive time and finite value")]
    InvalidEwmaInput,
    #[error("EWMA sample capacity must be in inclusive range 1..=4_096")]
    InvalidEwmaCapacity,
    #[error("EWMA sample capacity reached")]
    EwmaCapacity,
    #[error("EWMA watermark regressed")]
    EwmaWatermarkRegression,
    #[error("EWMA watermark is unavailable while required sources are incomplete or unhealthy")]
    EwmaWatermarkUnavailable,
    #[error("EWMA event is at or before the compacted watermark")]
    EwmaBeforeWatermark,
    #[error("window state invariant failed")]
    InvalidWindowState,
    #[error("threshold must be positive")]
    InvalidThreshold,
    #[error("observed threshold amount must be positive")]
    InvalidAmount,
    #[error("invalid lifecycle capacity")]
    InvalidLifecycle,
    #[error("window lifecycle capacity reached")]
    WindowCapacity,
    #[error("window revision capacity reached")]
    RevisionCapacity,
    #[error("window does not exist")]
    UnknownWindow,
    #[error("invalid window lifecycle transition")]
    InvalidTransition,
    #[error("finalization decision was already applied or predates the latest decision")]
    StaleFinalizationDecision,
    #[error("watermark decision belongs to a different tracker policy")]
    ForeignWatermarkPolicy,
    #[error("window arithmetic overflow")]
    ArithmeticOverflow,
    #[error("decimal arithmetic failed: {0}")]
    Decimal(#[from] DecimalError),
}
