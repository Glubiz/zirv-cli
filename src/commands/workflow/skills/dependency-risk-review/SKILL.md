---
name: dependency-risk-review
description: Review a project's dependencies for supply-chain risk and upgrade risk separately -- maintenance signal, single-maintainer exposure, transitive surface, license obligations, and pinning strategy. Use when auditing what a project depends on. Not for reviewing the application code that uses those dependencies -- that is review.
compatibility: repo.read; network.access lets you check current maintenance and advisory status instead of relying on a stale local cache.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: dependency-risk-review
  x-zirv-version: "1"
  x-zirv-name: Dependency risk review
  x-zirv-triggers: dependency audit,supply chain review,outdated packages,license review,audit dependencies
  x-zirv-phases: review
  x-zirv-required-capabilities: repo.read
  x-zirv-optional-capabilities: network.access
  x-zirv-context-budget-bytes: "2200"
---

Supply-chain risk and upgrade risk look similar in a dependency list but call
for different mitigations, and treating them as one problem produces advice
that fits neither -- pinning a compromised package tighter does not help, and
auditing a stale-but-safe package's maintainer does not either.

## Method

1. Separate the two questions explicitly for each dependency of concern:
   could this package or its maintainer be compromised (supply-chain), and
   does staying on this version cost more over time than upgrading (upgrade
   risk). Answer them independently.
2. Check maintenance signal -- commit recency, issue response time, release
   cadence, and whether the project depends on a single maintainer with no
   succession plan. A single-maintainer package is one account compromise
   away from a malicious release.
3. Look past direct dependencies to transitive surface: a small, careful
   direct dependency can still pull in a large or poorly maintained subtree.
   The attack surface is the whole resolved graph, not the manifest.
4. Check license obligations against how the project is distributed -- a
   copyleft license in a dependency can impose obligations the team has not
   agreed to, especially after a transitive upgrade changes what got pulled
   in.
5. Evaluate the pinning strategy: exact pins block security fixes from
   arriving automatically; loose ranges let an unreviewed release in
   automatically. State which failure mode the current strategy is exposed
   to.
6. Name the cost of not upgrading, which compounds quietly -- growing diff
   size, deprecated APIs, expiring security support -- against the visible,
   one-time cost of upgrading now. Deferral is a choice with a price, not a
   neutral default.

## Contract

Report supply-chain findings and upgrade-risk findings as separate lists, the
transitive exposure found, any license obligation surfaced, the pinning
strategy's failure mode, and the compounding cost of deferring each flagged
upgrade. Say "no advisory data available" rather than assuming a package is
safe from its absence in a stale local cache. Reviewing how the application
code uses these dependencies belongs to `review`.
