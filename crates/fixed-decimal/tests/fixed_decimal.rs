use fixed_decimal::{DecimalError, FixedDecimal, Notional, Price, Quantity, Rate};

#[test]
fn domain_wrappers_enforce_sign_boundaries() {
    let negative = FixedDecimal::parse_canonical("-0.1").expect("negative decimal must parse");
    let zero = FixedDecimal::parse_canonical("0").expect("zero must parse");
    let positive = FixedDecimal::parse_canonical("0.1").expect("positive decimal must parse");

    assert_eq!(Price::new(zero), Err(DecimalError::NonPositive("price")));
    assert_eq!(
        Price::new(negative),
        Err(DecimalError::NonPositive("price"))
    );
    assert!(Price::new(positive).is_ok());

    assert_eq!(
        Quantity::new(negative),
        Err(DecimalError::Negative("quantity"))
    );
    assert_eq!(
        Notional::new(negative),
        Err(DecimalError::Negative("notional"))
    );
    assert!(Quantity::new(zero).is_ok());
    assert!(Notional::new(zero).is_ok());
    assert!(Rate::new(negative).is_ok());
}

#[test]
fn serde_boundary_accepts_only_canonical_decimal_strings() {
    let value: FixedDecimal =
        serde_json::from_str("\"12.34\"").expect("canonical decimal JSON must parse");
    assert_eq!(value.to_string(), "12.34");
    assert_eq!(
        serde_json::to_string(&value).expect("decimal JSON must serialize"),
        "\"12.34\""
    );

    for invalid in ["12.340", " 12.34", "+12.34"] {
        let encoded = format!("\"{invalid}\"");
        assert!(
            serde_json::from_str::<FixedDecimal>(&encoded).is_err(),
            "{encoded} must be rejected"
        );
    }
    assert!(serde_json::from_str::<FixedDecimal>("12.34").is_err());
}
