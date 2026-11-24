//! Read-only connectivity checks for IOL and Telegram.

use std::sync::Arc;

use std::collections::BTreeMap;

use anyhow::{Context, anyhow};
use chrono::{Duration, NaiveDate, Utc};
use moon_broker::{IolQuotes, QuoteSource, implied_ccl};
use moon_core::asset::Market;
use moon_core::time::{SystemClock, art_date};
use moon_iol::{Country, IolClient, IolCredentials, OperationsQuery, TermAmounts};
use moon_telegram::TelegramClient;

use crate::config::{AppConfig, Secrets};

/// Logs in and reads everything the bot depends on: balances per term, positions with their
/// availability, the last 30 days of operations and activities (distinct states and types, to
/// verify the real values against what the code expects), the quotes of every FX pair and the
/// resulting CCL. It is strictly read-only: it never places, estimates or cancels orders (IOL
/// has no sandbox).
pub async fn check_iol(cfg: &AppConfig, secrets: &Secrets) -> anyhow::Result<String> {
    let (Some(username), Some(password)) = (&secrets.iol_username, &secrets.iol_password) else {
        return Err(anyhow!(
            "Faltan MOON_IOL_USERNAME y MOON_IOL_PASSWORD en el entorno o en .env."
        ));
    };
    let client = Arc::new(IolClient::new(
        cfg.iol.base_url.clone(),
        IolCredentials {
            username: username.clone(),
            password: password.clone(),
        },
        Arc::new(SystemClock),
    ));
    iol_report(&client, cfg, art_date(Utc::now())).await
}

fn amount(v: Option<f64>) -> String {
    v.map_or_else(|| "—".to_string(), |x| format!("{x:.2}"))
}

fn terms(a: &TermAmounts) -> String {
    format!(
        "T0 {} · T1 {} · T2 {}",
        amount(a.t0),
        amount(a.t1),
        amount(a.t2)
    )
}

/// The report of [`check_iol`] over an already built client (`today` is the ART date).
pub async fn iol_report(
    client: &Arc<IolClient>,
    cfg: &AppConfig,
    today: NaiveDate,
) -> anyhow::Result<String> {
    let mut lines = vec!["Conexión con IOL (solo lectura)".to_string()];

    // --- balances per term ---
    let balances = client
        .balances(Country::Ar)
        .await
        .context("no se pudo iniciar sesión o leer los saldos de IOL")?;
    for (label, block) in [("ARS", &balances.ars), ("USD", &balances.usd)] {
        match block {
            Some(b) => {
                lines.push(format!("✅ Saldo {label}:"));
                lines.push(format!("   total: {}", terms(&b.totals)));
                lines.push(format!("   disponible: {}", terms(&b.available)));
                lines.push(format!("   comprometido: {}", terms(&b.committed)));
            }
            None if label == "ARS" => {
                lines.push("⚠️ IOL no devolvió saldo en pesos.".to_string());
            }
            None => lines.push("ℹ️ IOL no devolvió saldo en dólares.".to_string()),
        }
    }
    lines.push(format!(
        "   El bot usa el plazo {}: revisá que total y disponible de ese plazo tengan sentido.",
        cfg.execution.settlement_term.as_str()
    ));

    // --- positions with availability ---
    let positions = client
        .positions(Country::Ar)
        .await
        .context("no se pudieron leer las posiciones de IOL")?;
    lines.push(format!(
        "✅ Posiciones: {} instrumentos",
        positions.positions.len()
    ));
    for p in &positions.positions {
        let a = p.availability.as_ref();
        lines.push(format!(
            "   • {}: cantidad {} · disponible {} · retenida {} · pendiente {}",
            p.instrument.symbol,
            p.quantity,
            amount(a.and_then(|a| a.quantity_available)),
            amount(a.and_then(|a| a.quantity_held)),
            amount(a.and_then(|a| a.quantity_pending)),
        ));
    }

    // --- operations and activities of the last 30 days ---
    let from = today - Duration::days(30);
    match client
        .operations(&OperationsQuery {
            start_date: from,
            end_date: today,
            country: Some("AR".to_string()),
            symbol: None,
            state: None,
        })
        .await
    {
        Ok(ops) => {
            let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
            for o in &ops {
                *by_status.entry(o.status.clone()).or_default() += 1;
            }
            lines.push(format!(
                "✅ Operaciones de los últimos 30 días: {}",
                ops.len()
            ));
            for (status, n) in by_status {
                lines.push(format!("   • estado «{status}»: {n}"));
            }
        }
        Err(e) => lines.push(format!("⚠️ No se pudieron leer las operaciones: {e}")),
    }
    match client.activities(from, today, None).await {
        Ok(acts) => {
            let mut groups: BTreeMap<(String, String, String), (usize, f64)> = BTreeMap::new();
            for a in &acts {
                let key = (
                    a.activity_type
                        .clone()
                        .unwrap_or_else(|| "(sin tipo)".into()),
                    a.status.clone().unwrap_or_else(|| "(sin estado)".into()),
                    a.currency.clone().unwrap_or_else(|| "(sin moneda)".into()),
                );
                let entry = groups.entry(key).or_default();
                entry.0 += 1;
                entry.1 += a.net_amount.unwrap_or(0.0);
            }
            lines.push(format!(
                "✅ Movimientos de los últimos 30 días: {}",
                acts.len()
            ));
            for ((kind, status, currency), (n, total)) in groups {
                lines.push(format!(
                    "   • tipo «{kind}», estado «{status}», {currency}: {n} movimiento(s), suma {total:.2}"
                ));
            }
        }
        Err(e) => lines.push(format!("⚠️ No se pudieron leer los movimientos: {e}")),
    }

    // --- one quote of a tradable asset ---
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

    // --- FX pairs and the resulting CCL ---
    let quotes = IolQuotes::new(client.clone(), cfg.execution.settlement_term);
    lines.push("Pares del CCL implícito:".to_string());
    for p in &cfg.fx.pairs {
        let local = quotes.quote(&p.local_symbol, Market::Bcba).await;
        let adr = quotes.quote(&p.adr_symbol, p.adr_market).await;
        let show = |q: &Result<moon_broker::Quote, moon_broker::BrokerError>| match q {
            Ok(q) => format!("{} {}", q.price, q.currency.as_str()),
            Err(e) => format!("error ({e})"),
        };
        lines.push(format!(
            "   • {} {} / {} {} (ratio {})",
            p.local_symbol,
            show(&local),
            p.adr_symbol,
            show(&adr),
            p.ratio
        ));
    }
    match implied_ccl(&quotes, &cfg.fx.pairs, cfg.fx.min_pairs, Utc::now()).await {
        Ok(fx) => {
            lines.push(format!("✅ CCL implícito: {}", fx.rate.round_dp(2)));
            for c in &fx.components {
                lines.push(format!("   • {}: {}", c.pair, c.rate.round_dp(2)));
            }
        }
        Err(e) => lines.push(format!("⚠️ No se pudo calcular el CCL: {e}")),
    }
    lines.push(
        "Revisá estos valores contra tu cuenta antes de operar en vivo (estados, tipos de movimiento, plazos y disponibilidad)."
            .to_string(),
    );
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
