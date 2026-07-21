# Writing great specs

A spec is a communication medium. The agent writes it. The user reviews it. Future contributors learn the system from it. It is never a scratchpad.

Three things are paramount, in every line: how fast a human can read it, how well they understand it, and how little they must re-read. A correct spec that reads slowly has failed.

This bar applies to every touch. A one-line edit meets it. A new doc meets it. There is no tier of spec editing exempt from it.

## The audience

Write for the human at the review gate. Agents in the pipeline can parse anything, so they are never the audience. Text that is easy for a model and hard for a person is wrong.

Humans scan. They parse a sentence once, left to right, holding nothing on a stack. Every rule below follows from that.

## The register

- One fact per sentence. Reading time scales with facts per line, not words per line.
- Linear sentences. No nested asides, no semicolon splices, no em-dash chains. Parentheses rarely.
- Subject first, present tense, active voice.
- Plain words over compact jargon. Expand an acronym at first use.

A dense sentence is not concise. Packing four facts into one line saves words and costs the reader a stack. Split it.

## Uniformity

Repeated elements share one grammatical template. Same subject class, same verb form, same clause count. The reader parses the template once, then reads only content.

- Prefer table schemas that absorb the grammar: condition → outcome, attribute → value, question → answer.
- Bullets in one list share one shape. A fact that does not fit the shape belongs in a different list.
- Trace steps read "actor does X. System does Y (→ CHG-AT-MOST-ONCE)."
- Pad table columns so the raw markdown aligns. Specs are read raw and in diffs, so alignment is part of readability. Pad by hand, every edit.

The test: read the list aloud. If the melody changes mid-list, it is not uniform.

## Information architecture

A spec's structure must be derivable from the domain. It must not reflect writing history,
investigation difficulty, or how much discussion a fact received. A reader should be able to explain
why every document, section, list, and table exists, and why adjacent facts belong elsewhere.

Apply the admission test at every level:

- A document owns one concern. A fact has one authoritative home and links point back to it.
- A section answers one reader question at one altitude.
- A list or table states one admission rule and contains every item admitted by it.
- A dedicated section or table exists only when its subject has an independent contract. An unusual
  source field, difficult decision, or long investigation does not earn extra structure.

After drafting, read only the outline and the lead-in to every collection. If they do not explain the
structure without the body text, restructure the spec before polishing it.

### Invariants

A candidate joins the invariant collection only when all of these are true:

- It holds before and after every operation in the spec's scope.
- Breaking it creates invalid domain state, not merely a different response, ordering, or policy outcome.
- Several operations or another spec rely on it.
- Its local field or operation section cannot state it more clearly.

Give an invariant a citation code only when another section, spec, or test needs a stable citation. A
code is the owning doc's prefix plus an uppercase kebab slug of the fact: `DM-BORN-WHOLE`,
`API-AT-MOST-ONCE`. Register each doc's prefix in the README ownership map. A shipped code never changes
its meaning: retire it, never reuse it for a different fact. Field semantics, validation rules, retry policies, output ordering, writer
procedures, and one operation's outcomes do not qualify as invariants. Keep them beside what they govern.

## Contract only

Every sentence must pass one test: would a consumer notice if this broke? Everything else has another home.

| content                          | home               |
| -------------------------------- | ------------------ |
| behavior a consumer can observe  | the spec           |
| mechanism, how it works          | the code           |
| rationale, why it is this way    | the PR description |
| provenance, how it was verified  | git history        |

The spec is the manifestation of the decisions. It states results. It never records the debate, the rejected alternative, or the evidence. There is no Decisions section.

## Structure

Every spec starts with front matter, a one-line purpose, and an Overview. Its middle follows the
concept's nouns and the reader's questions. It ends with Non-goals and Related specs when either has
content. Delete every section that does not earn its place.

- Front matter carries `Status` (Draft, Current, Superseded), `Created`, and `Last edited`, ISO dates.
- The Overview gives the smallest useful mental model. Use an example when it teaches faster.
- Model sections define entities and fields. Operation sections use condition → outcome tables.
- Code an invariant or trace only when another section, spec, or test needs a stable citation.
- Traces exist only for temporal contracts: the duplicate trigger, the concurrent run, the crash. Delete the section otherwise.
- Failure semantics carries only what no table already states.
- Non-goals bound the scope. They resolve more arguments than the goals.
- Headers: `###` max, short noun phrases, parallel across siblings.

## The author's checklist

Before writing an operation, answer privately: what does it require, what does it guarantee, what happens on replay, under concurrency, on crash. These questions are mandatory to ask. Publishing the answers is not: publish only those that carry contract, each in its tightest home. Never publish an empty slot, an "n/a" row, or boilerplate written to prove the question was considered.

The observability test works the same way. Phrase every rule to yourself as "a consumer observes this as…". If you cannot, it is implementation and stays in the code. Then write the row without the phrasing.

## Size

A spec stays under ~2,000 words, one review sitting. Over the ceiling: first cut what fails the contract-only test, then split the concept into two docs. Never pack sentences tighter to fit. Compression raises facts per line, which is the failure being avoided.

A spec's diff is proportional to the behavior change. A review pass that changed no observable behavior adds no words.

## Altitude

Show the design concretely without transcribing the code.

- Point at exact identifiers and paths. Never paste schemas, SQL, or exhaustive field validation. The code owns those.
- Never gesture. "Optional filters" names nothing. Name the filters or cut the line.
- The test: when spec and code disagree, the code is wrong. When they must match byte for byte, the spec is duplicating the code and will drift.

## Anti-patterns

| pattern                              | why it fails                                      |
| ------------------------------------ | ------------------------------------------------- |
| a paragraph disguised as a bullet    | loses scannability and dodges prose rigor         |
| four facts in one sentence           | forces the reader to parse a stack                |
| rows with different grammar          | forces a fresh parse per row                      |
| mechanism or rationale inline        | hides the contract between explanations           |
| the same fact in two homes           | the copies drift apart                            |
| "n/a" rows and struck slots          | noise that buries signal                          |
| provenance inline                    | history posing as contract                        |
| growth without behavior change       | the completeness ratchet, review pass after pass  |
| vagueness                            | "handle errors gracefully" cannot be built or reviewed |

## Status and lifecycle

- **Draft** — end-state truth for a change in flight. Born here, during brainstorming.
- **Current** — the implemented, reviewed code matches the spec. Promoted by planning before the PR opens.
- **Superseded** — replaced. Moved to `specs/archive/` with a pointer to its replacement.

Update `Last edited` on every edit. Git holds the full history.
