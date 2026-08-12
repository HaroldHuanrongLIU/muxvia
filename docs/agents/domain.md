# Domain documentation

Muxvia is a single-context repository. Engineering skills and agents must consume its domain documentation before exploring or changing the relevant behavior.

## Before exploring

- Read `CONTEXT.md` at the repository root.
- Read the records under `docs/adr/` that touch the area being explored or changed.
- If either source is absent, proceed silently rather than creating speculative terminology or decisions.

## Use the glossary vocabulary

Use the canonical terms defined in `CONTEXT.md` in issue titles, test names, interfaces, plans, and explanations. Do not substitute terms that the glossary explicitly marks under `_Avoid_`.

If a necessary concept is missing, first determine whether it is unnecessary new language or a real domain gap. Resolve genuine gaps through `domain-modeling`; keep implementation details out of `CONTEXT.md`.

## Respect architecture decisions

Surface any contradiction with an existing ADR explicitly. Do not silently replace an accepted decision. Add or supersede an ADR only when the new choice is hard to reverse, surprising without context, and the result of a real trade-off.
