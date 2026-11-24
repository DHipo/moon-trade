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
- **El bot trata como propia toda tenencia de un activo del universo**, aunque la hayas comprado vos a mano: esas posiciones se rebalancean (se pueden vender). Usá una cuenta o subcuenta dedicada al bot, o evitá operar a mano en la misma cuenta.
- **Los depósitos se invierten solos** (ver "Depósitos"): solo compras, hacia los pesos de la última decisión mensual.
- Las comisiones, ratios de CEDEAR, tick de precios y feriados del ejemplo son placeholders: verificalos (ver "Pendientes de verificar"). Si `schedule.holidays` está vacío, el servicio te avisa al arrancar.

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

**La configuración es estricta:** una clave desconocida o mal escrita en `moon.toml` es un error (no se ignora). Claves a tener presentes:

- `execution.emergency_limit_discount` (por defecto 0.03, entre 0 y 0.2): las ventas de emergencia van a límite este porcentaje por debajo del último precio (a mercado si no hay cotización).
- `backtest.cash_usd_return` (anual, por defecto 0.0, puede ser negativo): rendimiento en dólares del efectivo en el backtest. En vivo el efectivo son pesos y pierde valor frente al dólar: si el refugio es `CASH`, configurá un valor negativo realista.
- `schedule.holidays`: feriados de BYMA; cargalos.

## Primeros pasos (modo paper)

```bash
moon init               # crea la base de datos (moon.db) y los directorios
moon data sync          # descarga el historial de Yahoo
moon backtest           # corre y registra el backtest (excluye el período virgen)
moon backtest-runs      # lista los backtests registrados (cuántas configuraciones probaste)
moon plan               # muestra la cartera objetivo y las órdenes que haría
moon serve              # arranca el servicio en modo paper
```

En paper podés simular depósitos con `moon paper deposit <monto_ars>` y empezar de cero con `moon paper reset`. **Corré `moon paper deposit` con el servicio detenido** (o dejalo a cargo del servicio): el comando abre la base de datos por su cuenta y no hay bloqueo entre procesos. Si ya hubo una decisión mensual, el depósito simulado se invierte enseguida (ver "Depósitos").

## Telegram

1. Hablá con [@BotFather](https://t.me/BotFather), creá un bot con `/newbot` y copiá el token en `MOON_TELEGRAM_TOKEN`.
2. Abrí un chat **privado** con tu bot y mandale cualquier mensaje.
3. Obtené tu chat id: abrí `https://api.telegram.org/bot<TOKEN>/getUpdates` en el navegador y buscá `"chat":{"id":...}`. Ponelo en `MOON_TELEGRAM_CHAT_ID`.
4. Poné `enabled = true` en la sección `[telegram]` de `moon.toml`.
5. Probalo: `moon check telegram` envía "✅ moon-trade: prueba de Telegram".

Solo se aceptan mensajes y botones del `chat_id` configurado (y del mismo usuario). Comandos: `/estado`, `/pausa`, `/reanudar`, `/ayuda`. **En modo live Telegram es obligatorio** (`[telegram] enabled = true` con token y chat id): sin él no habría avisos ni aprobaciones, y el bot se niega a arrancar.

Las aprobaciones se procesan en tareas aparte, así que el bot sigue respondiendo comandos mientras una propuesta se ejecuta. Si la tarea de Telegram se cae, el servicio avisa (si puede) y termina con error para que el administrador de servicios del sistema lo reinicie.

## IOL

1. En IOL habilitá el acceso a la API (Configuración de la cuenta, API) con tu usuario y contraseña.
2. Completá `MOON_IOL_USERNAME` y `MOON_IOL_PASSWORD` en `.env`.
3. Verificá con `moon check iol`: inicia sesión y muestra los saldos por plazo (total, disponible y comprometido), las posiciones con su disponibilidad, las operaciones y los movimientos de los últimos 30 días (con los estados y tipos distintos que devuelve tu cuenta y sus montos), las cotizaciones de cada par del CCL y el CCL resultante. Es **estrictamente de solo lectura**: nunca envía, estima ni cancela órdenes.
4. **Antes de pasar a live, corré `moon check iol` y revisá toda la salida** contra lo que ves en IOL: que los saldos y plazos tengan sentido, que los estados de operaciones y tipos de movimiento (depósito, extracción) coincidan con lo que el bot espera y que el CCL sea razonable.

## Modos y cómo pasar a live

| Modo | Qué hace |
|---|---|
| `paper` | Precios reales, órdenes simuladas en SQLite. |
| `live_confirm` | Compras y rebalanceos esperan tu aprobación por Telegram (vencen el mismo día). Las salidas de emergencia se ejecutan solas. |
| `live_auto` | Opera solo dentro de los límites y avisa después. |

**Doble opt-in:** para operar en real hacen falta `mode = "live_confirm"` o `"live_auto"` **y** `[live] enabled = true` **y** credenciales de IOL. Además, live exige `execution.quote_source = "iol"` (las cotizaciones de Yahoo no sirven para operar) y Telegram habilitado y configurado.

**Paper y live no comparten estado:** el NAV, el índice de riesgo, el freno, el planificador, los depósitos pendientes y los intentos del rebalanceo mensual se guardan por modo. Podés pasar de paper a live (o volver) sin reiniciar nada; live arranca con su propio estado limpio y nunca hereda el máximo ni el freno de paper.

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
| `moon orders resolve <id_orden> <filled\|cancelled> [--qty N] [--price P]` | Registra a mano el resultado de una orden con resultado desconocido, **después de verificarlo en IOL**. Solo modifica la base local: no llama a IOL. |
| `moon check iol` | Prueba la conexión con IOL y muestra saldos, posiciones, operaciones, movimientos y CCL (solo lectura). |
| `moon check telegram` | Envía un mensaje de prueba. |

En Telegram: `/estado` (o `/status`), `/pausa` (`/pause`), `/reanudar` (`/resume`), `/ayuda`. La pausa frena los rebalanceos; **las salidas de emergencia siguen activas**.

## Cómo funciona

- **Señales y ejecución separadas:** las señales se calculan en USD con el subyacente de EE.UU. (series largas, con dividendos); se ejecuta con CEDEARs en pesos en BCBA. La conversión usa un CCL implícito (mediana de varios pares acción local / ADR).
- **Calendario (hora Argentina, con feriados):** decisión mensual con las señales al último día calendario del mes anterior, ejecutada el primer día hábil (~11:30; si se perdió, se recupera en un día hábil posterior). Antes de decidir el bot actualiza los datos y exige que cada símbolo de señal (riesgo, refugio y canarios) tenga una vela de hasta 4 días antes de esa fecha: si no, avisa y reintenta más tarde. El mes cuenta como hecho solo si el plan quedó vacío, se mandó la propuesta o el broker aceptó al menos una orden; si no, se reintenta cada 30 minutos. Control de riesgo cada 15 min en horario de mercado, foto y conciliación al cierre, resumen semanal.
- **Rebalanceo mensual idempotente:** el lote de órdenes de cada mes se identifica por modo, mes e intento. Antes de planear se actualizan las órdenes del mes; los activos con una orden en curso o con resultado desconocido se excluyen (y se avisa), así un reinicio a mitad de camino nunca duplica órdenes. Una orden más grande que `max_order_usd` se parte en tramos (cada uno dentro del límite) que cuentan para `max_orders_per_day`.
- **Datos de señales:** la sincronización vuelve a bajar una ventana de 90 días; si algún cierre ajustado cambió (dividendo o split) descarga y reemplaza el historial completo de ese símbolo. Nunca guarda la vela de la rueda de EE.UU. que todavía no cerró.
- **Régimen y estrategias:** Faber SMA (media de 10 meses), dual momentum (Antonacci), combinación por promedio, canarios opcionales (Keller/Keuning, DAA) y tamaño por volatilidad inversa opcional. Todas devuelven pesos que suman 1, incluyendo el refugio.
- **Riesgo (código determinístico):** lista blanca de activos, tope por activo (`max_weight`), monto mínimo y máximo por orden, máximo de órdenes por día, colchón de efectivo y tope de patrimonio (`max_equity_usd`).
- **Freno por caída:** mide la caída **de lo invertido, en dólares** (un índice de riesgo con el retorno de mercado de las posiciones que el bot tiene), nunca del efectivo: depósitos, extracciones, órdenes pendientes, liquidaciones y la devaluación del peso no lo activan. Si lo invertido cae `max_drawdown` desde su máximo, se vende todo lo riesgoso y se pasa al refugio automáticamente en cualquier modo (también en pausa), y queda bloqueado hasta la siguiente decisión mensual, que reinicia el máximo. La venta de emergencia cancela antes las órdenes abiertas, usa una orden límite agresiva (`emergency_limit_discount`) y no depende de que haya cotizaciones disponibles; si no se ejecuta, se reintenta en cada control.
- **Depósitos:** cuando se detecta un depósito (en un control de mercado o al cierre; en paper, con `moon paper deposit`), y ya existe una decisión mensual, el freno no está activo y el bot no está en pausa, se compran (solo compras, nunca ventas) los activos hacia los pesos de esa última decisión. En paper y `live_auto` se ejecuta solo; en `live_confirm` llega como propuesta aparte. Los retiros no generan operaciones.
- **Banda de rebalanceo:** solo se opera un activo si su desvío supera la banda (o entra o sale por completo).
- **Costos:** comisiones, derechos de mercado, IVA, mínimo y slippage configurables, aplicados en backtest, paper y estimaciones live.
- **Anti-overfitting:** cada backtest se registra (hash de configuración, parámetros, métricas); el período virgen (`holdout_start`) se excluye salvo `--include-holdout`; `moon backtest-runs` muestra cuántas configuraciones probaste.
- **Órdenes:** primero ventas, luego compras con el efectivo real (con comisiones estimadas); órdenes límite; cada orden se guarda antes de enviarse con clave de idempotencia derivada. Una orden que el broker rechaza de forma definitiva queda como rechazada y puede reintentarse; una con resultado desconocido nunca se reenvía.
- **Aprobaciones (`live_confirm`):** la propuesta aclara que el plan se recalcula con los precios y el efectivo del momento en que aprobás. Si al aprobar las compras recalculadas superan en más de 10 % a las mostradas, no se opera y se pide esperar una propuesta nueva.
- **Conciliación diaria:** al cierre se compara lo esperado con lo que informa IOL (posiciones y efectivo, contando todo lo ocurrido desde el cierre anterior) y se avisa de cualquier diferencia ("diferencia de caja: esperado X, IOL Y").

Diseño completo: [docs/superpowers/specs/2026-11-23-moon-trade-design.md](docs/superpowers/specs/2026-11-23-moon-trade-design.md). API de IOL: [docs/iol-api.md](docs/iol-api.md).

Referencias: Faber (SSRN 962461), Antonacci (dual momentum), Moskowitz-Ooi-Pedersen (JFE 2012), Keller-Keuning (DAA 2018), Carver (pysystemtrade), Vanguard (rebalanceo 2010/2022), Bailey-Borwein-López de Prado-Zhu (overfitting), Harvey-Liu (Backtesting, JPM 2015), FINRA/SEC.

## Límites del backtest

El backtest es una aproximación optimista; leelo con estas salvedades:

- **El refugio `CASH` rinde 0 % en dólares por defecto**, pero en vivo son pesos. Configurá `backtest.cash_usd_return` (negativo) para reflejar la pérdida; si el refugio es `CASH` y está en 0, el backtest imprime una advertencia.
- **Los dividendos entran brutos**: no se descuenta la retención del 30 % de EE.UU. que sufren los CEDEARs.
- **No incluye costos de custodia** ni impuestos.
- La tabla muestra aportes netos y rotación anual para que el resultado se interprete junto con el capital aportado y el costo de operar.

## Limitaciones conocidas

Decisiones de diseño que quedan fuera de esta versión (sin código):

- El CCL implícito mezcla mercados que no cotizan sincronizados (BCBA y EE.UU.); puede tener ruido intradiario.
- No hay bloqueo entre procesos: no corras `moon orders resolve` ni `moon paper deposit` mientras el servicio opera, o hacelo con cuidado.
- No se contempla la retención de dividendos de los CEDEARs en vivo.
- Los valores por defecto silenciosos de algunas señales (por ejemplo ante datos faltantes) no se informan.
- La espera de ejecuciones mantiene el bloqueo de operaciones mientras dura (hasta `fill_timeout_minutes`).
- Yahoo Finance es una API no oficial: puede cambiar o fallar sin aviso.

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
