use fixed_decimal::{DecimalError, FixedDecimal, MAX_SCALE};
use proptest::prelude::*;

#[test]
fn canonical_parser_accepts_only_unique_text_representations() {
    let accepted = [
        ("0", 0, 0),
        ("1", 1, 0),
        ("-1", -1, 0),
        ("0.1", 1, 1),
        ("-0.1", -1, 1),
        ("0.00000000000000000000000000000000000001", 1, MAX_SCALE),
    ];

    for (text, mantissa, scale) in accepted {
        let value = FixedDecimal::parse_canonical(text).expect("canonical value must parse");
        assert_eq!(value.mantissa(), mantissa);
        assert_eq!(value.scale(), scale);
        assert_eq!(value.to_string(), text);
    }

    for text in [
        "",
        " 1",
        "1 ",
        "+1",
        "1e3",
        "01",
        "-01",
        "1.",
        ".1",
        "1.0",
        "1.230",
        "-0",
        "-0.0",
        "0.000000000000000000000000000000000000001",
    ] {
        assert!(
            FixedDecimal::parse_canonical(text).is_err(),
            "{text:?} must be rejected"
        );
    }
}

#[test]
fn programmatic_construction_canonicalizes() {
    let value = FixedDecimal::new(123_000, 4).expect("scale is supported");
    assert_eq!(value.mantissa(), 123);
    assert_eq!(value.scale(), 1);
    assert_eq!(value.to_string(), "12.3");

    let zero = FixedDecimal::new(0, MAX_SCALE).expect("zero is representable");
    assert_eq!(zero.to_string(), "0");
}

#[test]
fn checked_arithmetic_reports_overflow() {
    let maximum = FixedDecimal::new(i128::MAX, 0).expect("maximum is representable");
    let one = FixedDecimal::new(1, 0).expect("one is representable");
    assert_eq!(
        maximum.checked_add(one),
        Err(DecimalError::ArithmeticOverflow)
    );

    let minimum = FixedDecimal::new(i128::MIN, 0).expect("minimum is representable");
    assert_eq!(minimum.checked_neg(), Err(DecimalError::ArithmeticOverflow));
    assert_eq!(
        maximum.checked_mul(FixedDecimal::new(2, 0).expect("two is representable")),
        Err(DecimalError::ArithmeticOverflow)
    );
}

#[test]
fn multiplication_reduces_scale_before_enforcing_the_limit() {
    let tiny_even = FixedDecimal::new(2, MAX_SCALE).expect("tiny even value");
    let half_ten = FixedDecimal::new(5, 1).expect("one half");

    assert_eq!(
        tiny_even.checked_mul(half_ten),
        FixedDecimal::new(1, MAX_SCALE)
    );
}

#[test]
fn multiplication_cancels_factors_before_raw_product_overflow() {
    let large_even = FixedDecimal::new(i128::MAX - 1, 0).expect("large even integer");
    let one_half = FixedDecimal::new(5, 1).expect("one half");

    assert_eq!(
        large_even.checked_mul(one_half),
        FixedDecimal::new((i128::MAX - 1) / 2, 0)
    );
}

proptest! {
    #[test]
    fn display_is_a_canonical_round_trip(mantissa in any::<i128>(), scale in 0_u32..=MAX_SCALE) {
        let value = FixedDecimal::new(mantissa, scale).expect("generated scale is supported");
        let text = value.to_string();
        let reparsed = FixedDecimal::parse_canonical(&text).expect("Display must be canonical");
        prop_assert_eq!(reparsed, value);
    }
}
