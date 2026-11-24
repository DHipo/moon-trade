use moon_app::config::{AppConfig, Secrets, TradingMode, validate_live};
use moon_app::report::format_metrics_table;
use moon_core::asset::Role;
use moon_core::metrics::nav_metrics;
use rust_decimal_macros::dec;

fn example() -> AppConfig {
    AppConfig::load(std::path::Path::new("../../moon.example.toml")).expect("example loads")
}

#[test]
fn example_config_parses_and_validates() {
    let cfg = example();
    assert_eq!(cfg.mode, TradingMode::Paper);
    assert_eq!(
        cfg.assets.iter().filter(|a| a.role == Role::Risky).count(),
        5
    );
    assert!(cfg.universe().expect("universe").refuge().id.is_cash());
    assert_eq!(cfg.fees.commission_rate, dec!(0.006));
    assert_eq!(cfg.backtest.config.fees.commission_rate, dec!(0.006));
    assert_eq!(cfg.execution.slippage_rate, dec!(0.003));
    assert_eq!(cfg.execution.limit_tolerance, dec!(0.005));
    assert!(!cfg.is_live());
}

#[test]
fn live_requires_double_opt_in() {
    let mut cfg = example();
    cfg.mode = TradingMode::LiveAuto;
    let err = cfg.validate().expect_err("must fail").to_string();
    assert!(err.contains("doble confirmación"), "{err}");

    cfg.live.enabled = true;
    cfg.validate().expect("opted in");
    let none = Secrets {
        iol_username: None,
        iol_password: None,
        telegram_token: None,
        telegram_chat_id: None,
    };
    assert!(validate_live(&cfg, &none).is_err());
    let ok = Secrets {
        iol_username: Some("u".into()),
        iol_password: Some("p".into()),
        ..none
    };
    validate_live(&cfg, &ok).expect("secrets present");
    // paper never needs secrets
    assert!(validate_live(&example(), &ok).is_ok());
}

#[test]
fn validation_rejects_bad_values() {
    let mut cfg = example();
    cfg.execution.slippage_rate = dec!(0.2);
    assert!(cfg.validate().is_err());
    let mut cfg = example();
    cfg.fx.min_pairs = 9;
    assert!(cfg.validate().is_err());
    let mut cfg = example();
    cfg.fees.commission_rate = dec!(2);
    assert!(cfg.validate().is_err());
}

#[test]
fn config_hash_is_stable_and_sensitive() {
    let a = example();
    let b = example();
    assert_eq!(a.config_hash(), b.config_hash());
    let mut c = example();
    c.risk.max_weight = 0.25;
    assert_ne!(a.config_hash(), c.config_hash());
}

#[test]
fn secrets_debug_hides_values() {
    let s = Secrets {
        iol_username: Some("alice".into()),
        iol_password: Some("hunter2".into()),
        telegram_token: Some("123:ABC".into()),
        telegram_chat_id: Some(99),
    };
    let dbg = format!("{s:?}");
    for secret in ["alice", "hunter2", "123:ABC", "99"] {
        assert!(!dbg.contains(secret), "{dbg}");
    }
    assert!(dbg.contains("***"));
}

#[test]
fn metrics_table_is_spanish() {
    let navs: Vec<_> = (0..400)
        .map(|i| {
            (
                moon_core::testutil::d(2020, 1, 1) + chrono::Duration::days(i),
                1.0 + i as f64 * 0.001,
            )
        })
        .collect();
    let m = nav_metrics(&navs, 0.0);
    let t = format_metrics_table(&m, Some(&m));
    for s in ["CAGR", "Máxima caída", "Sharpe"] {
        assert!(t.contains(s), "{t}");
    }
}

#[test]
fn emergency_limit_discount_defaults_and_is_validated() {
    let mut cfg = example();
    assert_eq!(cfg.execution.emergency_limit_discount, dec!(0.03));
    cfg.execution.emergency_limit_discount = dec!(0.2);
    cfg.validate().expect("0.2 is the upper bound");
    cfg.execution.emergency_limit_discount = dec!(0.21);
    let err = cfg.validate().expect_err("too large").to_string();
    assert!(err.contains("emergency_limit_discount"), "{err}");
    cfg.execution.emergency_limit_discount = dec!(-0.01);
    assert!(cfg.validate().is_err());
}
