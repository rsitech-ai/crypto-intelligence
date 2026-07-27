//! Checked fixed-point values for authoritative monetary boundaries.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use std::{cmp::Ordering, fmt, str::FromStr};
use thiserror::Error;

/// Maximum supported count of fractional decimal digits.
pub const MAX_SCALE: u32 = 38;

/// Failures produced by fixed-decimal parsing and checked arithmetic.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DecimalError {
    #[error("decimal text is not canonical")]
    InvalidSyntax,
    #[error("decimal scale exceeds {MAX_SCALE}")]
    ScaleTooLarge,
    #[error("fixed-decimal arithmetic overflow")]
    ArithmeticOverflow,
    #[error("rescaling would lose precision")]
    PrecisionLoss,
    #[error("division by zero")]
    DivisionByZero,
    #[error("{0} must be positive")]
    NonPositive(&'static str),
    #[error("{0} must not be negative")]
    Negative(&'static str),
}

/// A canonical signed fixed-point decimal backed by `i128`.
///
/// Canonical values have no redundant fractional zeroes. Zero always has scale
/// zero, so equality, ordering, hashing, and serialization have one
/// representation per numeric value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FixedDecimal {
    mantissa: i128,
    scale: u32,
}

impl FixedDecimal {
    /// Constructs a value and removes redundant fractional zeroes.
    pub fn new(mantissa: i128, scale: u32) -> Result<Self, DecimalError> {
        if scale > MAX_SCALE {
            return Err(DecimalError::ScaleTooLarge);
        }

        Ok(Self { mantissa, scale }.canonicalized())
    }

    /// Parses the only accepted textual representation of a value.
    pub fn parse_canonical(input: &str) -> Result<Self, DecimalError> {
        let (negative, body) = input
            .strip_prefix('-')
            .map_or((false, input), |body| (true, body));
        let (whole, fraction) = split_decimal(body)?;

        if whole.len() > 1 && whole.starts_with('0') {
            return Err(DecimalError::InvalidSyntax);
        }
        if !fraction.is_empty() && fraction.ends_with('0') {
            return Err(DecimalError::InvalidSyntax);
        }

        let scale = u32::try_from(fraction.len()).map_err(|_| DecimalError::ScaleTooLarge)?;
        if scale > MAX_SCALE {
            return Err(DecimalError::ScaleTooLarge);
        }

        let magnitude = parse_magnitude(whole, fraction)?;
        if negative && magnitude == 0 {
            return Err(DecimalError::InvalidSyntax);
        }

        let mantissa = signed_magnitude(magnitude, negative)?;
        Ok(Self { mantissa, scale })
    }

    /// Parses ordinary fixed-point text and canonicalizes its representation.
    ///
    /// Authoritative serialized boundaries should use [`Self::parse_canonical`].
    pub fn parse(input: &str) -> Result<Self, DecimalError> {
        let (negative, body) = input
            .strip_prefix('-')
            .map_or((false, input), |body| (true, body));
        let (whole, fraction) = split_decimal(body)?;
        let scale = u32::try_from(fraction.len()).map_err(|_| DecimalError::ScaleTooLarge)?;
        if scale > MAX_SCALE {
            return Err(DecimalError::ScaleTooLarge);
        }

        let magnitude = parse_magnitude(whole, fraction)?;
        Self::new(signed_magnitude(magnitude, negative)?, scale)
    }

    pub const fn mantissa(self) -> i128 {
        self.mantissa
    }

    pub const fn scale(self) -> u32 {
        self.scale
    }

    pub const fn is_zero(self) -> bool {
        self.mantissa == 0
    }

    pub const fn is_positive(self) -> bool {
        self.mantissa > 0
    }

    pub const fn is_negative(self) -> bool {
        self.mantissa < 0
    }

    pub fn checked_add(self, rhs: Self) -> Result<Self, DecimalError> {
        let (left, right, scale) = self.aligned_with(rhs)?;
        let mantissa = left
            .checked_add(right)
            .ok_or(DecimalError::ArithmeticOverflow)?;
        Self::new(mantissa, scale)
    }

    pub fn checked_sub(self, rhs: Self) -> Result<Self, DecimalError> {
        let (left, right, scale) = self.aligned_with(rhs)?;
        let mantissa = left
            .checked_sub(right)
            .ok_or(DecimalError::ArithmeticOverflow)?;
        Self::new(mantissa, scale)
    }

    pub fn checked_mul(self, rhs: Self) -> Result<Self, DecimalError> {
        let scale = self
            .scale
            .checked_add(rhs.scale)
            .ok_or(DecimalError::ArithmeticOverflow)?;
        if scale > MAX_SCALE {
            return Err(DecimalError::ScaleTooLarge);
        }
        let mantissa = self
            .mantissa
            .checked_mul(rhs.mantissa)
            .ok_or(DecimalError::ArithmeticOverflow)?;
        Self::new(mantissa, scale)
    }

    pub fn checked_neg(self) -> Result<Self, DecimalError> {
        Self::new(
            self.mantissa
                .checked_neg()
                .ok_or(DecimalError::ArithmeticOverflow)?,
            self.scale,
        )
    }

    /// Checks that conversion to `target` decimal places is exact.
    ///
    /// The returned value remains canonical, so its stored scale may be lower
    /// than `target` when the added decimal places are redundant.
    pub fn rescale_exact(self, target: u32) -> Result<Self, DecimalError> {
        if target > MAX_SCALE {
            return Err(DecimalError::ScaleTooLarge);
        }
        match target.cmp(&self.scale) {
            Ordering::Equal => Ok(self),
            Ordering::Greater => {
                let mantissa = checked_scale_up(self.mantissa, target - self.scale)?;
                Self::new(mantissa, target)
            }
            Ordering::Less => {
                let factor = checked_power_of_ten(self.scale - target)?;
                if self.mantissa % factor != 0 {
                    return Err(DecimalError::PrecisionLoss);
                }
                Self::new(self.mantissa / factor, target)
            }
        }
    }

    /// Converts to binary floating point for non-authoritative analytics only.
    pub fn to_f64_lossy_for_analysis(self) -> f64 {
        self.mantissa as f64 / 10_f64.powi(self.scale as i32)
    }

    fn canonicalized(mut self) -> Self {
        if self.mantissa == 0 {
            self.scale = 0;
            return self;
        }
        while self.scale > 0 && self.mantissa % 10 == 0 {
            self.mantissa /= 10;
            self.scale -= 1;
        }
        self
    }

    fn aligned_with(self, rhs: Self) -> Result<(i128, i128, u32), DecimalError> {
        let scale = self.scale.max(rhs.scale);
        Ok((
            checked_scale_up(self.mantissa, scale - self.scale)?,
            checked_scale_up(rhs.mantissa, scale - rhs.scale)?,
            scale,
        ))
    }
}

impl Ord for FixedDecimal {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.scale == other.scale {
            return self.mantissa.cmp(&other.mantissa);
        }

        match self.aligned_with(*other) {
            Ok((left, right, _)) => left.cmp(&right),
            Err(_) => compare_without_scaling(*self, *other),
        }
    }
}

impl PartialOrd for FixedDecimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for FixedDecimal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.mantissa.is_negative();
        let mut digits = self.mantissa.unsigned_abs().to_string();
        let scale = self.scale as usize;

        if scale == 0 {
            if negative {
                formatter.write_str("-")?;
            }
            return formatter.write_str(&digits);
        }

        if digits.len() <= scale {
            digits.insert_str(0, &"0".repeat(scale + 1 - digits.len()));
        }
        let decimal_point = digits.len() - scale;
        if negative {
            formatter.write_str("-")?;
        }
        write!(
            formatter,
            "{}.{}",
            &digits[..decimal_point],
            &digits[decimal_point..]
        )
    }
}

impl FromStr for FixedDecimal {
    type Err = DecimalError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse_canonical(input)
    }
}

impl Serialize for FixedDecimal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for FixedDecimal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse_canonical(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

macro_rules! decimal_wrapper {
    ($name:ident, $validate:expr, $error:expr) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(FixedDecimal);

        impl $name {
            pub fn new(value: FixedDecimal) -> Result<Self, DecimalError> {
                if ($validate)(value) {
                    Ok(Self(value))
                } else {
                    Err($error)
                }
            }

            pub const fn value(self) -> FixedDecimal {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                self.0.serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(FixedDecimal::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }
    };
}

decimal_wrapper!(
    Price,
    |value: FixedDecimal| value.is_positive(),
    DecimalError::NonPositive("price")
);
decimal_wrapper!(
    Quantity,
    |value: FixedDecimal| !value.is_negative(),
    DecimalError::Negative("quantity")
);
decimal_wrapper!(
    Notional,
    |value: FixedDecimal| !value.is_negative(),
    DecimalError::Negative("notional")
);
decimal_wrapper!(
    Rate,
    |_value: FixedDecimal| true,
    DecimalError::InvalidSyntax
);

fn split_decimal(input: &str) -> Result<(&str, &str), DecimalError> {
    if input.is_empty()
        || input.trim() != input
        || input.starts_with('+')
        || input.contains(['e', 'E'])
    {
        return Err(DecimalError::InvalidSyntax);
    }

    let (whole, fraction) = input.split_once('.').map_or((input, ""), |parts| parts);
    if whole.is_empty()
        || input.matches('.').count() > 1
        || (input.contains('.') && fraction.is_empty())
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(DecimalError::InvalidSyntax);
    }

    Ok((whole, fraction))
}

fn parse_magnitude(whole: &str, fraction: &str) -> Result<u128, DecimalError> {
    format!("{whole}{fraction}")
        .parse::<u128>()
        .map_err(|_| DecimalError::ArithmeticOverflow)
}

fn signed_magnitude(magnitude: u128, negative: bool) -> Result<i128, DecimalError> {
    if negative && magnitude == i128::MAX as u128 + 1 {
        return Ok(i128::MIN);
    }

    let value = i128::try_from(magnitude).map_err(|_| DecimalError::ArithmeticOverflow)?;
    if negative {
        value.checked_neg().ok_or(DecimalError::ArithmeticOverflow)
    } else {
        Ok(value)
    }
}

fn checked_power_of_ten(power: u32) -> Result<i128, DecimalError> {
    10_i128
        .checked_pow(power)
        .ok_or(DecimalError::ArithmeticOverflow)
}

fn checked_scale_up(value: i128, power: u32) -> Result<i128, DecimalError> {
    value
        .checked_mul(checked_power_of_ten(power)?)
        .ok_or(DecimalError::ArithmeticOverflow)
}

fn compare_without_scaling(left: FixedDecimal, right: FixedDecimal) -> Ordering {
    let sign = left.mantissa.signum().cmp(&right.mantissa.signum());
    if sign != Ordering::Equal {
        return sign;
    }
    if left.is_zero() {
        return Ordering::Equal;
    }

    let mut left_digits = left.mantissa.unsigned_abs().to_string();
    let mut right_digits = right.mantissa.unsigned_abs().to_string();
    left_digits.extend(std::iter::repeat_n('0', right.scale as usize));
    right_digits.extend(std::iter::repeat_n('0', left.scale as usize));
    let magnitude = left_digits
        .len()
        .cmp(&right_digits.len())
        .then_with(|| left_digits.cmp(&right_digits));

    if left.is_negative() {
        magnitude.reverse()
    } else {
        magnitude
    }
}
