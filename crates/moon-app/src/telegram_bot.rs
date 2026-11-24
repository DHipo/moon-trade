//! Maps incoming Telegram updates (commands and button callbacks) to actions on the engine.

use moon_telegram::{Incoming, IncomingKind};
use uuid::Uuid;

use crate::engine::Engine;

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
/aceptar <símbolo>: cuenta como movimiento real un posible cambio de ratio pendiente\n\
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
        "aceptar" | "accept" => accept_ratio(engine, args).await,
        _ => HELP.to_string(),
    }
}

/// `/aceptar <símbolo>`: the pending suspected ratio change counts as a real move; the risk
/// check runs right away so the breaker sees it.
async fn accept_ratio(engine: &Engine, args: &str) -> String {
    let symbol = match engine.accept_ratio_change(args) {
        Ok(symbol) => symbol,
        Err(e) => return format!("No se pudo aceptar: {e}"),
    };
    match engine.run_risk_check().await {
        Ok(out) => format!(
            "✅ Aceptado: el movimiento de {symbol} se cuenta como real. Índice de riesgo {:.4}, caída {:.1}%{}.",
            out.level,
            out.drawdown * 100.0,
            if out.tripped {
                " · FRENO ACTIVADO"
            } else {
                ""
            }
        ),
        Err(e) => format!(
            "Aceptado: {symbol} se cuenta como movimiento real en el próximo control de riesgo (el control inmediato falló: {e})."
        ),
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
