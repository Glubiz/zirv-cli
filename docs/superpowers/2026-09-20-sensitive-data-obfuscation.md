# Sensitive-data obfuscation design

Issue: [#466](https://github.com/Glubiz/zirv-cli/issues/466)

## Goal

Prevent credentials and supported personal data from crossing any
Zirv-controlled model or remote-service boundary while preserving enough type
and identity information for an agent to do correct work. The same guarantee
applies to the native runtime and the supervised/meta harness.

## Boundary

Detection is a pure, deterministic transformation. Impure code loads operator
configuration and a per-repository vault, holds a cross-process transaction
lock, applies the transformation, and persists the mapping as mode 0600.

The native runtime masks the complete `ProviderRequest` immediately before
either direct-provider transport or official-harness execution. The meta
harness masks every Zirv-composed prompt and uses `PostToolUse`/`PreToolUse`
hooks for model-context masking and local-action rehydration.

`UserPromptSubmit` cannot be rewritten by the hook protocol, so it is flagged
by default and can be blocked by operator policy. The README states this gap.

## Detection and placeholders

V1 detects supported credential families plus email, phone, Danish CPR,
validated IBAN and Luhn-valid payment cards. Operators can add regular-
expression kinds and literal values. Each value receives a stable typed token:

```text
ZIRV_SECRET_<KIND>_<N>
ZIRV_PII_<KIND>_<N>
```

The same value maps to the same placeholder across turns, workers, handoffs
and restarts for one repository. Rehydration is exact and unknown placeholders
fail closed at device-action boundaries.

## Failure policy

- Corrupt or ambiguous vault state fails closed.
- A transaction lock covers load, allocation and save.
- Native input is acknowledged only after final masking succeeds.
- Shared `.zirv/memory/` and `.zirv/work/` writes retain placeholders.
- `mode = "off"` is an explicit operator choice and preserves original bytes.

## Known limits

Zirv cannot inspect transport a harness performs outside its hooks, model
responses, third-party plugins that bypass Zirv, transformed/encoded values,
or arbitrary names and addresses not supplied as operator literals/patterns.
