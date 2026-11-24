//! Maps incoming Telegram updates (commands and button callbacks) to actions on the engine.

use moon_telegram::{Incoming, IncomingKind};
use uuid::Uuid;

use crate::engine::{Engine, RatioDecision};

/// What the service must do in response to an update.
#[derive(Debug, Clone, PartialEq)]
pub enum BotAction {
    Reply(String),
    CallbackDone {
        callback_id: String,
        message_id: Option<i64>,
        text: String,
    },
    Ignore,
}

const HELP: &str = "Comandos disponibles:\n\
/estado: patrimonio, posiciones y estado del bot\n\
/pausa: frena los rebalanceos (las salidas de emergencia siguen activas)\n\
/reanudar: reanuda los rebalanceos\n\
/ratio <símbolo>: el posible cambio de ratio pendiente fue un cambio de ratio (se rebasa sin contar caída)\n\
/caida <símbolo>: fue un movimiento real (se cuenta la caída desde la base)\n\
/ayuda: muestra esta ayuda";

pub async fn handle_incoming(engine: &Engine, incoming: &Incoming) -> BotAction {
    match &incoming.kind {
        IncomingKind::Command { name, args } => {
            BotAction::Reply(handle_command(engine, &name.to_lowercase(), args).await)
        }
        IncomingKind::Callback {
            id,
            data,
            message_id,
        } => {
            let text = handle_callback(engine, data).await;
            BotAction::CallbackDone {
                callback_id: id.clone(),
                message_id: *message_id,
                text,
            }
        }
        IncomingKind::Other => BotAction::Ignore,
    }
}

async fn handle_command(engine: &Engine, name: &str, args: &str) -> String {
    match name {
        "estado" | "status" => engine
            .status_text()
            .await
            .unwrap_or_else(|e| format!("No se pudo obtener el estado: {e}")),
        "pausa" | "pause" => match engine.set_paused(true) {
            Ok(()) => "⏸ Bot en pausa: no se harán rebalanceos. Las salidas de emergencia por \
                       caída siguen activas."
                .to_string(),
            Err(e) => format!("No se pudo pausar: {e}"),
        },
        "reanudar" | "resume" => match engine.set_paused(false) {
            Ok(()) => "▶️ Bot reanudado.".to_string(),
            Err(e) => format!("No se pudo reanudar: {e}"),
        },
        "ratio" => decide_ratio(engine, args, RatioDecision::Ratio),
        "caida" | "caída" | "drop" => decide_ratio(engine, args, RatioDecision::Drop),
        "aceptar" | "accept" => {
            "El comando /aceptar ya no existe. Primero verificá la relación del CEDEAR en BYMA/IOL y después usá /ratio <símbolo> (fue un cambio de ratio) o /caida <símbolo> (fue un movimiento real)."
                .to_string()
        }
        _ => HELP.to_string(),
    }
}

/// `/ratio` and `/caida`: only RECORD the owner's decision about a pending suspected ratio
/// change; the scheduler applies it at its next risk check, so no check runs from here.
fn decide_ratio(engine: &Engine, args: &str, decision: RatioDecision) -> String {
    match engine.decide_ratio_change(args, decision) {
        Ok(text) => text,
        Err(e) => format!("No se pudo anotar: {e}"),
    }
}

async fn handle_callback(engine: &Engine, data: &str) -> String {
    const INVALID: &str = "Acción inválida.";
    let Some((action, id)) = data.split_once(':') else {
        return INVALID.to_string();
    };
    let approve = match action {
        "approve" => true,
        "reject" => false,
        _ => return INVALID.to_string(),
    };
    let Ok(id) = Uuid::parse_str(id) else {
        return INVALID.to_string();
    };
    engine
        .handle_approval(id, approve)
        .await
        .unwrap_or_else(|e| format!("Error al procesar la propuesta: {e}"))
}
