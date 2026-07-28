//! Checked fixed-point values for authoritative monetary boundaries.

mod error;

pub use error::DecimalError;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use std::{cmp::Ordering, fmt, str::FromStr};

/// Maximum supported count of fractional decimal digits.
pub const MAX_SCALE: u32 = 38;

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
        self.checked_sum(rhs, false)
    }

    pub fn checked_sub(self, rhs: Self) -> Result<Self, DecimalError> {
        self.checked_sum(rhs, true)
    }

    pub fn checked_mul(self, rhs: Self) -> Result<Self, DecimalError> {
        if self.is_zero() || rhs.is_zero() {
            return Self::new(0, 0);
        }

        let combined_scale = self
            .scale
            .checked_add(rhs.scale)
            .ok_or(DecimalError::ArithmeticOverflow)?;
        let mut factors = [self.mantissa.unsigned_abs(), rhs.mantissa.unsigned_abs()];
        let cancellable_tens = combined_scale
            .min(total_factor_count(&factors, 2))
            .min(total_factor_count(&factors, 5));
        cancel_factor(&mut factors, 2, cancellable_tens);
        cancel_factor(&mut factors, 5, cancellable_tens);

        let scale = combined_scale - cancellable_tens;
        if scale > MAX_SCALE {
            return Err(DecimalError::ScaleTooLarge);
        }
        let magnitude = factors[0]
            .checked_mul(factors[1])
            .ok_or(DecimalError::ArithmeticOverflow)?;
        let negative = self.is_negative() ^ rhs.is_negative();
        Self::new(signed_magnitude(magnitude, negative)?, scale)
    }

    /// Divides without rounding and returns a canonical finite decimal.
    ///
    /// A quotient whose reduced denominator contains factors other than two
    /// and five, or whose exact representation requires more than
    /// [`MAX_SCALE`] fractional digits, returns [`DecimalError::PrecisionLoss`].
    pub fn checked_div_exact(self, rhs: Self) -> Result<Self, DecimalError> {
        if rhs.is_zero() {
            return Err(DecimalError::DivisionByZero);
        }
        if self.is_zero() {
            return Self::new(0, 0);
        }

        let mut numerator = self.mantissa.unsigned_abs();
        let mut denominator = rhs.mantissa.unsigned_abs();
        let common_factor = greatest_common_divisor(numerator, denominator);
        numerator /= common_factor;
        denominator /= common_factor;

        let denominator_twos = remove_factor(&mut denominator, 2);
        let denominator_fives = remove_factor(&mut denominator, 5);
        if denominator != 1 {
            return Err(DecimalError::PrecisionLoss);
        }

        let decimal_places = denominator_twos.max(denominator_fives);
        let result_scale = i64::from(decimal_places) + i64::from(self.scale) - i64::from(rhs.scale);
        if result_scale > i64::from(MAX_SCALE) {
            return Err(DecimalError::PrecisionLoss);
        }

        numerator = checked_multiply_power(numerator, 2, decimal_places - denominator_twos)?;
        numerator = checked_multiply_power(numerator, 5, decimal_places - denominator_fives)?;

        let scale = if result_scale < 0 {
            let integer_places =
                u32::try_from(-result_scale).map_err(|_| DecimalError::ArithmeticOverflow)?;
            numerator = checked_multiply_power(numerator, 10, integer_places)?;
            0
        } else {
            u32::try_from(result_scale).map_err(|_| DecimalError::ArithmeticOverflow)?
        };

        let negative = self.is_negative() ^ rhs.is_negative();
        Self::new(signed_magnitude(numerator, negative)?, scale)
    }

    /// Multiplies three factors after globally cancelling exact decimal scale.
    ///
    /// Global cancellation avoids rejecting a representable final value merely
    /// because one arbitrary pair would overflow as an intermediate product.
    pub fn checked_product3(self, second: Self, third: Self) -> Result<Self, DecimalError> {
        if self.is_zero() || second.is_zero() || third.is_zero() {
            return Self::new(0, 0);
        }

        let combined_scale = self
            .scale
            .checked_add(second.scale)
            .and_then(|scale| scale.checked_add(third.scale))
            .ok_or(DecimalError::ArithmeticOverflow)?;
        let mut factors = [
            self.mantissa.unsigned_abs(),
            second.mantissa.unsigned_abs(),
            third.mantissa.unsigned_abs(),
        ];
        let cancellable_tens = combined_scale
            .min(total_factor_count(&factors, 2))
            .min(total_factor_count(&factors, 5));
        cancel_factor(&mut factors, 2, cancellable_tens);
        cancel_factor(&mut factors, 5, cancellable_tens);

        let scale = combined_scale - cancellable_tens;
        if scale > MAX_SCALE {
            return Err(DecimalError::ScaleTooLarge);
        }
        let magnitude = factors.into_iter().try_fold(1_u128, |product, factor| {
            product
                .checked_mul(factor)
                .ok_or(DecimalError::ArithmeticOverflow)
        })?;
        let negative = self.is_negative() ^ second.is_negative() ^ third.is_negative();
        Self::new(signed_magnitude(magnitude, negative)?, scale)
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

    fn checked_sum(self, rhs: Self, subtract_rhs: bool) -> Result<Self, DecimalError> {
        let mut scale = self.scale.max(rhs.scale);
        let left = checked_scale_up_magnitude(self.mantissa.unsigned_abs(), scale - self.scale)?;
        let right = checked_scale_up_magnitude(rhs.mantissa.unsigned_abs(), scale - rhs.scale)?;
        let left_negative = self.is_negative();
        let right_negative = rhs.is_negative() ^ subtract_rhs;

        let (mut magnitude, negative) = if left_negative == right_negative {
            (
                left.checked_add(right)
                    .ok_or(DecimalError::ArithmeticOverflow)?,
                left_negative,
            )
        } else {
            match left.cmp(&right) {
                Ordering::Less => (right - left, right_negative),
                Ordering::Equal => (0, false),
                Ordering::Greater => (left - right, left_negative),
            }
        };
        while scale > 0 && magnitude.is_multiple_of(10) {
            magnitude /= 10;
            scale -= 1;
        }

        Self::new(signed_magnitude(magnitude, negative)?, scale)
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

fn checked_scale_up_magnitude(value: u128, power: u32) -> Result<u128, DecimalError> {
    value
        .checked_mul(
            10_u128
                .checked_pow(power)
                .ok_or(DecimalError::ArithmeticOverflow)?,
        )
        .ok_or(DecimalError::ArithmeticOverflow)
}

fn checked_multiply_power(value: u128, factor: u128, power: u32) -> Result<u128, DecimalError> {
    value
        .checked_mul(
            factor
                .checked_pow(power)
                .ok_or(DecimalError::ArithmeticOverflow)?,
        )
        .ok_or(DecimalError::ArithmeticOverflow)
}

fn greatest_common_divisor(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

fn remove_factor(value: &mut u128, factor: u128) -> u32 {
    let mut count = 0;
    while (*value).is_multiple_of(factor) {
        *value /= factor;
        count += 1;
    }
    count
}

fn factor_count(mut value: u128, factor: u128) -> u32 {
    let mut count = 0;
    while value.is_multiple_of(factor) {
        value /= factor;
        count += 1;
    }
    count
}

fn total_factor_count(values: &[u128], factor: u128) -> u32 {
    values
        .iter()
        .map(|value| factor_count(*value, factor))
        .sum()
}

fn cancel_factor(values: &mut [u128], factor: u128, mut count: u32) {
    for value in values {
        while count > 0 && (*value).is_multiple_of(factor) {
            *value /= factor;
            count -= 1;
        }
        if count == 0 {
            return;
        }
    }
    debug_assert_eq!(count, 0);
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
