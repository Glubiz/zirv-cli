---
name: architecture-discovery
description: Map an unfamiliar system's real boundaries -- build/deploy units, data ownership, call direction, transaction edges -- before proposing or touching anything. Use when arriving in unfamiliar code or before a cross-cutting change. Not for choosing what to change -- that is `design`.
compatibility: repo.read; shell.exec makes dependency and call-graph checks materially faster.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: architecture-discovery
  x-zirv-version: "1"
  x-zirv-name: Architecture discovery
  x-zirv-triggers: map the architecture,system map,unfamiliar codebase,module boundaries,service boundaries,ownership map
  x-zirv-phases: design
  x-zirv-required-capabilities: repo.read
  x-zirv-optional-capabilities: shell.exec
  x-zirv-context-budget-bytes: "2100"
---

A folder tree and a deploy diagram both describe intent, not the running
system. Proposing a change against the documented shape instead of the actual
one produces a plan that is correct about a system that does not exist.

## Method

1. Find the build and deploy units first -- what actually ships and restarts
   together. Folder structure misrepresents this more than anything else: a
   monorepo can deploy as one unit or twenty, and only the pipeline knows
   which.
2. Trace data ownership: which unit holds the write path for each piece of
   state. A value read in a dozen places but written in one has one real
   owner, regardless of how many modules import its type.
3. Follow call direction across each candidate boundary, not just import
   statements. A synchronous call and an event published to a queue can carry
   the same information with opposite failure semantics -- one blocks its
   caller, the other loses data silently if nobody reads it.
4. Find where each transaction actually ends. The boundary is wherever
   atomicity stops, and that is invisible from a component diagram.
5. Compare the result against any documented architecture and name every
   disagreement explicitly. A stale diagram nobody challenges becomes the next
   change's false premise.

Common misses even for someone experienced: mistaking a shared library for a
shared boundary, treating a message queue as decoupling when both sides still
deploy together, and trusting a README's box diagram over what the code
actually calls.

Boundary: this only establishes what is there. Deciding what to change against
that map is `design`; writing the map down as a durable decision belongs in
`adr-authoring` only once something was actually decided.

## Contract

Report the boundaries found with the evidence for each -- not assertion --
plus every point where the running system disagrees with its documentation.
Mark anything not directly confirmed as unknown rather than inferred. This
skill only reads; it makes no change.
