---
name: simplify
description: Improve the quality of code that already works -- reuse what exists, remove needless complexity, fix obvious inefficiency, and put logic at the right level -- then apply the changes. Use after a change is functionally complete. Quality only; it does not hunt for defects -- that is `review`.
compatibility: repo.read and repo.write; test.run makes behaviour preservation checkable.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: simplify
  x-zirv-version: "1"
  x-zirv-name: Simplify changed code
  x-zirv-triggers: simplify,clean up this code,reduce complexity,tidy this up,too complicated,deduplicate
  x-zirv-phases: review,implement
  x-zirv-required-capabilities: repo.read,repo.write
  x-zirv-optional-capabilities: test.run,shell.exec
  x-zirv-context-budget-bytes: "3000"
---

Simpler means fewer things a reader must hold in their head, not fewer
characters. A cleanup that compresses three clear lines into one clever
expression has made the code shorter and worse, and a cleanup that changes
behaviour is not a cleanup at all.

## Scope

Work on the code the change touched, plus only what it must touch to be made
simple. A wider refactor buries the real change under churn, makes the diff
unreviewable, and belongs in its own change with its own intent.

## Method

1. Read the whole change before editing any of it. The best simplification is
   often across hunks -- two new helpers doing one job -- and is invisible when
   each hunk is polished in isolation.
2. Reuse first. For each new helper, type or constant, search for one that
   already exists; a second implementation of an existing utility is the most
   expensive kind of complexity because both now have to be kept in agreement.
3. Remove what does not pay for itself: an abstraction with one caller, an
   option nobody passes, a parameter that is always the same value, handling
   for a state the types already exclude, a comment restating the line below
   it, code the change orphaned. Flatten nesting with early returns where that
   shortens the path a reader follows.
4. Fix inefficiency only where it is plain from reading -- repeated work
   inside a loop, a query per item, a copy that is never needed, quadratic
   behaviour where linear is equally clear. Anything that needs a measurement
   to justify is out of scope here, and clarity is never traded for speed.
5. Check altitude: each function should read at one level of abstraction, and
   each piece of logic should live where the next reader will look for it.
   Byte-level detail inlined in a high-level flow, or policy hidden inside a
   low-level helper, is misplaced even when it is correct.
6. After each change, run the checks that cover it. A simplification nothing
   verifies is a behaviour change waiting to be discovered; when no check
   covers the code, say so and prefer leaving the complexity in place.

Failure modes: removing a check that looked redundant but guarded a real
case; renaming or reshaping a public interface as a side effect; folding a
bug fix silently into a cleanup -- a defect you notice gets reported for
`review`, never quietly repaired here, because the fix deserves its own test
and its own line in the record.

## Contract

Report each change with its category -- reuse, simplification, efficiency or
altitude -- its location, and why behaviour is preserved, then the checks run
and their result. Name what you considered and left alone, and why. "Nothing
here is worth changing" is a complete and respectable answer; churn invented
to justify the pass is the failure this skill exists to prevent.
