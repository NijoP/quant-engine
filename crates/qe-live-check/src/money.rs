//! Decimal accounting for balances and reconciliation. Exchange responses
//! arrive as lenient `f64`; they are converted once here and rounded to 8 dp.
use rust_decimal::Decimal;

pub const DP: u32 = 8;

/// `None` for NaN / infinity: a missing number is never treated as zero.
pub fn dec(x: f64) -> Option<Decimal> {
    if !x.is_finite() {
        return None;
    }
    Decimal::from_f64_retain(x).map(|d| d.round_dp(DP).normalize())
}

/// Round `x` DOWN to a multiple of `step` (step > 0).
pub fn floor_to(x: Decimal, step: Decimal) -> Decimal {
    if step <= Decimal::ZERO {
        return x;
    }
    (x / step).floor() * step
}

/// Round `x` UP to a multiple of `step` (step > 0).
pub fn ceil_to(x: Decimal, step: Decimal) -> Decimal {
    if step <= Decimal::ZERO {
        return x;
    }
    (x / step).ceil() * step
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn conversions() {
        assert_eq!(dec(f64::NAN), None);
        assert_eq!(dec(0.1 + 0.2).unwrap(), Decimal::from_str("0.3").unwrap());
        let s = Decimal::from_str("0.001").unwrap();
        assert_eq!(
            ceil_to(Decimal::from_str("0.0021").unwrap(), s),
            Decimal::from_str("0.003").unwrap()
        );
        assert_eq!(
            floor_to(Decimal::from_str("0.0029").unwrap(), s),
            Decimal::from_str("0.002").unwrap()
        );
    }
}
