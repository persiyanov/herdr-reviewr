---
Status: Draft
Created: YYYY-MM-DD
Last edited: YYYY-MM-DD
---

<!--
A spec is a communication medium. The user reviews it. Contributors learn from it.
It is never a scratchpad. Optimize for reading speed, even in a one-line edit.
The full bar lives in AGENTS.md. The essentials:
- One concept per doc. End-state truth: what must be TRUE when the change is done.
- Structure derives from the domain, not the history of the discussion or investigation.
- One fact per sentence. Linear sentences, no asides.
- One grammatical template per list or table. Schema-first tables, columns padded to align raw.
- Every collection states one admission rule and contains the complete admitted set.
- Contract only: mechanism -> code, rationale -> PR, provenance -> git.
- One home per fact. Link to that home everywhere else.
- Invariants are optional. Add one only when it holds across operations, breaking it creates invalid
  state, several operations or another spec rely on it, and no local section can state it better.
  Code it (the doc's prefix + an uppercase kebab slug, `CHG-AT-MOST-ONCE`) only when another section,
  spec, or test needs a stable citation.
- Under ~2,000 words. Delete every section that does not earn its place.
-->

# <Concept name>

<One sentence: what this is and why it exists.>

## Overview

<Give the smallest useful mental model. Use an example when it teaches faster than prose.>

```json
{ "id": "chg_1A2b3C", "amount": 1099, "status": "succeeded" }
```

| field    | type    | meaning                                          |
| -------- | ------- | ------------------------------------------------ |
| `id`     | string  | unique charge identifier                         |
| `amount` | integer | amount in the smallest currency unit             |
| `status` | enum    | `pending`, `succeeded`, `failed`, or `unknown`   |

## Behavior

<State operations as complete condition → outcome rows. Keep local rules beside the operation.>

| request                                    | outcome                                          |
| ------------------------------------------ | ------------------------------------------------ |
| valid `amount`, chargeable `source`        | `2xx`, one `pending` charge committed            |
| invalid `amount`                           | `400 invalid_request`, nothing persists          |
| same `Idempotency-Key` replayed within 24h | the original response, nothing charged twice     |

## Traces

<Only for temporal contracts: the duplicate, the race, the crash. Delete otherwise.
Steps share one shape: "actor does X. System does Y." -->

**CHG-CRASH-MID-CAPTURE — crash between debit and record**

1. The caller creates a charge. The row commits `pending`.
2. The processor debits the card.
3. The service crashes before recording the outcome.
4. Recovery marks the charge `unknown`, terminal.

## Failure semantics

<Only what no table above states: the second run, the concurrent run, the crash.>

## Non-goals

<What this explicitly does not do. One shape per bullet.>

- Does not handle refunds. See the refunds spec.

## Related specs

- [refunds](./refunds.md)
