# moon-trade: diseño del MVP

Fecha: 2026-11-23 · Estado: aprobado para implementar (las decisiones marcadas como *decisión técnica* se pueden revisar)

## 1. Intención

Un **administrador de cartera automático y personal**. El usuario deposita plata en su cuenta de IOL
(InvertirOnline) y el bot decide qué comprar, cuánto y cuándo, mantiene la cartera, y la protege en
épocas bajistas moviéndola a un refugio. Funciona "como un contador": el usuario pone la plata y el
bot se encarga, dentro de reglas y límites que el usuario aprueba.

- Uso personal (el usuario y, a lo sumo, un familiar). No es un producto para terceros.
- Todo se mide **en dólares** (USD). Perfil de riesgo **medio**.
- Sin LLM en esta etapa (queda como capa futura: el LLM propone, el código decide).

## 2. Objetivos, no objetivos y anti-objetivos

**Objetivos**
1. Invertir automáticamente el efectivo que entra a la cuenta.
2. Mantener una cartera objetivo calculada por reglas publicadas y respaldadas (ver §5).
3. Detectar régimen bajista y pasar al refugio; salida de emergencia automática por caída máxima.
4. Backtest honesto (costos reales, sin mirar el futuro, registro anti-overfitting).
5. Modos `paper` → `live_confirm` → `live_auto`, cambiando solo configuración.
6. Avisos y aprobaciones por Telegram; resumen semanal.

**No objetivos (por ahora)**: LLM, cripto, multiusuario, interfaz gráfica, intradía, operar la
cuenta de EE.UU. de IOL, optimización automática de parámetros.

**Anti-objetivos (sería un fracaso aunque "funcione")**
- Que el bot pueda mandar una orden real sin doble opt-in explícito del usuario.
- Que un error de cálculo de plata (redondeo, moneda, comisión) pase en silencio.
- Que el backtest dé resultados inflados por mirar el futuro, ignorar costos o probar mil variantes.
- Que una credencial termine en el repositorio (el repo es **público**).

## 3. Enfoques considerados

| Enfoque | Pros | Contras | Decisión |
|---|---|---|---|
| **A. Servicio permanente** (`moon serve`) con planificador interno + Telegram escuchando; toda la lógica en librerías compartidas con la CLI | Responde al instante a aprobaciones y emergencias; un solo lugar de estado y logs | Necesita una máquina prendida en horario de mercado | **Elegido** |
| B. Comandos sueltos programados (Programador de tareas / cron) | Más simple, sin proceso que se cuelgue | Aprobaciones y emergencias con hasta 15 min de atraso | Descartado |
| C. Montarlo sobre barter-rs / nautilus_trader | Motores maduros | Pensados para alta frecuencia; mucha complejidad; sin conector IOL | Descartado (solo referencia) |

*Decisión técnica:* el servicio corre por defecto en la PC Windows del usuario; también funciona en
Linux (Raspberry Pi o VPS). Reinicio automático (Programador de tareas / systemd) queda documentado
en el README, fuera del código.

## 4. Restricciones conocidas

- **IOL no tiene sandbox**: toda orden por API es real. El modo paper se simula localmente.
- API de IOL (`https://api.iolinversiones.com`, referencia en `docs/iol-api.md`):
  token de 15 min + refresh de 30 días; histórico solo diario; órdenes requieren `X-Idempotency-Key`
  (UUID) y `due_date`; no hay GET de una orden: el estado se consulta en
  `/broker/accounts/activities/operations`; no hay rate limits documentados.
- Mercado BYMA ≈ 11:00–17:00 hora Argentina (America/Argentina/Buenos_Aires).
- Datos históricos largos: Yahoo Finance (API no oficial; puede cambiar).
- Comisiones de IOL: **a verificar** por el usuario; el modelo es configurable.

## 5. Modelo de inversión

### 5.1 Separación señal / ejecución (*decisión técnica*)
- **Señales y backtest en USD con el subyacente de EE.UU.** (ej. `SPY` en Yahoo): series largas,
  limpias y con dividendos (precio ajustado).
- **Ejecución con CEDEARs en pesos en BCBA** vía IOL (el segmento más líquido; los depósitos en
  pesos se invierten directo).
- Conversión ARS↔USD en vivo con un **CCL implícito**: mediana de varios pares acción local / ADR
  (`precio_local_ARS / (precio_ADR_USD / ratio)`), ej. GGAL/GGAL (10), YPFD/YPF (1), PAMP/PAM (25),
  BMA/BMA (10). Pares y ratios configurables (verificar ratios).
- El backtest simula unidades de CEDEAR con precio `subyacente / ratio_cedear` para que el
  redondeo de lotes sea realista.

### 5.2 Calendario
- **Decisión mensual**: señales con el cierre del último día hábil del mes; ejecución el primer
  día hábil del mes siguiente (en vivo ~11:30; en backtest al precio de apertura + slippage).
  Nunca se usa información posterior a la fecha de la señal.
- **Control diario**: cada 15 min en horario de mercado se valúa la cartera y se chequea el freno
  por caída. Al cierre (~17:30) se guarda la foto del día y se concilia con IOL.

### 5.3 Estrategias (todas devuelven pesos objetivo que suman 1, incluyendo el refugio)
1. **Faber SMA**: cada activo de riesgo recibe 1/N; si su cierre mensual está por debajo de su
   media de 10 meses, su parte va al refugio.
2. **Dual momentum (Antonacci)**: rankear activos de riesgo por retorno de 12 meses; tomar los
   `top_n`; si el retorno de uno no supera al del refugio (o a 0 si el refugio es efectivo), su
   parte va al refugio.
3. **Combinación**: promedio simple de las estrategias habilitadas.
4. **Canario (opcional, Keller/Keuning DAA)**: activos que no se operan (ej. emergentes y bonos);
   momentum 13612W (`12·r1 + 4·r3 + 2·r6 + r12`, sobre 4); con `b` canarios negativos, fracción
   defensiva = `min(b, umbral) / umbral`, que se pasa del riesgo al refugio.
5. **Tamaño por volatilidad (opcional)**: dentro de lo elegido por cada estrategia, pesos ∝ 1/vol
   (vol diaria anualizada de 63 ruedas) en lugar de partes iguales.

Refugio: un activo configurable o `CASH`. *Elegirlo es decisión del usuario*; el ejemplo de
configuración lo marca como placeholder. Ojo: efectivo en pesos pierde valor en dólares.

### 5.4 Capa de riesgo (código determinístico, ninguna estrategia la puede saltear)
- Lista blanca: solo se operan activos del universo configurado; lo demás de la cuenta se ignora.
- Tope por activo (`max_weight`, ej. 0,20): el exceso va al refugio.
- **Freno por caída**: si el patrimonio (USD) cae `max_drawdown` (ej. 15 %) desde su máximo, se
  pasa todo al refugio **automáticamente en cualquier modo**. Queda bloqueado hasta la siguiente
  decisión mensual; si esa decisión vuelve a riesgo, el máximo se reinicia al patrimonio actual.
- Banda de rebalanceo (ej. 5 %): solo se opera un activo si su desvío supera la banda, o si entra o
  sale por completo. Si el efectivo supera la banda (depósito), se compran los subponderados.
- Monto mínimo por operación, monto máximo por orden, máximo de órdenes por día, colchón de
  efectivo para comisiones, tope de patrimonio que maneja el bot.
- Pausa (`moon pause` o `/pausa` en Telegram) frena rebalanceos; las salidas de emergencia siguen
  activas.

### 5.5 Costos
Modelo de comisiones configurable: `commission_pct`, `market_fee_pct`, `vat_pct` (sobre comisión),
`min_commission`, más `slippage_pct`. Se aplica en backtest, en paper y en las estimaciones live.
Valores por defecto = placeholders conservadores **a verificar** con IOL
(`POST /broker/orders/estimate` devuelve el desglose real).

## 6. Ejecución y modos

- `paper`: precios reales (IOL o Yahoo `.BA` como respaldo), órdenes simuladas y registradas en
  SQLite. Depósitos simulados con `moon paper deposit`.
- `live_confirm`: compras y rebalanceos esperan aprobación por Telegram (vencen el mismo día);
  las salidas de emergencia se ejecutan solas.
- `live_auto`: opera solo dentro de los límites y avisa después.
- **Doble opt-in para live**: `mode = live_*` **y** `[live] enabled = true` **y** credenciales.
- Flujo de órdenes: primero ventas, luego compras con el efectivo real disponible. Órdenes límite
  al último precio ± tolerancia. Cada orden se guarda **antes** de enviarse, con clave de
  idempotencia derivada (UUID v5 de rebalanceo + activo + lado), así un reintento tras un corte no
  duplica. El estado se sigue por la API de operaciones hasta completarse o cancelarse al cierre.
- Conciliación diaria: posiciones y efectivo de IOL vs. lo que el bot espera; diferencia → aviso.

## 7. Datos y almacenamiento
- SQLite local (`moon.db`): velas, tipo de cambio, fotos de patrimonio, órdenes, propuestas,
  estado (pausa, freno, máximo, offset de Telegram), cuenta paper, registro de backtests y log de
  decisiones. Montos de dinero y cantidades se guardan como texto decimal exacto.
- Velas guardan su fuente (`yahoo`/`iol`).

## 8. Backtest
- Mensual con valuación diaria, mismo código de estrategia y riesgo que en vivo.
- Costos, slippage y lotes realistas; aportes mensuales opcionales.
- Métricas sobre retorno ponderado en el tiempo (TWR, neutraliza aportes): CAGR, volatilidad,
  Sharpe (rf configurable, 0 por defecto), máxima caída, Calmar, turnover, comisiones totales,
  % del tiempo en refugio, mejor/peor mes, % de meses positivos; comparación contra un benchmark
  comprado y mantenido.
- **Anti-overfitting**: cada corrida se registra (hash de configuración, parámetros, métricas);
  período "virgen" (`holdout_start`) excluido salvo `--include-holdout`; `moon backtest runs`
  muestra cuántas configuraciones se probaron.

## 9. Interfaz
- CLI `moon`: `init`, `data sync`, `backtest`, `backtest runs`, `plan`, `status`,
  `paper deposit|reset`, `serve`, `pause`, `resume`, `iol check`, `telegram test`.
- Telegram: avisos de órdenes y errores, aprobaciones con botones, `/estado`, `/pausa`,
  `/reanudar`, resumen semanal (viernes ~18:00). Solo se aceptan mensajes del `chat_id` configurado.
- Configuración: `moon.toml` (sin secretos; se versiona `moon.example.toml`) + secretos por
  variables de entorno / `.env` (en `.gitignore`).
- Textos para el usuario en español; código y comentarios en inglés.

## 10. Arquitectura (workspace de Cargo)

```
crates/
  moon-core      dominio puro y sincrónico: tipos, indicadores, estrategias, riesgo, comisiones,
                 rebalanceo, backtest, métricas (sin IO)
  moon-store     SQLite (rusqlite, migraciones con user_version)
  moon-yahoo     cliente HTTP de Yahoo (chart v8)
  moon-iol       cliente HTTP de IOL (auth, market data, cuentas, órdenes, operaciones)
  moon-telegram  cliente HTTP del Bot API (enviar, botones, long polling)
  moon-broker    trait Broker + PaperBroker + IolBroker; fuentes de cotización; tipo de cambio
  moon-app       binario `moon`: configuración, CLI, motor de ejecución, planificador, servicio
```

## 11. Calidad y tooling
- Rust estable, edición 2024; `cargo fmt`, `cargo clippy -D warnings`, `unsafe_code = forbid`,
  `unwrap_used = deny` fuera de tests.
- Tests unitarios en todos los crates; tests de propiedades (proptest) para invariantes de dinero
  del rebalanceo; clientes HTTP probados con `wiremock` y fixtures; SQLite en memoria; **ningún
  test usa la red**.
- CI en GitHub Actions (fmt, clippy, tests en Linux y Windows, auditoría de dependencias);
  Dependabot; scripts `scripts/check.ps1` / `scripts/check.sh`.

## 12. Pendiente de verificar (del usuario o con la API real)
1. Comisiones reales de IOL para CEDEARs.
2. Moneda en que la API devuelve cotizaciones de CEDEARs (se asume ARS en BCBA).
3. Ratios de CEDEARs y de ADRs usados para el CCL.
4. Instrumento de refugio.
5. Semántica de `t0..t3` en balances y de estados de órdenes parciales en operaciones.
6. Feriados de BYMA (lista configurable).

## Referencias
Faber (SSRN 962461), Antonacci (dual momentum), Moskowitz-Ooi-Pedersen (JFE 2012),
Keller-Keuning (DAA 2018), Carver (pysystemtrade), Vanguard (rebalanceo 2010/2022),
Bailey-Borwein-López de Prado-Zhu (overfitting), Harvey-Liu (Backtesting, JPM 2015), FINRA/SEC.
