use rust_decimal::prelude::*;
use rust_decimal::{Decimal, RoundingStrategy};

/// Exact conversion using the shortest round-trip representation; `None` if not finite.
pub fn dec_from_f64(x: f64) -> Option<Decimal> {
    if !x.is_finite() {
        return None;
    }
    let s = format!("{x}");
    Decimal::from_str(&s)
        .or_else(|_| Decimal::from_scientific(&s))
        .ok()
}

pub fn dec_to_f64(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(f64::NAN)
}

/// 2 decimals, midpoint away from zero.
pub fn round_money(d: Decimal) -> Decimal {
    d.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
}

/// 4 decimals, midpoint away from zero.
pub fn round_price(d: Decimal) -> Decimal {
    d.round_dp_with_strategy(4, RoundingStrategy::MidpointAwayFromZero)
}

/// `floor(qty / lot) * lot`; a lot of 0 counts as 1; negative quantities give 0.
pub fn floor_to_lot(qty: Decimal, lot: u32) -> Decimal {
    if qty <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let lot = Decimal::from(lot.max(1));
    (qty / lot).floor() * lot
}

/// Clamps to `[0, 1]` and rounds to 8 decimals; non-finite becomes 0.
pub fn weight_to_dec(w: f64) -> Decimal {
    let w = if w.is_finite() {
        w.clamp(0.0, 1.0)
    } else {
        0.0
    };
    dec_from_f64(w)
        .unwrap_or(Decimal::ZERO)
        .round_dp_with_strategy(8, RoundingStrategy::MidpointAwayFromZero)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn dec_from_f64_is_exact_for_short_decimals() {
        assert_eq!(dec_from_f64(0.1), Some(dec!(0.1)));
        assert_eq!(dec_from_f64(f64::NAN), None);
        assert_eq!(dec_from_f64(f64::INFINITY), None);
    }

    #[test]
    fn dec_to_f64_works() {
        assert_eq!(dec_to_f64(dec!(1.5)), 1.5);
    }

    #[test]
    fn round_money_half_away() {
        assert_eq!(round_money(dec!(1.005)), dec!(1.01));
        assert_eq!(round_money(dec!(-1.005)), dec!(-1.01));
        assert_eq!(round_price(dec!(1.00005)), dec!(1.0001));
    }

    #[test]
    fn floor_to_lot_rounds_down() {
        assert_eq!(floor_to_lot(dec!(17), 5), dec!(15));
        assert_eq!(floor_to_lot(dec!(4), 5), dec!(0));
        assert_eq!(floor_to_lot(dec!(-3), 1), dec!(0));
        assert_eq!(floor_to_lot(dec!(7.9), 0), dec!(7));
    }

    #[test]
    fn weight_to_dec_clamps() {
        assert_eq!(weight_to_dec(1.5), dec!(1));
        assert_eq!(weight_to_dec(-0.2), dec!(0));
        assert_eq!(weight_to_dec(0.123456789123), dec!(0.12345679));
        assert_eq!(weight_to_dec(f64::NAN), dec!(0));
    }
}
