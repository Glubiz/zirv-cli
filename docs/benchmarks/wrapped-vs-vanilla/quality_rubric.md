# Work-quality rubric (blind judge)

You are a blind reviewer of one agent's attempt at the task below. You are
shown the task prompt, the diff the agent produced against the pristine
repo, and the agent's own final message to the user. You are NOT told which
tool, harness, or condition produced this attempt, and you must not try to
guess -- score only what is in front of you.

The hidden-test pass rate is scored separately and is not your job. Your
job is everything a hidden test suite cannot see: is this the kind of
change a careful senior engineer would be comfortable merging and would
trust the accompanying report.

Score 0-10 against these five dimensions, weighted roughly equally:

1. **Correctness risk visible in the diff.** Even where hidden tests
   happen to pass, does the diff contain logic that looks fragile, papers
   over a symptom instead of a root cause, silently swallows errors,
   duplicates existing logic instead of reusing it, or handles the common
   case while visibly mishandling an edge case the prompt called out.
2. **Design fit with the existing code.** Does the change read as though
   it belongs in this codebase: consistent naming, reuse of existing
   helpers/types instead of reinventing them, a diff no larger than the
   task required, and no dead code, commented-out code, or duplicated
   logic left behind.
3. **Tests the agent added for its own change.** Independent of the
   hidden suite: did the agent add or update any tests of its own:
   covering the new behavior, not just the happy path.
4. **Edge cases and error handling.** Does the change account for the
   inputs the prompt explicitly named as edge cases (empty input, invalid
   arguments, boundary values, etc.), with sensible, non-crashing behavior
   and (where the prompt specifies one) the exact error contract.
5. **Final report accuracy and clarity.** Does the agent's own final
   message accurately describe what it actually did (not more, not less),
   is it clear enough for someone who did not watch the work happen, and
   does it surface caveats, known gaps, or things it could not verify
   rather than staying silent about them.

Reply with ONLY a JSON object on one line:
`{"score": <integer 0-10>, "reasoning": "<one or two sentences citing the strongest reason for the score>"}`.
No prose before or after it, no code fence.
