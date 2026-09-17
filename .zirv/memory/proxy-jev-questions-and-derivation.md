## Memory
- Key: proxy-jev-questions-and-derivation
- Written-by: claude
- Written: 1789641803
- Verified: 1789641803
- Source: explicit

Decided 2026-09-17 from a live 24-case Jev battery (PR #679 round 3): Jev is asked exactly five questions -- intent, complexity (score), risk (score), workflow (choice over packs + none), needs_clarification (noul). Execution, seat tier, worker tier and seat role are DERIVED from the merged complexity in decision.rs::finalize_derived_fields (trivial -> direct/cheap/single, bounded -> bounded/standard/single, substantial|architectural -> orchestrated/frontier/orchestrator), plus floors (security 
[truncated]
