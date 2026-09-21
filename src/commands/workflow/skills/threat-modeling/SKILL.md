---
name: threat-modeling
description: Enumerate threats from assets and trust boundaries, then from the data flows that cross a boundary, ranked by exploitability against impact, with accepted risks recorded explicitly. Use when designing a system or feature that handles trust boundaries. Not a diff-level security review -- that is `review`.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: threat-modeling
  x-zirv-version: "1"
  x-zirv-name: Threat modeling
  x-zirv-triggers: threat model,stride,trust boundary,attack surface,security design
  x-zirv-phases: design
  x-zirv-required-capabilities: repo.read
  x-zirv-context-budget-bytes: "2400"
---

An unrecorded risk acceptance and a genuine oversight look identical on
review, so a threat model that skips the record leaves nobody able to tell
later whether a gap was a decision or a mistake.

## Method

1. Enumerate assets and trust boundaries before threats. A threat list built
   without first fixing what is being protected and where trust actually
   changes hands produces items that sound alarming but do not map to a real
   boundary.
2. Walk the data flows that cross each boundary, not the ones that stay
   inside it. A flow entirely within a single trust zone is not where an
   attacker gains anything; the crossing is the interesting part.
3. Rank each threat by exploitability against impact, not by how alarming it
   sounds. A high-impact threat that requires privileged internal access
   competes for attention with a low-impact one reachable from the open
   internet, and treating them the same misallocates the fix effort.
4. For each threat, decide: mitigate, accept, or transfer. An accepted risk
   gets written down with who accepted it and why -- silence on this point is
   read as an oversight by the next person who finds the gap, not as a
   considered decision.
5. Revisit the model when a trust boundary moves -- a new integration, a
   new deployment target, a new class of user. A threat model frozen at design
   time silently goes stale as the boundaries it described shift.

Failure modes: modeling the code structure instead of the trust boundaries,
which misses flows that cross zones inside a single service; treating STRIDE
categories as a checklist to fill rather than a lens for finding real flows;
skipping low-glamour boundaries like internal service-to-service calls because
attention gravitates to the public edge.

Boundary: this is design-time enumeration and risk disposition across a
system; a diff-level check of what a specific change introduces is `review`,
and `data-source-audit` is the narrower question of whether one dataset can be
trusted.

## Contract

Report the assets, trust boundaries, the flows crossing each, threats ranked
by exploitability against impact, and the disposition of each -- mitigated,
accepted with owner and rationale, or transferred. Mark anything not
evaluated as not yet modeled rather than implicitly accepted.
