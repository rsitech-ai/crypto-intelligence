use fixed_decimal::{DecimalError, FixedDecimal, MAX_SCALE};
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, FileFailurePersistence};

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
    assert_eq!(
        FixedDecimal::parse("-0.000").expect("ordinary parsing canonicalizes negative zero"),
        zero
    );
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
fn cross_scale_cancellation_does_not_report_intermediate_overflow() {
    let large_integer = FixedDecimal::new(20_000_000_000_000_000_000_000_000_000_000_000_000, 0)
        .expect("large integer is representable");
    let minimum_at_tenths =
        FixedDecimal::new(i128::MIN, 1).expect("minimum mantissa is representable");
    let expected = FixedDecimal::new(29_858_816_539_530_768_268_312_696_284_115_894_272, 1);

    assert_eq!(large_integer.checked_add(minimum_at_tenths), expected);
    assert_eq!(minimum_at_tenths.checked_add(large_integer), expected);
    assert_eq!(
        large_integer.checked_sub(
            FixedDecimal::new(i128::MAX, 1).expect("maximum mantissa is representable")
        ),
        FixedDecimal::new(29_858_816_539_530_768_268_312_696_284_115_894_273, 1,)
    );
}

#[test]
fn signed_magnitude_boundaries_are_exact() {
    let negative_maximum =
        FixedDecimal::new(-i128::MAX, 1).expect("negative maximum is representable");
    let negative_tenth = FixedDecimal::new(-1, 1).expect("negative tenth is representable");
    assert_eq!(
        negative_maximum.checked_add(negative_tenth),
        FixedDecimal::new(i128::MIN, 1)
    );

    let minimum = FixedDecimal::new(i128::MIN, 0).expect("minimum is representable");
    let zero = FixedDecimal::new(0, 0).expect("zero is representable");
    assert_eq!(minimum.checked_sub(minimum), Ok(zero));
    assert_eq!(minimum.checked_sub(zero), Ok(minimum));
    assert_eq!(
        zero.checked_sub(minimum),
        Err(DecimalError::ArithmeticOverflow)
    );
}

#[test]
fn add_and_sub_canonicalize_before_signed_i128_conversion() {
    let positive = FixedDecimal::new(i128::MAX, 1).expect("positive boundary is representable");
    let positive_adjustment =
        FixedDecimal::new(3, 1).expect("positive adjustment is representable");
    let positive_expected =
        FixedDecimal::new(17_014_118_346_046_923_173_168_730_371_588_410_573, 0);
    assert_eq!(positive.checked_add(positive_adjustment), positive_expected);
    assert_eq!(
        positive
            .checked_sub(FixedDecimal::new(-3, 1).expect("negative adjustment is representable")),
        positive_expected
    );

    let negative = FixedDecimal::new(i128::MIN, 1).expect("negative boundary is representable");
    let negative_adjustment =
        FixedDecimal::new(-2, 1).expect("negative adjustment is representable");
    let negative_expected =
        FixedDecimal::new(-17_014_118_346_046_923_173_168_730_371_588_410_573, 0);
    assert_eq!(negative.checked_add(negative_adjustment), negative_expected);
    assert_eq!(
        negative
            .checked_sub(FixedDecimal::new(2, 1).expect("positive adjustment is representable")),
        negative_expected
    );
}

#[test]
fn analytical_float_conversion_is_finite_but_not_authoritative() {
    let exact =
        FixedDecimal::new(9_007_199_254_740_993, 0).expect("integer is representable exactly");
    let adjacent =
        FixedDecimal::new(9_007_199_254_740_992, 0).expect("integer is representable exactly");

    assert!(exact.to_f64_lossy_for_analysis().is_finite());
    assert_eq!(
        exact.to_f64_lossy_for_analysis(),
        adjacent.to_f64_lossy_for_analysis(),
        "binary analysis conversion must not be used as an authoritative identity"
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

#[test]
fn three_factor_product_cancels_scale_globally_before_multiplication() {
    let maximum = FixedDecimal::new(i128::MAX, 0).expect("maximum integer");
    let two = FixedDecimal::new(2, 0).expect("two");
    let one_half = FixedDecimal::new(5, 1).expect("one half");

    for factors in [
        [maximum, two, one_half],
        [maximum, one_half, two],
        [two, maximum, one_half],
        [two, one_half, maximum],
        [one_half, maximum, two],
        [one_half, two, maximum],
    ] {
        assert_eq!(
            factors[0].checked_product3(factors[1], factors[2]),
            Ok(maximum),
            "exact result must not depend on pairwise multiplication order"
        );
    }

    let negative_two = FixedDecimal::new(-2, 0).expect("negative two");
    assert_eq!(
        maximum.checked_product3(negative_two, one_half),
        FixedDecimal::new(-i128::MAX, 0)
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_failure_persistence(
        FileFailurePersistence::Direct(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/proptest-regressions/canonical_decimal.txt"
        ))
    ))]

    #[test]
    fn display_is_a_canonical_round_trip(mantissa in any::<i128>(), scale in 0_u32..=MAX_SCALE) {
        let value = FixedDecimal::new(mantissa, scale).expect("generated scale is supported");
        let text = value.to_string();
        let reparsed = FixedDecimal::parse_canonical(&text).expect("Display must be canonical");
        prop_assert_eq!(reparsed, value);
    }

    #[test]
    fn checked_add_and_sub_match_safe_i128_reference_arithmetic(
        left_mantissa in any::<i64>(),
        left_scale in 0_u32..=18,
        right_mantissa in any::<i64>(),
        right_scale in 0_u32..=18,
    ) {
        let left = FixedDecimal::new(i128::from(left_mantissa), left_scale)
            .expect("generated left value is representable");
        let right = FixedDecimal::new(i128::from(right_mantissa), right_scale)
            .expect("generated right value is representable");
        let scale = left.scale().max(right.scale());
        let aligned_left = left.mantissa() * 10_i128.pow(scale - left.scale());
        let aligned_right = right.mantissa() * 10_i128.pow(scale - right.scale());

        let expected_sum = FixedDecimal::new(aligned_left + aligned_right, scale)
            .expect("bounded reference sum is representable");
        let expected_difference = FixedDecimal::new(aligned_left - aligned_right, scale)
            .expect("bounded reference difference is representable");

        prop_assert_eq!(left.checked_add(right), Ok(expected_sum));
        prop_assert_eq!(right.checked_add(left), Ok(expected_sum));
        prop_assert_eq!(left.checked_sub(right), Ok(expected_difference));
    }

    #[test]
    fn three_factor_product_matches_bounded_exact_reference(
        first_mantissa in any::<i32>(),
        first_scale in 0_u32..=6,
        second_mantissa in any::<i32>(),
        second_scale in 0_u32..=6,
        third_mantissa in any::<i32>(),
        third_scale in 0_u32..=6,
    ) {
        let first = FixedDecimal::new(i128::from(first_mantissa), first_scale)
            .expect("generated first factor is representable");
        let second = FixedDecimal::new(i128::from(second_mantissa), second_scale)
            .expect("generated second factor is representable");
        let third = FixedDecimal::new(i128::from(third_mantissa), third_scale)
            .expect("generated third factor is representable");
        let expected = FixedDecimal::new(
            i128::from(first_mantissa)
                * i128::from(second_mantissa)
                * i128::from(third_mantissa),
            first_scale + second_scale + third_scale,
        )
        .expect("bounded exact reference product is representable");

        prop_assert_eq!(first.checked_product3(second, third), Ok(expected));
    }

    #[test]
    fn ordinary_parse_removes_redundant_fractional_zeroes(
        mantissa in any::<i64>(),
        scale in 0_u32..=18,
    ) {
        let value = FixedDecimal::new(i128::from(mantissa), scale)
            .expect("generated value is representable");
        let canonical = value.to_string();
        let redundant = if canonical.contains('.') {
            format!("{canonical}0")
        } else {
            format!("{canonical}.0")
        };
        prop_assert_eq!(FixedDecimal::parse(&redundant), Ok(value));
    }

    #[test]
    fn exact_rescale_preserves_value_or_reports_canonical_precision_loss(
        mantissa in any::<i64>(),
        scale in 0_u32..=18,
        extra_scale in 0_u32..=18,
    ) {
        let value = FixedDecimal::new(i128::from(mantissa), scale)
            .expect("generated value is representable");
        let larger_target = value.scale() + extra_scale;
        prop_assert_eq!(value.rescale_exact(larger_target), Ok(value));

        if value.scale() > 0 {
            prop_assert_eq!(
                value.rescale_exact(value.scale() - 1),
                Err(DecimalError::PrecisionLoss)
            );
        }
    }
}
