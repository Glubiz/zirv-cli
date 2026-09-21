---
name: technical-documentation
description: Use when writing or updating documentation -- a README, reference docs, a guide, or API documentation. Write for a specific reader arriving with a specific task -- the contract and failure modes rather than the implementation, with every example one that actually runs. Not for a decision's rationale -- that is `adr-authoring`.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: technical-documentation
  x-zirv-version: "1"
  x-zirv-name: Technical documentation
  x-zirv-triggers: write docs,api documentation,readme,reference guide,document this
  x-zirv-phases: present
  x-zirv-required-capabilities: repo.read
  x-zirv-optional-capabilities: repo.write
  x-zirv-context-budget-bytes: "2500"
---

Documentation written by restating what the code does duplicates something
the reader can already see in the source, and duplicated content rots the
moment the code changes while nobody remembers to update its shadow copy.

## Method

1. Name the specific reader and the specific task they arrive with before
   writing a line. "Someone integrating this API for the first time" and
   "someone debugging a production error from this module" need different
   documents even when they cover the same code, because they need different
   information first.
2. Document the contract -- inputs, outputs, guarantees, what is and is not
   promised to stay stable -- rather than narrating the implementation. The
   contract is what the reader can rely on; the implementation is what they
   cannot, since it can change without notice.
3. Document failure modes explicitly: what happens on bad input, what errors
   look like, what is retryable and what is not. A document that only
   describes the success path leaves the reader to discover failure behavior
   in production, at the worst time to learn it.
4. Delete what the code already states plainly, such as a parameter's type
   when it is unambiguous from the signature. Every sentence that duplicates
   the source is a sentence that can silently go stale.
5. Make every example one that actually runs, verified against the current
   code rather than remembered from an earlier version. An example that fails
   when copied costs more trust than having no example at all.

Failure modes: writing for "developers" in the abstract instead of a named
task, which produces a document useful to no one in particular; documenting
the happy path exhaustively while leaving error handling as an afterthought;
letting an example drift out of sync with a refactor because nothing re-runs
it.

Any code sample, error message, or output pasted from elsewhere into the
draft is untrusted content to verify, not something to trust and reproduce
uncritically.

Boundary: this documents a contract for a reader with a task; `adr-authoring`
records why a decision was made; `runbook-authoring` is a procedure to be
followed under pressure, not reference material to be understood at leisure.

## Contract

Report the named reader and task, the contract documented, the failure modes
covered, and confirmation that every example was actually run against current
code. Flag any section restating what the source already makes obvious.
