//! Deterministic, quality-aware consolidated market primitives.

mod fair_price;
mod stablecoin;

pub use fair_price::{
    AbstentionReason, BookAdmissionExclusion, BookProvenance, CandidatePriceKind,
    ConsolidatedError, ConsolidatedLineage, ConsolidatedOutcome, EstimatorConfigInput,
    ExcludedVenue, FairPrice, FairPriceEstimator, IncludedVenue, InstrumentProvenance,
    MetadataStatus, PolicyId, Ppm, TrustedBookAdmission, TrustedBookAdmissionInput,
    VenueExclusionReason, VenueQuote, VenueQuoteInput, VenueTradingState, VerifiedCatalogSnapshot,
};
pub use stablecoin::{
    PriceInterval, QuoteConversionReference, StablecoinDislocationState, StablecoinExcludedVenue,
    StablecoinExclusionReason, StablecoinReferenceConfigInput, StablecoinReferenceEstimator,
    StablecoinReferenceOutcome, StablecoinVenueQuote, StablecoinVenueQuoteInput,
};
