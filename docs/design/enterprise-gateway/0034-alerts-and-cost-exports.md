# 0034: Alerts and cost exports

Status: built (one slice).

## Decision

`serve` sends no alerts and writes no scheduled exports itself. Both are already served by what an
operator runs next to a gateway, if the gateway exposes the right facts:

- **Alerts** are Prometheus alert rules (Alertmanager routes them to a pager, chat or mail) over the
  `/metrics` series the admin listener already serves. `serve` does not hold a notification
  credential, a webhook URL or a retry queue, and a gateway that pages people is a second system to
  secure and to get wrong.
- **Cost exports** are the usage log (`[logging] usage_log`, note 0014): one JSON line per provider
  attempt with tenant, key, tags, deployment, provider, model, tokens, `cost_usd` and `cost_source`
  (`null` is unknown, never free). A log shipper or a scheduled job aggregates it into whatever the
  finance team reads. Aggregation is the consumer's: the gateway keeps no history to export.

## What is added: spend counters

Three counters in `/metrics`, fed when an attempt ends, so a cost alert needs no log parsing:

| Series | Labels | Meaning |
| --- | --- | --- |
| `llmshim_gateway_cost_usd_micros_total` | `provider`, `source` | attributed spend in millionths of a USD (`source` is `provider`, `provider_floor` or `catalog`) |
| `llmshim_gateway_cost_unknown_total` | `provider` | attempts whose cost is unknown |
| `llmshim_gateway_tokens_total` | `provider`, `kind` | tokens (`input`, `output`, `cache_read`, `cache_write`; reasoning is inside `output`) |

Integer micro-dollars keep the counter exact; divide by 1e6. An attempt with an unknown cost adds to
`cost_unknown_total` and **never** to the cost counter: a rising unknown count means the cost series
under-reports, which an alert should treat as a reason not to trust it (the rule below does).

**Labels are the configured provider names and a closed set, never a tenant, key or model.** The
gateway's metrics deliberately have no per-tenant dimension (cardinality, and a tenant's name does not
belong in a scrape): per-tenant spend is the usage log's job, and a per-tenant *budget* is enforced
by admission (0030), not by an alert. The series exist only when the usage recorder does, that is,
when `[logging] usage_log` or `[telemetry]` is configured; without either, `serve` keeps no per-attempt
state, and the fleet job scope carries no credential hash (note 0014), which this note does not change.
They are compiled only with `serve`: default, `proxy`, `gateway` and `gateway-redis` `/metrics` are
unchanged.

## Example rules

```yaml
groups:
  - name: llmshim-spend
    rules:
      - alert: LlmshimSpendRate
        # More than $50 an hour on one provider (micro-dollars / 1e6).
        expr: sum by (provider) (increase(llmshim_gateway_cost_usd_micros_total[1h])) / 1e6 > 50
        for: 10m
      - alert: LlmshimCostUnderReported
        # Attempts with no price: the spend counter is a lower bound while this fires.
        expr: sum by (provider) (increase(llmshim_gateway_cost_unknown_total[1h])) > 0
        for: 30m
      - alert: LlmshimShedding
        # Requests refused because a lane is overloaded (the other reasons are model_not_allowed,
        # timeout, unauthorized and upstream).
        expr: sum by (provider) (increase(llmshim_gateway_rejected_total{reason="overloaded"}[15m])) > 0
```

A spend cap's refusal is an HTTP 429 answer to the caller, not a `rejected_total` reason; budget
use is watched through the cost counters above and enforced by admission (0030).

## Not built

- Per-tenant or per-key metrics, budget-remaining gauges, and webhook, mail or chat senders.
- A scheduled export job, a report endpoint or any history held by the gateway.
- Spend counters without a usage log or telemetry (they need the usage recorder).

## Tests

`gateway::attempt::usage_records` (unit): a record with a known price is counted as exact
micro-dollars under its provider and source, with the non-zero token kinds (a wrong multiplier fails
it). `integration_serve_deployments::finished_attempts_feed_token_and_unknown_cost_counters_labelled_by_provider_only`
(process): an unpriced model counts as unknown and writes no cost series, tokens are counted per
provider, and no spend line carries a tenant, key, model or request label.
