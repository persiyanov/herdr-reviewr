---
Status: Draft
Created: YYYY-MM-DD
Last edited: YYYY-MM-DD
---

<!--
One concept per doc: the data model, the API, a subsystem.
State end-state truth — what must be TRUE when the change is done, not what's built or when.
Keep the sections in this order; delete any that don't earn their place. Scale to the change.
Update `Last edited` on every edit. Dates are ISO (YYYY-MM-DD).
The bar: lead with an example, ### max, one idea per bullet, no schema transcription.
-->

# <Concept name>

<One sentence: what this concept is and why it exists.>

## Overview

<The concrete design. Lead with one realistic example, then a field table.>

## Behavior

<Short present-tense statements of what must be true. Show both outcomes for anything that can fail.>

## Failure semantics

<!-- Required if this concept is retryable, billable, persistent, or side-effecting. Otherwise delete. -->

<What happens on the second run and under concurrent runs. State the contract, not a label.>

## Non-goals

<What this explicitly does NOT do.>

## Decisions

<Each: the decision, the main alternative rejected, a one-line why.>

## Open decisions

- None.

## Related specs

<Relative links to the specs this one depends on or borders.>
