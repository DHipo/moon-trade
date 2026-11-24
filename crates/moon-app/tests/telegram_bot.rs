mod common;

use std::collections::BTreeMap;

use common::{Harness, TODAY, clock_time};
use moon_app::config::TradingMode;
use moon_app::engine::{MonthlyOutcome, STATE_RISK_INDEX};
use moon_app::telegram_bot::{BotAction, handle_incoming};
use moon_core::risk_index::RiskIndex;
use moon_telegram::{Incoming, IncomingKind};
use rust_decimal_macros::dec;

fn command(name: &str) -> Incoming {
    Incoming {
        update_id: 1,
        kind: IncomingKind::Command {
            name: name.into(),
            args: String::new(),
        },
    }
}

fn callback(data: &str) -> Incoming {
    Incoming {
        update_id: 2,
        kind: IncomingKind::Callback {
            id: "cb1".into(),
            data: data.into(),
            message_id: Some(77),
        },
    }
}

#[tokio::test]
async fn estado_returns_status() {
    let h = Harness::paper(dec!(1000000));
    let BotAction::Reply(text) = handle_incoming(&h.engine, &command("estado")).await else {
        panic!("expected a reply");
    };
    assert!(text.contains("Órdenes abiertas"), "{text}");
}

#[tokio::test]
async fn pausa_and_reanudar_toggle_state() {
    let h = Harness::paper(dec!(1000000));
    let BotAction::Reply(text) = handle_incoming(&h.engine, &command("pausa")).await else {
        panic!("expected a reply");
    };
    assert!(text.contains("emergencia"), "{text}");
    assert!(h.engine.is_paused().expect("paused"));
    handle_incoming(&h.engine, &command("reanudar")).await;
    assert!(!h.engine.is_paused().expect("paused"));
    let BotAction::Reply(help) = handle_incoming(&h.engine, &command("loquesea")).await else {
        panic!("expected help");
    };
    assert!(help.contains("/estado") && help.contains("/pausa") && help.contains("/reanudar"));
}

fn command_with(name: &str, args: &str) -> Incoming {
    Incoming {
        update_id: 3,
        kind: IncomingKind::Command {
            name: name.into(),
            args: args.into(),
        },
    }
}

#[tokio::test]
async fn aceptar_accepts_a_pending_ratio_change_and_reports_errors() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_position("AAA", dec!(100));
    let index = RiskIndex {
        level: 1.0,
        holdings: BTreeMap::from([("AAA".to_string(), dec!(100))]),
        prices_usd: BTreeMap::from([("AAA".to_string(), dec!(20000) / dec!(1200))]),
        at: Some(clock_time()),
        suspicions: BTreeMap::from([("AAA".to_string(), 1)]),
        ..RiskIndex::default()
    };
    let key = h.engine.state_key(STATE_RISK_INDEX);
    h.store
        .lock()
        .expect("lock")
        .set_state(&key, &index)
        .expect("index");

    let BotAction::Reply(none) = handle_incoming(&h.engine, &command_with("aceptar", "")).await
    else {
        panic!("expected a reply");
    };
    assert!(none.contains("No se pudo aceptar"), "{none}");
    let BotAction::Reply(wrong) = handle_incoming(&h.engine, &command_with("aceptar", "ZZZ")).await
    else {
        panic!("expected a reply");
    };
    assert!(wrong.contains("No hay un posible cambio"), "{wrong}");
    let BotAction::Reply(ok) = handle_incoming(&h.engine, &command_with("aceptar", "aaa")).await
    else {
        panic!("expected a reply");
    };
    assert!(ok.contains("Aceptado") && ok.contains("AAA"), "{ok}");
    // The risk check ran right away and counted the (halved) price as a real move.
    assert!(ok.contains("0.5000"), "{ok}");
    let help = handle_incoming(&h.engine, &command("ayuda")).await;
    assert!(matches!(help, BotAction::Reply(t) if t.contains("/aceptar")));
}

#[tokio::test]
async fn approve_callback_executes_proposal() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(1200000));
    let MonthlyOutcome::ProposalSent(id) = h.engine.run_monthly(TODAY).await.expect("monthly")
    else {
        panic!("expected a proposal");
    };
    let action = handle_incoming(&h.engine, &callback(&format!("approve:{id}"))).await;
    let BotAction::CallbackDone {
        callback_id,
        message_id,
        text,
    } = action
    else {
        panic!("expected CallbackDone");
    };
    assert_eq!(callback_id, "cb1");
    assert_eq!(message_id, Some(77));
    assert!(!text.is_empty());
    assert!(!h.fake().places().is_empty());
}

#[tokio::test]
async fn malformed_callback_is_rejected() {
    let h = Harness::paper(dec!(1000000));
    for data in ["approve:not-a-uuid", "garbage", "delete:abc", "approve"] {
        let action = handle_incoming(&h.engine, &callback(data)).await;
        assert_eq!(
            action,
            BotAction::CallbackDone {
                callback_id: "cb1".into(),
                message_id: Some(77),
                text: "Acción inválida.".into()
            },
            "{data}"
        );
    }
}

#[tokio::test]
async fn other_updates_are_ignored() {
    let h = Harness::paper(dec!(1000000));
    let other = Incoming {
        update_id: 3,
        kind: IncomingKind::Other,
    };
    assert_eq!(handle_incoming(&h.engine, &other).await, BotAction::Ignore);
}
