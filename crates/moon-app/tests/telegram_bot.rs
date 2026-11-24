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
async fn ratio_and_caida_only_record_and_never_run_a_risk_check() {
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

    let BotAction::Reply(none) = handle_incoming(&h.engine, &command_with("caida", "")).await
    else {
        panic!("expected a reply");
    };
    assert!(none.contains("No se pudo anotar"), "{none}");
    let BotAction::Reply(wrong) = handle_incoming(&h.engine, &command_with("ratio", "ZZZ")).await
    else {
        panic!("expected a reply");
    };
    assert!(wrong.contains("No hay un posible cambio"), "{wrong}");
    let BotAction::Reply(ok) = handle_incoming(&h.engine, &command_with("caida", "aaa")).await
    else {
        panic!("expected a reply");
    };
    assert!(ok.contains("Anotado") && ok.contains("AAA"), "{ok}");
    assert!(ok.contains("No se corrió ningún control"), "{ok}");
    let BotAction::Reply(ok) = handle_incoming(&h.engine, &command_with("ratio", "aaa")).await
    else {
        panic!("expected a reply");
    };
    assert!(ok.contains("cambio de ratio"), "{ok}");
    // Nothing ran: the stored index is untouched and no order went out.
    let after: RiskIndex = h
        .store
        .lock()
        .expect("lock")
        .get_state(&key)
        .expect("get")
        .expect("index");
    assert_eq!(after, index);
    assert!(h.fake().places().is_empty());
    // The old command is gone and says what to use now.
    let BotAction::Reply(old) = handle_incoming(&h.engine, &command_with("aceptar", "AAA")).await
    else {
        panic!("expected a reply");
    };
    assert!(old.contains("/ratio") && old.contains("/caida"), "{old}");
    let help = handle_incoming(&h.engine, &command("ayuda")).await;
    assert!(
        matches!(help, BotAction::Reply(t) if t.contains("/ratio") && t.contains("/caida") && !t.contains("/aceptar"))
    );
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
