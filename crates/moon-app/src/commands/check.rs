//! Read-only connectivity checks for IOL and Telegram.

use std::sync::Arc;

use anyhow::{Context, anyhow};
use moon_core::time::SystemClock;
use moon_iol::{Country, IolClient, IolCredentials};
use moon_telegram::TelegramClient;

use crate::config::{AppConfig, Secrets};

/// Logs in and reads balances, positions and one quote. It never places, estimates or cancels
/// orders: IOL has no sandbox.
pub async fn check_iol(cfg: &AppConfig, secrets: &Secrets) -> anyhow::Result<String> {
    let (Some(username), Some(password)) = (&secrets.iol_username, &secrets.iol_password) else {
        return Err(anyhow!(
            "Faltan MOON_IOL_USERNAME y MOON_IOL_PASSWORD en el entorno o en .env."
        ));
    };
    let client = IolClient::new(
        cfg.iol.base_url.clone(),
        IolCredentials {
            username: username.clone(),
            password: password.clone(),
        },
        Arc::new(SystemClock),
    );
    let mut lines = vec!["Conexión con IOL (solo lectura)".to_string()];

    let balances = client
        .balances(Country::Ar)
        .await
        .context("no se pudo iniciar sesión o leer los saldos de IOL")?;
    match balances.ars {
        Some(b) => lines.push(format!(
            "✅ Saldo ARS: disponible T0 {:?} · T1 {:?}",
            b.available.t0, b.available.t1
        )),
        None => lines.push("⚠️ IOL no devolvió saldo en pesos.".to_string()),
    }

    let positions = client
        .positions(Country::Ar)
        .await
        .context("no se pudieron leer las posiciones de IOL")?;
    lines.push(format!(
        "✅ Posiciones: {} instrumentos",
        positions.positions.len()
    ));

    let universe = cfg.universe()?;
    let asset = universe
        .tradable()
        .into_iter()
        .find_map(|a| a.exec.as_ref().map(|e| (a, e)));
    match asset {
        Some((a, exec)) => {
            let quote = client
                .quote(&exec.symbol, exec.market, cfg.execution.settlement_term)
                .await
                .with_context(|| format!("no se pudo leer la cotización de {}", exec.symbol))?;
            lines.push(format!(
                "✅ Cotización {} ({}): último {:?}",
                exec.symbol, a.id, quote.prices.current
            ));
        }
        None => lines.push("⚠️ No hay activos operables configurados para cotizar.".to_string()),
    }
    Ok(lines.join("\n"))
}

/// Sends a test message to the configured chat.
pub async fn check_telegram(secrets: &Secrets) -> anyhow::Result<String> {
    let (Some(token), Some(chat_id)) = (&secrets.telegram_token, secrets.telegram_chat_id) else {
        return Err(anyhow!(
            "Faltan MOON_TELEGRAM_TOKEN y MOON_TELEGRAM_CHAT_ID en el entorno o en .env."
        ));
    };
    let client = TelegramClient::new(token.clone(), chat_id);
    client
        .send_text("✅ moon-trade: prueba de Telegram")
        .await
        .context("no se pudo enviar el mensaje de prueba por Telegram")?;
    Ok("Mensaje de prueba enviado: revisá tu chat de Telegram.".to_string())
}
