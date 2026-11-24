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
    cfg.telegram.enabled = true;
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
        telegram_token: Some("t".into()),
        telegram_chat_id: Some(1),
    };
    validate_live(&cfg, &ok).expect("secrets present");
    // paper never needs secrets
    assert!(validate_live(&example(), &none).is_ok());
}

fn live_ready() -> (AppConfig, Secrets) {
    let mut cfg = example();
    cfg.mode = TradingMode::LiveConfirm;
    cfg.live.enabled = true;
    cfg.telegram.enabled = true;
    let secrets = Secrets {
        iol_username: Some("u".into()),
        iol_password: Some("p".into()),
        telegram_token: Some("t".into()),
        telegram_chat_id: Some(1),
    };
    (cfg, secrets)
}

#[test]
fn live_requires_iol_quotes() {
    let (mut cfg, secrets) = live_ready();
    validate_live(&cfg, &secrets).expect("ready");
    cfg.execution.quote_source = moon_app::config::QuoteSourceKind::Yahoo;
    let err = validate_live(&cfg, &secrets)
        .expect_err("yahoo quotes")
        .to_string();
    assert!(err.contains("quote_source"), "{err}");
}

#[test]
fn live_requires_telegram() {
    let (mut cfg, mut secrets) = live_ready();
    secrets.telegram_chat_id = None;
    let err = validate_live(&cfg, &secrets)
        .expect_err("no chat id")
        .to_string();
    assert!(err.contains("TELEGRAM"), "{err}");
    secrets.telegram_chat_id = Some(1);
    secrets.telegram_token = None;
    assert!(validate_live(&cfg, &secrets).is_err());
    secrets.telegram_token = Some("t".into());
    cfg.telegram.enabled = false;
    let err = validate_live(&cfg, &secrets)
        .expect_err("disabled")
        .to_string();
    assert!(err.contains("[telegram]"), "{err}");
}

fn example_text() -> String {
    std::fs::read_to_string("../../moon.example.toml").expect("example")
}

fn load_text(text: &str) -> anyhow::Result<AppConfig> {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("moon.toml");
    std::fs::write(&path, text).expect("write");
    AppConfig::load(&path)
}

#[test]
fn unknown_config_keys_are_rejected_everywhere() {
    let base = example_text();
    // top level, a section, a nested struct and a flattened-before section (backtest)
    let cases = [
        ("mode = \"paper\"", "mode = \"paper\"\nmodo_tipeado = 1"),
        ("[risk]", "[risk]\nmax_drawdwn = 0.1"),
        ("[execution]", "[execution]\nlimit_tolerence = 0.1"),
        ("[backtest]", "[backtest]\ncash_usd_retun = -0.1"),
        ("[backtest.fees]", "[backtest.fees]\ncommision_rate = 0.1"),
        ("[telegram]", "[telegram]\nenabeld = true"),
        ("kind = \"faber_sma\"", "kind = \"faber_sma\"\nmonts = 3"),
    ];
    for (needle, replacement) in cases {
        assert!(base.contains(needle), "{needle}");
        let text = base.replacen(needle, replacement, 1);
        let err = load_text(&text).expect_err(needle);
        assert!(
            format!("{err:#}").contains("unknown field"),
            "{needle}: {err:#}"
        );
    }
    load_text(&base).expect("the example itself stays valid");
}

#[test]
fn backtest_cash_return_is_configurable() {
    let text = example_text().replacen("cash_usd_return = 0.0", "cash_usd_return = -0.2", 1);
    let cfg = load_text(&text).expect("loads");
    assert!((cfg.backtest.config.cash_usd_return + 0.2).abs() < 1e-12);
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
    for s in [
        "CAGR",
        "Máxima caída",
        "Sharpe",
        "Aportes netos (USD)",
        "Rotación anual",
    ] {
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

#[test]
fn cash_refuge_warning_depends_on_the_cash_return() {
    let mut cfg = example();
    let w = moon_app::report::cash_refuge_warning(&cfg).expect("CASH refuge at 0 %");
    assert!(w.contains("backtest.cash_usd_return"), "{w}");
    assert!(w.contains("sin pérdida"), "{w}");
    cfg.backtest.config.cash_usd_return = -0.1;
    assert!(moon_app::report::cash_refuge_warning(&cfg).is_none());
}

#[test]
fn emergency_deep_limit_discount_defaults_and_is_validated() {
    let mut cfg = example();
    assert_eq!(cfg.execution.emergency_deep_limit_discount, dec!(0.10));
    cfg.execution.emergency_deep_limit_discount = dec!(0.5);
    cfg.validate().expect("valid");
    cfg.execution.emergency_deep_limit_discount = dec!(0.01);
    cfg.validate().expect("valid");
    for bad in [dec!(0), dec!(-0.1), dec!(0.51)] {
        cfg.execution.emergency_deep_limit_discount = bad;
        let err = cfg.validate().expect_err("invalid").to_string();
        assert!(err.contains("emergency_deep_limit_discount"), "{err}");
    }
}

#[test]
fn emergency_reprice_and_wait_have_defaults_and_are_validated() {
    let mut cfg = example();
    assert_eq!(cfg.execution.emergency_reprice_minutes, 10);
    assert_eq!(cfg.execution.emergency_fill_wait_seconds, 120);
    cfg.execution.emergency_reprice_minutes = 1;
    cfg.validate().expect("1 is the lower bound");
    cfg.execution.emergency_reprice_minutes = 60;
    cfg.validate().expect("60 is the upper bound");
    cfg.execution.emergency_reprice_minutes = 0;
    let err = cfg.validate().expect_err("zero").to_string();
    assert!(err.contains("emergency_reprice_minutes"), "{err}");
    cfg.execution.emergency_reprice_minutes = 61;
    assert!(cfg.validate().is_err());
    cfg.execution.emergency_reprice_minutes = 10;

    cfg.execution.emergency_fill_wait_seconds = 0;
    cfg.validate().expect("0 disables the wait");
    cfg.execution.emergency_fill_wait_seconds = 600;
    cfg.validate().expect("600 is the upper bound");
    cfg.execution.emergency_fill_wait_seconds = 601;
    let err = cfg.validate().expect_err("too long").to_string();
    assert!(err.contains("emergency_fill_wait_seconds"), "{err}");
}
