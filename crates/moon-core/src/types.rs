use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// Declares a fieldless enum with serde names, `as_str` and `FromStr`.
macro_rules! str_enum {
    ($(#[$meta:meta])* $vis:vis enum $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        $vis enum $name {
            $(#[serde(rename = $s)] $variant),+
        }

        impl $name {
            /// The serde name of the variant.
            pub fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $s),+
                }
            }
        }

        impl std::str::FromStr for $name {
            type Err = CoreError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($s => Ok(Self::$variant),)+
                    other => Err(CoreError::Parse(format!(
                        "{}: valor desconocido '{}'",
                        stringify!($name),
                        other
                    ))),
                }
            }
        }
    };
}
pub(crate) use str_enum;

str_enum! {
    pub enum Side { Buy => "buy", Sell => "sell" }
}

str_enum! {
    pub enum ExecMode { Paper => "paper", Live => "live" }
}

str_enum! {
    pub enum OrderReason {
        Monthly => "monthly",
        Deposit => "deposit",
        Emergency => "emergency",
        Manual => "manual",
    }
}

str_enum! {
    pub enum OrderStatus {
        PendingSend => "pending_send",
        Sent => "sent",
        PartiallyFilled => "partially_filled",
        Filled => "filled",
        Cancelled => "cancelled",
        Rejected => "rejected",
        Failed => "failed",
    }
}

impl OrderStatus {
    /// True when no further state changes are expected.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Cancelled | Self::Rejected | Self::Failed
        )
    }
}

str_enum! {
    pub enum DataSource { Yahoo => "yahoo", Iol => "iol" }
}

str_enum! {
    /// Settlement term.
    pub enum Term { T0 => "T0", T1 => "T1", T2 => "T2" }
}

str_enum! {
    pub enum OrderType { Limit => "limit", Market => "market" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_round_trip() {
        for v in [Side::Buy, Side::Sell] {
            assert_eq!(v.as_str().parse::<Side>().unwrap(), v);
        }
        for v in [ExecMode::Paper, ExecMode::Live] {
            assert_eq!(v.as_str().parse::<ExecMode>().unwrap(), v);
        }
        for v in [
            OrderReason::Monthly,
            OrderReason::Deposit,
            OrderReason::Emergency,
            OrderReason::Manual,
        ] {
            assert_eq!(v.as_str().parse::<OrderReason>().unwrap(), v);
        }
        for v in [
            OrderStatus::PendingSend,
            OrderStatus::Sent,
            OrderStatus::PartiallyFilled,
            OrderStatus::Filled,
            OrderStatus::Cancelled,
            OrderStatus::Rejected,
            OrderStatus::Failed,
        ] {
            assert_eq!(v.as_str().parse::<OrderStatus>().unwrap(), v);
        }
        for v in [DataSource::Yahoo, DataSource::Iol] {
            assert_eq!(v.as_str().parse::<DataSource>().unwrap(), v);
        }
        for v in [Term::T0, Term::T1, Term::T2] {
            assert_eq!(v.as_str().parse::<Term>().unwrap(), v);
        }
        for v in [OrderType::Limit, OrderType::Market] {
            assert_eq!(v.as_str().parse::<OrderType>().unwrap(), v);
        }
        assert_eq!(serde_json::to_string(&Term::T1).unwrap(), "\"T1\"");
        assert_eq!(serde_json::to_string(&Side::Buy).unwrap(), "\"buy\"");
        assert!("nope".parse::<Side>().is_err());
    }

    #[test]
    fn terminal_statuses() {
        assert!(OrderStatus::Filled.is_terminal());
        assert!(OrderStatus::Failed.is_terminal());
        assert!(!OrderStatus::Sent.is_terminal());
        assert!(!OrderStatus::PartiallyFilled.is_terminal());
    }
}
