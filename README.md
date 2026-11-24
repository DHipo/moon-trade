# moon-trade

Administrador de cartera personal en Rust (CLI y servicio) para una cuenta de IOL (InvertirOnline, Argentina). Vos depositás plata en IOL; el bot calcula una cartera objetivo con reglas publicadas, la mantiene, y la protege en épocas bajistas moviéndola a un refugio. Todo se mide en dólares.

## Qué es y qué no es

- **Es** una herramienta de uso personal: backtest honesto, modo simulado (paper), avisos y aprobaciones por Telegram, y un servicio que corre en tu PC.
- **No es** asesoramiento financiero ni una recomendación de inversión. Los instrumentos y números de `moon.example.toml` son ejemplos para probar. No hay garantía de ganancias: podés perder plata.
- **No es** un producto para terceros, ni trading de alta frecuencia, ni opera cripto ni la cuenta de EE.UU. de IOL.

## Advertencias

- **IOL no tiene sandbox: toda orden enviada en modo live es real.** El modo paper se simula localmente.
- **Este repositorio es público.** Nunca subas `.env` ni `moon.toml` (ya están en `.gitignore`): contienen tus credenciales y tu configuración personal.
- **Usá un chat PRIVADO con el bot de Telegram.** En un grupo cualquier miembro podría apretar "Aprobar" en una propuesta.
- **Las salidas de emergencia primero venden a efectivo y recién después compran el refugio** (si el refugio no es `CASH`). Entre una cosa y la otra podés quedar un rato en pesos.
- **Una orden con resultado desconocido ("resultado desconocido") hay que verificarla a mano en IOL.** El bot nunca la reintenta, para no duplicarla.
- Las comisiones, ratios de CEDEAR, tick de precios y feriados del ejemplo son placeholders: verificalos (ver "Pendientes de verificar").

## Requisitos

- Rust estable (el repositorio fija la versión en `rust-toolchain.toml`).
- En Windows: Build Tools de Visual Studio (carga de trabajo "Desarrollo para el escritorio con C++").
- Una cuenta de IOL con la API habilitada (solo para live o para cotizaciones de IOL) y, opcionalmente, un bot de Telegram.

## Instalación

```bash
git clone <este repositorio>
cd moon-trade
cargo build --release
```

El binario queda en `target/release/moon` (`moon.exe` en Windows).

## Configuración

1. Copiá `moon.example.toml` a `moon.toml` y revisá cada valor (universo, refugio, límites de riesgo, comisiones, feriados).
2. Copiá `.env.example` a `.env` y completá los secretos:

```
MOON_IOL_USERNAME=
MOON_IOL_PASSWORD=
MOON_TELEGRAM_TOKEN=
MOON_TELEGRAM_CHAT_ID=
```

Los secretos nunca van en `moon.toml`. Todos los comandos aceptan `--config <ruta>` (por defecto `moon.toml`).

## Primeros pasos (modo paper)

```bash
moon init               # crea la base de datos (moon.db) y los directorios
moon data sync          # descarga el historial de Yahoo
moon backtest           # corre y registra el backtest (excluye el período virgen)
moon backtest-runs      # lista los backtests registrados (cuántas configuraciones probaste)
moon plan               # muestra la cartera objetivo y las órdenes que haría
moon serve              # arranca el servicio en modo paper
```

En paper podés simular depósitos con `moon paper deposit <monto_ars>` y empezar de cero con `moon paper reset`.

## Telegram

1. Hablá con [@BotFather](https://t.me/BotFather), creá un bot con `/newbot` y copiá el token en `MOON_TELEGRAM_TOKEN`.
2. Abrí un chat **privado** con tu bot y mandale cualquier mensaje.
3. Obtené tu chat id: abrí `https://api.telegram.org/bot<TOKEN>/getUpdates` en el navegador y buscá `"chat":{"id":...}`. Ponelo en `MOON_TELEGRAM_CHAT_ID`.
4. Poné `enabled = true` en la sección `[telegram]` de `moon.toml`.
5. Probalo: `moon check telegram` envía "✅ moon-trade: prueba de Telegram".

Solo se aceptan mensajes y botones del `chat_id` configurado. Comandos: `/estado`, `/pausa`, `/reanudar`, `/ayuda`.

## IOL

1. En IOL habilitá el acceso a la API (Configuración de la cuenta, API) con tu usuario y contraseña.
2. Completá `MOON_IOL_USERNAME` y `MOON_IOL_PASSWORD` en `.env`.
3. Verificá con `moon check iol`: inicia sesión y lee saldos, posiciones y una cotización. Es **estrictamente de solo lectura**: nunca envía, estima ni cancela órdenes.

## Modos y cómo pasar a live

| Modo | Qué hace |
|---|---|
| `paper` | Precios reales, órdenes simuladas en SQLite. |
| `live_confirm` | Compras y rebalanceos esperan tu aprobación por Telegram (vencen el mismo día). Las salidas de emergencia se ejecutan solas. |
| `live_auto` | Opera solo dentro de los límites y avisa después. |

**Doble opt-in:** para operar en real hacen falta `mode = "live_confirm"` o `"live_auto"` **y** `[live] enabled = true` **y** credenciales de IOL.

Pasá a live de forma escalonada:

1. Varias semanas en `paper`, mirando avisos, estado y el resumen semanal.
2. `live_confirm` con un monto chico: limitá lo que maneja el bot con `max_equity_usd` en `[risk]`. Aprobá cada propuesta a mano y comprobá en IOL que coincide.
3. Recién después, `live_auto` (y subir `max_equity_usd` de a poco).

## Correr como servicio

`moon serve` es un proceso permanente: planifica los controles de riesgo, el rebalanceo mensual, el cierre diario y el resumen semanal, y escucha Telegram. Necesita una máquina prendida en horario de mercado.

**Windows (Programador de tareas):** creá una tarea que ejecute `moon.exe --config C:\ruta\moon.toml serve` con "Iniciar en la carpeta" = la carpeta del proyecto (donde están `moon.toml` y `.env`), disparador "Al iniciar sesión", y en Configuración marcá "Si la tarea falla, reiniciar cada 1 minuto" (con varios reintentos).

**Linux (systemd), ejemplo `/etc/systemd/system/moon-trade.service`:**

```ini
[Unit]
Description=moon-trade
After=network-online.target

[Service]
WorkingDirectory=/opt/moon-trade
ExecStart=/opt/moon-trade/moon --config /opt/moon-trade/moon.toml serve
Restart=on-failure
RestartSec=30
User=moon

[Install]
WantedBy=multi-user.target
```

El estado del planificador se guarda en la base de datos: si el servicio se reinicia, no repite tareas ya hechas y recupera un rebalanceo mensual perdido en cualquier día hábil posterior del mes. Si el equipo estuvo apagado, el bot reanuda en el siguiente control.

## Comandos

| Comando | Descripción |
|---|---|
| `moon init` | Crea la base de datos y los directorios. |
| `moon data sync [--from FECHA]` | Descarga historial de Yahoo. |
| `moon backtest [--include-holdout] [--json]` | Corre y registra un backtest. `--include-holdout` invalida la validación final. |
| `moon backtest-runs` | Lista los backtests registrados. |
| `moon plan` | Muestra la cartera objetivo y las órdenes previstas. |
| `moon status` | Patrimonio, posiciones y estado. |
| `moon paper deposit <monto_ars>` / `moon paper reset` | Simula un depósito / reinicia la cuenta paper. |
| `moon pause` / `moon resume` | Frena / reanuda los rebalanceos. |
| `moon serve` | Servicio permanente. |
| `moon check iol` | Prueba la conexión con IOL (solo lectura). |
| `moon check telegram` | Envía un mensaje de prueba. |

En Telegram: `/estado` (o `/status`), `/pausa` (`/pause`), `/reanudar` (`/resume`), `/ayuda`. La pausa frena los rebalanceos; **las salidas de emergencia siguen activas**.

## Cómo funciona

- **Señales y ejecución separadas:** las señales se calculan en USD con el subyacente de EE.UU. (series largas, con dividendos); se ejecuta con CEDEARs en pesos en BCBA. La conversión usa un CCL implícito (mediana de varios pares acción local / ADR).
- **Calendario (hora Argentina, con feriados):** decisión mensual con el cierre del último día hábil del mes, ejecutada el primer día hábil (~11:30; si se perdió, se recupera en un día hábil posterior). Control de riesgo cada 15 min en horario de mercado, foto y conciliación al cierre, resumen semanal.
- **Régimen y estrategias:** Faber SMA (media de 10 meses), dual momentum (Antonacci), combinación por promedio, canarios opcionales (Keller/Keuning, DAA) y tamaño por volatilidad inversa opcional. Todas devuelven pesos que suman 1, incluyendo el refugio.
- **Riesgo (código determinístico):** lista blanca de activos, tope por activo (`max_weight`), monto mínimo y máximo por orden, máximo de órdenes por día, colchón de efectivo y tope de patrimonio (`max_equity_usd`).
- **Freno por caída:** si el patrimonio en USD cae `max_drawdown` desde su máximo, se pasa todo al refugio automáticamente en cualquier modo, y queda bloqueado hasta la siguiente decisión mensual.
- **Banda de rebalanceo:** solo se opera un activo si su desvío supera la banda (o entra o sale por completo).
- **Costos:** comisiones, derechos de mercado, IVA, mínimo y slippage configurables, aplicados en backtest, paper y estimaciones live.
- **Anti-overfitting:** cada backtest se registra (hash de configuración, parámetros, métricas); el período virgen (`holdout_start`) se excluye salvo `--include-holdout`; `moon backtest-runs` muestra cuántas configuraciones probaste.
- **Órdenes:** primero ventas, luego compras con el efectivo real; órdenes límite; cada orden se guarda antes de enviarse con clave de idempotencia derivada.

Diseño completo: [docs/superpowers/specs/2026-11-23-moon-trade-design.md](docs/superpowers/specs/2026-11-23-moon-trade-design.md). API de IOL: [docs/iol-api.md](docs/iol-api.md).

Referencias: Faber (SSRN 962461), Antonacci (dual momentum), Moskowitz-Ooi-Pedersen (JFE 2012), Keller-Keuning (DAA 2018), Carver (pysystemtrade), Vanguard (rebalanceo 2010/2022), Bailey-Borwein-López de Prado-Zhu (overfitting), Harvey-Liu (Backtesting, JPM 2015), FINRA/SEC.

## Pendientes de verificar

1. Comisiones reales de IOL para CEDEARs (`POST /broker/orders/estimate` devuelve el desglose real).
2. Moneda en que la API devuelve cotizaciones de CEDEARs (se asume ARS en BCBA).
3. Ratios de CEDEARs y de ADRs usados para el CCL.
4. Instrumento de refugio (elegirlo es decisión tuya; `CASH` pierde valor en dólares).
5. Semántica de `t0..t3` en balances y de estados de órdenes parciales en operaciones.
6. Feriados de BYMA (lista configurable en `[schedule] holidays`).

## Desarrollo

- `scripts/check.ps1` (Windows) o `scripts/check.sh` (Linux/macOS): `cargo fmt --check`, `cargo clippy -D warnings` y `cargo test` de todo el workspace.
- Estructura (`crates/`): `moon-core` (dominio puro: estrategias, riesgo, comisiones, backtest), `moon-store` (SQLite), `moon-yahoo`, `moon-iol`, `moon-telegram` (clientes HTTP), `moon-broker` (broker paper e IOL, cotizaciones, CCL) y `moon-app` (binario `moon`: configuración, CLI, motor, planificador, servicio).
- Ningún test usa la red (HTTP con `wiremock`, SQLite en memoria).
- CI en GitHub Actions (`.github/workflows/ci.yml`): formato, clippy y tests en Linux y Windows, auditoría de dependencias; Dependabot habilitado.

## Licencia

Ver [LICENSE](LICENSE).
