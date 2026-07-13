---
Status: Draft
Created: YYYY-MM-DD
Last edited: YYYY-MM-DD
---

<!--
A spec is a communication medium. The user reviews it. Contributors learn from it.
It is never a scratchpad. Optimize for reading speed, even in a one-line edit.
The full bar: references/writing-great-specs.md. The essentials:
- One concept per doc. End-state truth: what must be TRUE when the change is done.
- One fact per sentence. Linear sentences, no asides.
- One grammatical template per list or table. Schema-first tables, columns padded to align raw.
- Contract only: mechanism -> code, rationale -> PR, provenance -> git.
- One home per fact. Cite by number everywhere else.
- Under ~2,000 words. Sections in this order. Delete sections that don't earn their place.
-->

# <Concept name>

<One sentence: what this is and why it exists.>

## Overview

<Lead with one realistic example, then a field table.>

```json
{ "id": "chg_1A2b3C", "amount": 1099, "status": "succeeded" }
```

| field    | type    | meaning                                          |
| -------- | ------- | ------------------------------------------------ |
| `id`     | string  | unique charge identifier                         |
| `amount` | integer | amount in the smallest currency unit             |
| `status` | enum    | `pending`, `succeeded`, `failed`, or `unknown`   |

## Behavior

<Rules as schema-first tables. Number invariants for citation. One grammatical shape per table.>

| #  | Always true                                                    |
| -- | -------------------------------------------------------------- |
| I1 | A charge is captured at most once.                              |
| I2 | A charge reaches exactly one terminal status and never leaves it. |

<Operations as condition → outcome rows:>

| request                                    | outcome                                          |
| ------------------------------------------ | ------------------------------------------------ |
| valid `amount`, chargeable `source`        | `2xx`, one `pending` charge committed            |
| invalid `amount`                           | `400 invalid_request`, nothing persists          |
| same `Idempotency-Key` replayed within 24h | the original response, nothing charged twice (→ I1) |

## Traces

<Only for temporal contracts: the duplicate, the race, the crash. Delete otherwise.
Steps share one shape: "actor does X. System does Y (→ I1)."->

**T1 — crash between debit and record**

1. The caller creates a charge. The row commits `pending`.
2. The processor debits the card.
3. The service crashes before recording the outcome.
4. Recovery marks the charge `unknown`, terminal (→ I2).

## Failure semantics

<Only what no table above states: the second run, the concurrent run, the crash.>

## Non-goals

<What this explicitly does not do. One shape per bullet.>

- Does not handle refunds. See the refunds spec.

## Related specs

- [refunds](./refunds.md)
