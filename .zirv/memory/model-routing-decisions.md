## Memory
- Key: model-routing-decisions
- Written-by: claude
- Written: 1791637440
- Verified: 1791637440
- Source: explicit

Operator decisions for evidence-driven model routing (2026-10-10, answering the five questions in the routing investigation):

1. Promotion: a newly discovered model stays on probation and zirv keeps the incumbent until evidence shows the new model is at least as good. An explicit model choice by the user (a flag, operator config, or a pin) always wins and is never re-routed.
2. Probes run automatically, triggered by the 24-hour refresher or whenever zirv starts, with a hard minimum of 24 hours between runs. No per-run approval.
3. Claude is dispatched by dated model id (for example claude-sonnet-5-5) instead of an alias, so zirv can also hold back a Claude upgrade.
4. Evidence comes from both real-world use (the scorecard and outcomes) and synthetic benchmark probes, combined.
5. Routing is across every installed harness. zirv chooses the harness and model per task from the evidence for that task's role and complexity, not just within one harness.

6. New models and new families must be picked up with no code change (operator, same day). Supervisor ruling 82227517 decided how:
   - Evidence-only adoption: any family name is parsed generically, and a new family becomes a vendor-level probation candidate with no ladder rung and no incumbent.
   - It is probed automatically, and after MIN_SYNTH synthetic samples the router may pick it on merit.
   - models.dev is an existence source only. A probe confirms account availability, and a probe that produces no output backs the model off for 7 days.
   - The ladder and fallbacks are unchanged, because price alone cannot establish a tier.

Why: a vendor can ship a cheaper but worse model, and zirv previously adopted the newest version of each family on sight (catalogue::base_ladder).
