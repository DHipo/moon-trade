//! Brokerage fee model.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::money::round_money;

/// Fee rates as fractions (0.006 = 0.6 %).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeeModel {
    pub commission_rate: Decimal,
    pub market_fee_rate: Decimal,
    pub vat_rate: Decimal,
    #[serde(default)]
    pub min_commission: Decimal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeeBreakdown {
    pub commission: Decimal,
    pub vat: Decimal,
    pub market_fee: Decimal,
    pub total: Decimal,
}

impl FeeModel {
    pub fn zero() -> Self {
        Self {
            commission_rate: Decimal::ZERO,
            market_fee_rate: Decimal::ZERO,
            vat_rate: Decimal::ZERO,
            min_commission: Decimal::ZERO,
        }
    }

    /// Rates must be fractions in `[0, 1)`; the minimum commission must be `>= 0`.
    pub fn validate(&self) -> Result<(), CoreError> {
        let rate_ok = |r: Decimal| r >= Decimal::ZERO && r < Decimal::ONE;
        if !rate_ok(self.commission_rate)
            || !rate_ok(self.market_fee_rate)
            || !rate_ok(self.vat_rate)
        {
            return Err(CoreError::InvalidConfig(
                "las tasas de comisión deben estar en [0, 1)".into(),
            ));
        }
        if self.min_commission < Decimal::ZERO {
            return Err(CoreError::InvalidConfig(
                "min_commission debe ser >= 0".into(),
            ));
        }
        Ok(())
    }

    pub fn breakdown(&self, trade_value: Decimal) -> FeeBreakdown {
        if trade_value <= Decimal::ZERO {
            return FeeBreakdown {
                commission: Decimal::ZERO,
                vat: Decimal::ZERO,
                market_fee: Decimal::ZERO,
                total: Decimal::ZERO,
            };
        }
        let commission = round_money((trade_value * self.commission_rate).max(self.min_commission));
        let vat = round_money(commission * self.vat_rate);
        let market_fee = round_money(trade_value * self.market_fee_rate);
        FeeBreakdown {
            commission,
            vat,
            market_fee,
            total: commission + vat + market_fee,
        }
    }

    pub fn total(&self, trade_value: Decimal) -> Decimal {
        self.breakdown(trade_value).total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn model(min: Decimal) -> FeeModel {
        FeeModel {
            commission_rate: dec!(0.006),
            market_fee_rate: dec!(0.0008),
            vat_rate: dec!(0.21),
            min_commission: min,
        }
    }

    #[test]
    fn fees_breakdown_example() {
        let b = model(dec!(0)).breakdown(dec!(10000));
        assert_eq!(b.commission, dec!(60.00));
        assert_eq!(b.vat, dec!(12.60));
        assert_eq!(b.market_fee, dec!(8.00));
        assert_eq!(b.total, dec!(80.60));
    }

    #[test]
    fn fee_model_validate() {
        assert!(model(dec!(0)).validate().is_ok());
        assert!(FeeModel::zero().validate().is_ok());
        let mut m = model(dec!(0));
        m.commission_rate = dec!(1);
        assert!(m.validate().is_err());
        let mut m = model(dec!(0));
        m.vat_rate = dec!(-0.1);
        assert!(m.validate().is_err());
        assert!(model(dec!(-1)).validate().is_err());
    }

    #[test]
    fn fees_min_commission() {
        let m = model(dec!(100));
        let b = m.breakdown(dec!(1000));
        assert_eq!(b.commission, dec!(100));
        assert_eq!(b.vat, dec!(21.00));
        assert_eq!(b.market_fee, dec!(0.80));
        assert_eq!(b.total, dec!(121.80));
        assert_eq!(m.total(dec!(0)), dec!(0));
        assert_eq!(FeeModel::zero().total(dec!(500)), dec!(0));
    }
}
