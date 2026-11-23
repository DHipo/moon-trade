# IOL API: referencia para moon-trade

Resumen de la especificación pública de IOL (https://developers.iolinversiones.com/api/llms,
OpenAPI 3.1.2, versión 1.0.0), consultada el 2026-11-23. Solo cubre lo que usa moon-trade.
Los nombres de campos son literales. Lo marcado **NO ESTÁ EN LA SPEC** hay que confirmarlo con la
API real (con cuidado: no hay sandbox, todo es real).

## Base

- Base URL: `https://api.iolinversiones.com`
- Sandbox: NO ESTÁ EN LA SPEC.
- Autenticación: `Authorization: Bearer <access_token>`.
- Todos los números son `number` JSON (no strings). Precisión: NO ESTÁ EN LA SPEC.
- Rate limits y paginación: NO ESTÁN EN LA SPEC.

## Envoltorio de respuesta (todas las rutas)

```json
{ "meta": { "method": "", "path": "", "status_code": 200, "timestamp": "2026-10-21T20:59:55.000Z", "trace_id": "" },
  "data": { },
  "error": null }
```

- Error: `data` = `null`; `error` = `{ "message": string, "code": string, "details": any }`.
- `meta.total_count` (opcional) en listados.
- Header de respuesta `X-IOL-Trace-ID`.
- Errores documentados: 400 (`grant_type` inválido), 401 ("Credenciales inválidas", "Token expirado").

## 1. Token: `POST /auth/token` (JSON, sin auth)

Login: `{"grant_type":"password","username":"...","password":"..."}`
Refresh: `{"grant_type":"refresh_token","refresh_token":"..."}`

`data`: `access_token` (string, JWT), `refresh_token` (string), `expires_in` (number, segundos, ej. 900),
`expires_at` (string ISO 8601 UTC, ej. `"2026-10-21T21:14:55Z"`), `token_type` (`"bearer"`).

Duración: access 15 min, refresh 30 días. Si el refresh rota o invalida el anterior: NO ESTÁ EN LA SPEC
(guardar siempre el último recibido).

## 2. Velas diarias: `GET /broker/market-data/{symbol}/bars`

Query (todas obligatorias): `market` (`BCBA`|`NYSE`|`NASDAQ`), `from` y `to` con formato
`yyyy-MM-dd HH:mm:ss` (ej. `2023-01-01 03:00:00`).

`data`: `{ "bars": [ { "open", "high", "low", "close", "volume", "datetime" (string ISO 8601),
"timestamp" (number, segundos Unix) } ], "symbol", "market", "total_bars" }`.
Zona horaria de `datetime`: NO ESTÁ EN LA SPEC (usar `timestamp` y convertir a hora Argentina).

## 3. Cotización: `GET /broker/market-data/{symbol}/quote`

Query obligatoria: `market` (`BCBA`|`NYSE`|`NASDAQ`), `term` (`T0`|`T1`).

`data`: `symbol`, `market`, `settlement_term`, `currency` (string), `prices` {`current`, `open`,
`close`, `high`, `low`, `previous_close`}, `change` {`amount`, `percent`}, `volume` {`shares`,
`amount`}, `lot_size` (number), `last_updated` (string, formato no especificado),
`maturity_date` (string|null). Sin bid/ask (están en `/depth`: `data.bids[]`/`data.asks[]` con
`position`, `price`, `size`).

## 4. Saldos: `GET /broker/accounts/{country}/balances`

`country`: `AR`|`US`. `data` no está tipado en la spec; el ejemplo muestra:

```json
{ "ars": { "currency": "ARS",
           "totals":    { "t0": 0, "t1": 0, "t2": 0, "t3": 0 },
           "available": { "t0": 0, "t1": 0, "t2": 0, "t3": 0 },
           "committed": { "t0": 0, "t1": 0, "t2": 0, "t3": 0 },
           "available_for_withdrawal": 0 },
  "usd": { "currency": "USD", "...": "misma forma" } }
```

Significado exacto de `t0..t3`: NO ESTÁ EN LA SPEC. Parsear de forma tolerante (campos opcionales).

## 5. Posiciones: `GET /broker/accounts/{country}/positions`

Query opcional: `valuation_currency` (`ARS`|`USD`|`MEP`|`CCL`).

`data`: `account`, `currency`, `positions` (array), `positions_count`, `total_market_value`.
Cada posición: `quantity`; `instrument` {`symbol`, `name`, `type`, `asset_class`, `exchange`,
`currency`, `country`|null, `tradable`, `lot_size`, `minimum_quantity`}; `availability`
{`quantity_available`, `quantity_held`, `quantity_pending`}; `cost_basis` {`average_entry_price`,
`total_cost`, `days_held`, `entry_date`|null}; `market_value` {`current_price`, `price_change`,
`price_change_percent`}; `profit_loss` {`unrealized_pl`, `unrealized_pl_percent`};
`portfolio_weight_percent`; `is_accredited`; `maturity_date`|null; `parent_instrument`|null.

## 6. Crear orden: `POST /broker/orders`

Headers: `Authorization`, `X-Idempotency-Key` (obligatorio, UUID).

Body JSON:

| Campo | Tipo | Oblig. | Valores |
|---|---|---|---|
| `symbol` | string | sí | |
| `market_id` | string | sí | `BCBA`, `NYSE`, `NASDAQ` |
| `side` | string | sí | `buy`, `sell` |
| `order_type` | string | sí | `limit`, `market` |
| `due_date` | string | sí | `YYYY-MM-DD` (vencimiento) |
| `limit_price` | number | si `limit` | |
| `quantity` | number | uno de los dos | cantidad |
| `notional` | number | uno de los dos | monto |
| `settlement_term` | string | no | `T0`, `T1`, `T2` |

Mandar **solo** `quantity` (no `notional`). Respuesta 201 `data`: `order_id` (string),
`successful_purchase` (bool), `messages` ([{`code`, `message`}]). No trae estado.

## 7. Estimar orden: `POST /broker/orders/estimate`

Mismo body (sin idempotency key). `data`: `amount` {`currency`, `value`}, `commission`
{`currency`, `value`}, `regulatory_fees` {`market_fee` {`currency`, `value`}, `vat` {`currency`, `value`}}.

## 8. Estado de órdenes: `GET /broker/accounts/activities/operations`

No existe GET de una orden. Query: `start_date`, `end_date` (obligatorios, `YYYY-MM-DD`),
opcionales `country` (`all`|`AR`|`US`), `transaction_id`, `state`
(`all`|`completed`|`pending`|`cancelled`|`finished`), `symbol`, `currency`, `term`, `asset_type`
(`cedears`, `stocks`, `ons`, `government_bonds`, `letters`, `options`, `adrs`, ...), etc.

`data` (array): `order_id`, `ticket_id`, `order_date`, `execution_date`, `type`, `status`
(`completed`|`pending`|`cancelled`|`rejected`), `symbol`, `description`, `asset_type`|null, `term`,
`currency`, `order` {`qty`, `price`, `amount`}, `executed` {`qty`, `price`, `amount`}.
Ejecución parcial: no hay estado propio; inferir con `executed.qty < order.qty`.

Comisiones cobradas: `GET /broker/accounts/activities` → `fees` {`commission`, `vat`, `other`}.

## 9. Cancelar: `DELETE /broker/orders/{order_id}`

Solo órdenes pendientes. `data`: `order_id`, `status` (`cancelled`), `cancelled` (bool).

## 10. Moneda de CEDEARs y segmento dólar

NO ESTÁ EN LA SPEC. Cada cotización trae `currency`; moon-trade **verifica** que sea `ARS` antes
de operar un CEDEAR en BCBA y aborta si no coincide. `GET /broker/assets/{symbol}?market=` devuelve
`currency`, `asset_type`, `default_settlement_term`.
