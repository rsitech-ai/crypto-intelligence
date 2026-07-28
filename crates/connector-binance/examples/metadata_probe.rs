use std::{env, fs, process::ExitCode};

use connector_binance::{BinanceMarket, parse_exchange_info_report};

fn main() -> ExitCode {
    let mut arguments = env::args_os().skip(1);
    let Some(market) = arguments.next() else {
        eprintln!("usage: metadata_probe <spot|usdm> <exchange-info.json>");
        return ExitCode::FAILURE;
    };
    let Some(path) = arguments.next() else {
        eprintln!("usage: metadata_probe <spot|usdm> <exchange-info.json>");
        return ExitCode::FAILURE;
    };
    if arguments.next().is_some() {
        eprintln!("usage: metadata_probe <spot|usdm> <exchange-info.json>");
        return ExitCode::FAILURE;
    }
    let market = match market.to_str() {
        Some("spot") => BinanceMarket::Spot,
        Some("usdm") => BinanceMarket::UsdMarginedPerpetual,
        _ => {
            eprintln!("market must be spot or usdm");
            return ExitCode::FAILURE;
        }
    };
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(error) => {
            eprintln!("failed to read {}: {error}", path.to_string_lossy());
            return ExitCode::FAILURE;
        }
    };
    match parse_exchange_info_report(market, &raw) {
        Ok(report) => {
            println!(
                "active={} pending={} lifecycle={} skipped_symbols={} skipped_contracts={} skipped_inactive={}",
                report.instruments().len(),
                report.pending_instruments().len(),
                report.lifecycle().len(),
                report.skipped_unsupported_symbols(),
                report.skipped_unsupported_contracts(),
                report.skipped_inactive(),
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("metadata rejected: {error}");
            ExitCode::FAILURE
        }
    }
}
