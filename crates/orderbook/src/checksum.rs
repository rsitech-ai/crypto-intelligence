use crate::{
    BookError,
    book::{L2Book, L3Book, L3Order, L3Side},
};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use std::collections::BTreeSet;

const KRAKEN_CHECKSUM_DEPTH: usize = 10;
const MAX_DECIMAL_TEXT_BYTES: usize = 128;

/// One exact source-formatted price/quantity pair used in a venue checksum.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactChecksumLevel {
    price_text: String,
    quantity_text: String,
    price: Price,
    quantity: Quantity,
}

impl ExactChecksumLevel {
    pub fn new(price: impl Into<String>, quantity: impl Into<String>) -> Result<Self, BookError> {
        let price_text = price.into();
        let quantity_text = quantity.into();
        if !valid_decimal_text(&price_text) || !valid_decimal_text(&quantity_text) {
            return Err(BookError::InvalidChecksumInput);
        }
        let price = Price::new(
            FixedDecimal::parse(&price_text).map_err(|_| BookError::InvalidChecksumInput)?,
        )
        .map_err(|_| BookError::InvalidChecksumInput)?;
        let quantity = Quantity::new(
            FixedDecimal::parse(&quantity_text).map_err(|_| BookError::InvalidChecksumInput)?,
        )
        .map_err(|_| BookError::InvalidChecksumInput)?;
        if quantity.value().is_zero() {
            return Err(BookError::InvalidChecksumInput);
        }
        Ok(Self {
            price_text,
            quantity_text,
            price,
            quantity,
        })
    }

    pub fn price_text(&self) -> &str {
        &self.price_text
    }

    pub fn quantity_text(&self) -> &str {
        &self.quantity_text
    }

    pub const fn price(&self) -> Price {
        self.price
    }

    pub const fn quantity(&self) -> Quantity {
        self.quantity
    }
}

/// Source-exact Kraken WebSocket v2 top-ten CRC32 observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KrakenV2Checksum {
    expected: u32,
    asks: Vec<ExactChecksumLevel>,
    bids: Vec<ExactChecksumLevel>,
}

impl KrakenV2Checksum {
    pub fn new(
        expected: u32,
        asks: Vec<ExactChecksumLevel>,
        bids: Vec<ExactChecksumLevel>,
    ) -> Result<Self, BookError> {
        if asks.len() > KRAKEN_CHECKSUM_DEPTH
            || bids.len() > KRAKEN_CHECKSUM_DEPTH
            || asks.is_empty() && bids.is_empty()
            || !asks.windows(2).all(|pair| pair[0].price < pair[1].price)
            || !bids.windows(2).all(|pair| pair[0].price > pair[1].price)
        {
            return Err(BookError::InvalidChecksumInput);
        }
        Ok(Self {
            expected,
            asks,
            bids,
        })
    }

    pub const fn expected(&self) -> u32 {
        self.expected
    }

    pub fn computed(&self) -> u32 {
        let mut bytes = Vec::with_capacity(
            self.asks
                .len()
                .saturating_add(self.bids.len())
                .saturating_mul(MAX_DECIMAL_TEXT_BYTES),
        );
        for level in self.asks.iter().chain(&self.bids) {
            append_checksum_decimal(&mut bytes, &level.price_text);
            append_checksum_decimal(&mut bytes, &level.quantity_text);
        }
        ieee_crc32(&bytes)
    }

    pub fn matches_expected(&self) -> bool {
        self.computed() == self.expected
    }

    pub(crate) fn verify_book(&self, book: &L2Book) -> Result<bool, BookError> {
        let expected_ask_count = book.ask_levels().min(KRAKEN_CHECKSUM_DEPTH);
        let expected_bid_count = book.bid_levels().min(KRAKEN_CHECKSUM_DEPTH);
        if self.asks.len() != expected_ask_count || self.bids.len() != expected_bid_count {
            return Err(BookError::InvalidChecksumInput);
        }
        let asks_match = self
            .asks
            .iter()
            .zip(book.top_asks(KRAKEN_CHECKSUM_DEPTH))
            .all(|(source, (price, quantity))| {
                source.price == price && source.quantity == quantity
            });
        let bids_match = self
            .bids
            .iter()
            .zip(book.top_bids(KRAKEN_CHECKSUM_DEPTH))
            .all(|(source, (price, quantity))| {
                source.price == price && source.quantity == quantity
            });
        if asks_match && bids_match {
            Ok(self.matches_expected())
        } else {
            Err(BookError::InvalidChecksumInput)
        }
    }
}

/// One source-exact L3 order retained for Kraken queue checksum validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactL3ChecksumOrder {
    order: L3Order,
    price_text: String,
    quantity_text: String,
}

impl ExactL3ChecksumOrder {
    pub fn new(
        order: L3Order,
        price: impl Into<String>,
        quantity: impl Into<String>,
    ) -> Result<Self, BookError> {
        let price_text = price.into();
        let quantity_text = quantity.into();
        if !valid_decimal_text(&price_text) || !valid_decimal_text(&quantity_text) {
            return Err(BookError::InvalidChecksumInput);
        }
        let parsed_price = Price::new(
            FixedDecimal::parse(&price_text).map_err(|_| BookError::InvalidChecksumInput)?,
        )
        .map_err(|_| BookError::InvalidChecksumInput)?;
        let parsed_quantity = Quantity::new(
            FixedDecimal::parse(&quantity_text).map_err(|_| BookError::InvalidChecksumInput)?,
        )
        .map_err(|_| BookError::InvalidChecksumInput)?;
        if parsed_price != order.price()
            || parsed_quantity != order.quantity()
            || parsed_quantity.value().is_zero()
        {
            return Err(BookError::InvalidChecksumInput);
        }
        Ok(Self {
            order,
            price_text,
            quantity_text,
        })
    }

    pub const fn order(&self) -> &L3Order {
        &self.order
    }

    pub fn price_text(&self) -> &str {
        &self.price_text
    }

    pub fn quantity_text(&self) -> &str {
        &self.quantity_text
    }
}

/// Source-exact Kraken WebSocket v2 L3 CRC32 observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KrakenV2L3Checksum {
    expected: u32,
    asks: Vec<ExactL3ChecksumOrder>,
    bids: Vec<ExactL3ChecksumOrder>,
}

impl KrakenV2L3Checksum {
    pub fn new(
        expected: u32,
        asks: Vec<ExactL3ChecksumOrder>,
        bids: Vec<ExactL3ChecksumOrder>,
    ) -> Result<Self, BookError> {
        if asks.is_empty() && bids.is_empty()
            || !valid_l3_checksum_side(&asks, L3Side::Ask)
            || !valid_l3_checksum_side(&bids, L3Side::Bid)
        {
            return Err(BookError::InvalidChecksumInput);
        }
        let unique_orders = asks
            .iter()
            .chain(&bids)
            .map(|order| order.order.id())
            .collect::<BTreeSet<_>>();
        if unique_orders.len() != asks.len().saturating_add(bids.len()) {
            return Err(BookError::InvalidChecksumInput);
        }
        Ok(Self {
            expected,
            asks,
            bids,
        })
    }

    pub const fn expected(&self) -> u32 {
        self.expected
    }

    pub fn computed(&self) -> u32 {
        let mut bytes = Vec::with_capacity(
            self.asks
                .len()
                .saturating_add(self.bids.len())
                .saturating_mul(MAX_DECIMAL_TEXT_BYTES),
        );
        for order in self.asks.iter().chain(&self.bids) {
            append_checksum_decimal(&mut bytes, &order.price_text);
            append_checksum_decimal(&mut bytes, &order.quantity_text);
        }
        ieee_crc32(&bytes)
    }

    pub fn matches_expected(&self) -> bool {
        self.computed() == self.expected
    }

    pub(crate) fn verify_book(&self, book: &L3Book) -> Result<bool, BookError> {
        let asks = book.checksum_orders(L3Side::Ask);
        let bids = book.checksum_orders(L3Side::Bid);
        if !l3_orders_match(&self.asks, &asks) || !l3_orders_match(&self.bids, &bids) {
            return Err(BookError::InvalidChecksumInput);
        }
        Ok(self.matches_expected())
    }
}

fn valid_l3_checksum_side(orders: &[ExactL3ChecksumOrder], side: L3Side) -> bool {
    if orders.iter().any(|order| order.order.side() != side) {
        return false;
    }
    let distinct_levels = orders
        .iter()
        .map(|order| order.order.price())
        .collect::<BTreeSet<_>>();
    if distinct_levels.len() > KRAKEN_CHECKSUM_DEPTH {
        return false;
    }
    orders.windows(2).all(|pair| {
        let left = pair[0].order();
        let right = pair[1].order();
        let prices_ordered = match side {
            L3Side::Ask => left.price() <= right.price(),
            L3Side::Bid => left.price() >= right.price(),
        };
        prices_ordered
            && (left.price() != right.price()
                || (left.priority_ns(), left.queue_order())
                    < (right.priority_ns(), right.queue_order()))
    })
}

fn l3_orders_match(exact: &[ExactL3ChecksumOrder], actual: &[L3Order]) -> bool {
    exact.len() == actual.len()
        && exact
            .iter()
            .zip(actual)
            .all(|(source, normalized)| source.order() == normalized)
}

fn valid_decimal_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_DECIMAL_TEXT_BYTES
        && value.trim() == value
        && value.is_ascii()
        && !value.starts_with(['-', '+'])
        && !value.contains(['e', 'E'])
}

fn append_checksum_decimal(output: &mut Vec<u8>, value: &str) {
    let mut wrote_nonzero = false;
    for byte in value.bytes().filter(|byte| *byte != b'.') {
        if wrote_nonzero || byte != b'0' {
            output.push(byte);
            wrote_nonzero = true;
        }
    }
    if !wrote_nonzero {
        output.push(b'0');
    }
}

fn ieee_crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{ExactChecksumLevel, ExactL3ChecksumOrder, KrakenV2Checksum, KrakenV2L3Checksum};
    use crate::{L3Order, L3OrderId, L3Side};
    use fixed_decimal::{FixedDecimal, Price, Quantity};

    #[test]
    fn official_kraken_websocket_v2_example_matches() {
        let bids = [
            ("45283.5", "0.10000000"),
            ("45283.4", "1.54582015"),
            ("45282.1", "0.10000000"),
            ("45281.0", "0.10000000"),
            ("45280.3", "1.54592586"),
            ("45279.0", "0.07990000"),
            ("45277.6", "0.03310103"),
            ("45277.5", "0.30000000"),
            ("45277.3", "1.54602737"),
            ("45276.6", "0.15445238"),
        ];
        let asks = [
            ("45285.2", "0.00100000"),
            ("45286.4", "1.54571953"),
            ("45286.6", "1.54571109"),
            ("45289.6", "1.54560911"),
            ("45290.2", "0.15890660"),
            ("45291.8", "1.54553491"),
            ("45294.7", "0.04454749"),
            ("45296.1", "0.35380000"),
            ("45297.5", "0.09945542"),
            ("45299.5", "0.18772827"),
        ];
        let checksum = KrakenV2Checksum::new(
            3_310_070_434,
            asks.into_iter()
                .map(|(price, quantity)| ExactChecksumLevel::new(price, quantity).unwrap())
                .collect(),
            bids.into_iter()
                .map(|(price, quantity)| ExactChecksumLevel::new(price, quantity).unwrap())
                .collect(),
        )
        .unwrap();

        assert_eq!(checksum.computed(), 3_310_070_434);
        assert!(checksum.matches_expected());
    }

    #[test]
    fn official_kraken_l3_websocket_v2_example_matches() {
        let asks = [
            ("44939.5", "4.52308393"),
            ("44939.5", "0.00111261"),
            ("44939.5", "0.00100000"),
            ("44939.5", "0.01000000"),
            ("44950.0", "0.10334926"),
            ("44953.0", "0.00064537"),
            ("44955.0", "0.00250000"),
            ("44959.6", "0.35630000"),
            ("44959.6", "0.35630000"),
            ("44960.1", "0.00338072"),
            ("44960.2", "0.88967575"),
            ("44967.0", "3.14392283"),
            ("44978.5", "0.06778960"),
            ("44979.2", "0.35630000"),
        ];
        let bids = [
            ("44939.4", "0.88968699"),
            ("44939.4", "0.45210000"),
            ("44939.4", "0.10000000"),
            ("44939.4", "0.14296323"),
            ("44939.4", "0.25000000"),
            ("44939.4", "0.10292988"),
            ("44939.4", "0.33880000"),
            ("44939.4", "1.28140860"),
            ("44937.1", "0.03346877"),
            ("44934.7", "0.35630000"),
            ("44930.2", "0.22734299"),
            ("44930.2", "0.01000000"),
            ("44930.2", "0.05550000"),
            ("44930.2", "0.70000000"),
            ("44930.2", "0.15000000"),
            ("44928.0", "0.00105240"),
            ("44919.6", "0.33870000"),
            ("44919.5", "0.07610000"),
            ("44912.0", "0.35630000"),
            ("44909.7", "0.06690000"),
            ("44901.9", "0.00088982"),
        ];
        let checksum = KrakenV2L3Checksum::new(
            1_063_832_831,
            exact_l3_side(&asks, L3Side::Ask, "ask"),
            exact_l3_side(&bids, L3Side::Bid, "bid"),
        )
        .unwrap();

        assert_eq!(checksum.computed(), 1_063_832_831);
        assert!(checksum.matches_expected());
    }

    fn exact_l3_side(
        values: &[(&str, &str)],
        side: L3Side,
        prefix: &str,
    ) -> Vec<ExactL3ChecksumOrder> {
        values
            .iter()
            .enumerate()
            .map(|(index, (price_text, quantity_text))| {
                let ordinal = u64::try_from(index).unwrap();
                let order = L3Order::new(
                    L3OrderId::new(format!("{prefix}-{index}")).unwrap(),
                    side,
                    Price::new(FixedDecimal::parse(price_text).unwrap()).unwrap(),
                    Quantity::new(FixedDecimal::parse(quantity_text).unwrap()).unwrap(),
                    ordinal,
                    ordinal,
                )
                .unwrap();
                ExactL3ChecksumOrder::new(order, *price_text, *quantity_text).unwrap()
            })
            .collect()
    }
}
