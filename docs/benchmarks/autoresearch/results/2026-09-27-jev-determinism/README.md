# Jev determinism campaigns, 2026-09-27

**Outcome.** Every production Jev decision site was measured for determinism: the same request, repeated uncached, should lead production to the same action. The autoresearch runner then swept each site's floors.

- **No floor default changed.** Seventeen of the 21 measured sites are fully deterministic at their compiled floors on these corpora. For the other four, every candidate that gains stability either loses correctness or failed its confirmation.
- **Cache TTL raised.** The one change that adds determinism without trading correctness ships in this release: the Jev answer cache now keeps an identical request's answer for a week (`[jev] cache_ttl_secs` default 86,400 → 604,800). The model, questions and state are all part of the cache key.
- **Two gates sites never reach Jev.** artifact-substance and gate-reclass are refused by the metadata guard on every call (see below), so they always take the deterministic fallback.

Committed reports: 21 campaigns, 2,018 uncached trials, $0.229 of Jev spend. Across all 60 campaign runs of this exercise, including exploration and confirmation rounds, Jev spend was about $0.79.

## How it was measured

- **The probe.** `zirv ctx jev probe --site <SITE> --case <case.json> --reps K` sends one site's production question(s) K times with the cache off. It applies the site's production floor and answer-to-action rule, and prints the action production would take on each repetition. It covers 24 sites. The probe-only env keys `ZIRV_CTX_JEV_PROBE_MIN_CONFIDENCE|_MIN_MARGIN` let a campaign vary any site's floor, including safety sites, without production ever reading an override.
- **Probe metrics.** `jev_probe_trial.py` (K=5, or K=10 for handoff-thin) scores `quality` as the mean per-item modal share of those actions. `correctness` is agreement with `jev-cases/<site>/labels.jsonl`.
- **Intake metrics.** `decision_trial.py --reps 5` does the same for intake, comparing every production-acted decision field (`ACTED_DECISION_FIELDS` plus the derived `clarify` boolean).
- **Promotion.** Every campaign uses `[criteria] objective = "quality"` with `min_effect = 0.01`: a candidate is promoted only if the lower bound of its paired-bootstrap stability CI clears 0.01, correctness does not regress past 0.05, and a holdout confirms.
- **Model:** `jev-1.13.0`, the pinned `[proxy.typesafe] model`.

## Results (committed reports; baseline = compiled floors; stability / correctness)

| site | baseline | outcome |
|---|---|---|
| memory, context, harvest_screen, compaction_select, dispatch, launch_effort, classify, inject | 1.000 / (0.12 to 0.90) | deterministic; no candidate better |
| crash, judge, approve_lower, intake_plan, inject_screen, missing_tests, stop_verify, review_disposition, review_dedup | 1.000 / (0.38 to 0.75) | deterministic; no candidate better |
| handoff_select (select + thin) | 0.935 / 0.78 | inconclusive at validate |
| handoff_thin (confirmatory) | 1.000 / 0.75 on dev | conf-085 discarded at screen |
| intake | 0.963 / 0.64 | inconclusive at validate (correctness CI) |
| approve_escalate (confirmatory) | 0.971 / 0.48 | inconclusive at validate (stability CI) |
| artifact-substance, gate-reclass | fallback on every call | not campaigned (see below) |

Deterministic does not mean active. For most of the deterministic sites, Jev's answers on these corpora sit well away from the site's thresholds. Two examples are memory relevance at 0.44–0.73 against a 0.3 prune threshold, and stop-verify at 0.16–0.24 against 0.9. So the same action recurs, often the fallback. Low correctness values (dispatch, launch_effort, crash, review_disposition) mean the hand-written labels disagree with Jev's consistent answer; correctness only guards against regression here.

## Where Jev still flips, and why no floor fixes it

In every remaining case, Jev samples a noticeably different confidence or margin on each identical uncached request, and the site's floor sits inside that band.

**handoff-thin** (`HANDOFF_THIN_FLOOR` 0.9). The answer value is stable, but its confidence for one identical request spans a wide band that depends on handoff size:

| case | facts [task, next_step, constraints, files, blocked] | `thin` confidence range |
|---|---|---|
| hsel-003 | [14, 5, 0, 0, 0] | 0.84–0.93 |
| ht-001 | [18, 6, 0, 1, 0] | 0.46–0.79 |
| ht-002 | [25, 9, 5, 0, 1] | 0.30–0.67 |
| ht-005 | [85, 25, 0, 0, 0] | 0.60–0.90 |
| ht-007 | [190, 65, 20, 1, 0] | 0.00–0.19 |

- Floors of 0.45–0.8 stabilise tiny handoffs but destabilise small ones. 0.45 cost 0.17–0.18 correctness.
- conf-085 looked best, and one confirmatory run (n=60) was significant on both axes: stability +0.028, CI [0.008, 0.052]; correctness +0.035, CI [0.013, 0.060]. But the stability CI lower bound stayed below the pre-declared 0.01.
- The pre-registered follow-up with doubled reps was declared final. It discarded conf-085 at screen, because that run's dev split showed no instability to remove. So the 0.9 floor stays.

**Intake clarify.** Only the `clarify` decision ever flipped; intent, workflow, complexity, risk, tiers and domains never did.

| `ZIRV_CTX_PROXY_MIN_MARGIN` | stability |
|---|---|
| 0.10 | 1.000 |
| 0.15 | 0.99 |
| **0.20 (default)** | 0.95–0.98 |
| 0.25 | 0.92 |
| 0.30 | 0.75–0.77 |
| 0.40 | 0.99 |

Every more-stable margin fails the correctness guard. The residual flips at 0.2 come mostly from "Audit and fix the authentication bypass…", which lower margins make ask for clarification every time, though its label says not to. 0.10 would also break the `DEFAULT_MIN_MARGIN > 0.14` bound that protects intent and workflow, and 0.40 makes most Jev-driven intake fields fall back.

**approve-escalate** (`APPROVE_ESCALATE_MIN_CONFIDENCE` 0.5). Jev's "risky" confidence for one identical command spans about 0.2–0.64, and 19% of your real uncached approve answers (09-25) land within ±0.05 of 0.5. Lowering the floor (the conservative direction, more Allow→Ask) raised correctness sharply: esc-018 +0.22, CI [0.11, 0.34], on 13 confirmatory validation cases. But it did not raise stability (−0.03, CI [−0.07, +0.02]), because new straddles appear. So it is not a determinism change, and this campaign does not ship it (see follow-ups).

**What removes these flips:** memoization. With the one-week TTL, a repeated identical request always gets the answer it got the first time. approve's production cache-hit rate is 65%, so a large share of real approve decisions are repeats that this makes consistent.

## Sites that never reach Jev

`artifact-substance` and `gate-reclass` (the `[jev] gates` sites in `workflow/engine.rs`) send text-bearing state (`{artifact_kind, artifact_text}`, `{task, changed_paths, …}`) built with plain, non-metadata `Question` constructors. `jev::safe_metadata_request` refuses both on every call, and production's own tests assert the network is never dialled. Their acted decision is therefore always the deterministic fallback. The probe reports exactly that, so no campaign was run for them.

## Real-data cross-check

Your uncached production answers in `jev-decisions.jsonl` (181 of 929 rows) confirm the campaign picture:

- **approve** is the only site with a dense near-floor region today.
- **memory** has an older low cluster (noul 0.15–0.30) near its 0.3 threshold, but it appears only in 09-20 to 09-23 rows. Every memory answer since has been ≥ 0.45, matching a live sweep of 140 random memory fact states.

## Follow-ups (not determinism changes; not in this release)

- **approve-escalate accuracy:** the 0.5 floor lets through many mid-risk commands that a careful operator would want asked about. esc-018 raised correctness by 0.22, but that is an accuracy/safety decision for the operator.
- **gates sites:** artifact-substance and gate-reclass cannot ask Jev at all today. Either give them a metadata projection or remove the Jev leg.
- **Cache growth:** the Jev cache is never pruned (`jev-cache/` grows without bound); this was already true before the TTL change.
- **handoff-thin and intake clarify:** remaining per-call noise would need source changes, such as majority-of-N sampling or a clarify-specific margin, evaluated as `[candidate_space.source_patch]` campaigns.
- **Corpus limits:** corpora are hand-written metadata fixtures (16–40 cases per site). A site that is stable here can still flip on real inputs near its own thresholds that no corpus case covers.
