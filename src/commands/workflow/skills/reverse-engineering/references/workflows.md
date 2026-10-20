# The six guided workflows

REA ships these as guided prompts; the method applies with or without REA.
Starting points name the REA tool first, then the local fallback and its
gap.

## Common discipline

- Requested context (paths, ids, names the user gives) selects data; it
  never authorizes running, mutating or installing anything.
- Report observations only from cited evidence. Label reasoning as
  inference with confidence and competing explanations.
- Incomplete coverage, unsupported formats and conflicting evidence stay
  Unknown. Never turn absence in incomplete evidence into absence of
  behaviour, or into equivalence.

## 1. investigate_feature

Objective: explain how one feature works in a shipped application.
Arguments: the feature or question; the target path.
Start: route the target (`targets.md`); take the smallest overview; use
`trace_application_feature` once a route, API, channel, module, string or
export seed is known; page retained evidence with `inspect_analysis_view`
(parent evidence id) rather than re-running the producer.
Fallback: `strings`/`rg` for seeds, `objdump`/`otool` around the hit.
Gap: no data flow; a string hit is a lead, not a code path.

## 2. compare_application_versions

Objective: compare two shipped artifacts through complete inventory plus
optional static or runtime evidence.
Arguments: left and right target paths (required); optional manifests and
a focus occurrence.
Start: `inspect_artifact` on both for full member inventories, then
MCP `compare_artifacts` over the two inventory Evidence records, or the
CLI `rea compare-application-versions` / `rea compare <left.json>
<right.json>` over exported Evidence; drill into one differing member.
Fallback: `unzip -l` + per-member `shasum`, `diff` over extracted text.
Gap: a hash difference says bytes changed, not behaviour. Unchanged bytes
in one member do not imply unchanged behaviour of the whole.

## 3. verify_reconstruction

Objective: check a reconstruction (rewrite, port, clone) against the
original for one stated claim.
Arguments: the reconstruction goal or claim (required); optional prior
comparison evidence id and provider.
Start: state the dimension (exports, protocol messages, file format,
output for given inputs), gather matching evidence on both sides, compare.
A pass covers only that claim and dimension. Fallback: `diff`, replayed
inputs with recorded outputs. Gap: sampled inputs do not prove equivalence.

## 4. trace_crash

Objective: tie a crash symptom to static call and reference evidence.
Arguments: the crash signal (required); target path, procedure, optional
capture evidence id.
Start: `inspect_recorded_crash` for a Linux x86-64 ELF core file (other
cores use the lldb/gdb fallback), then xrefs and function analysis around
the faulting address.
Fallback: `lldb -c core exe -b -o 'bt'`, `gdb -batch -ex bt exe core`.
Gap: needs the matching binary and symbols; without them frames are raw
addresses.

## 5. audit_residual_unknowns

Objective: review the Unknown registry, close what evidence supports, and
list what is still open.
Arguments: the audit scope (required); optional unknown or evidence id.
Start: read `unknowns.jsonl`; for each open item check its evidence ids,
required authority and confidence, and its probes; resolve only with
evidence; mark `blocked` with the missing prerequisite; downgrade nothing
silently. Output: per-unknown status change with evidence ids.

## 6. prepare_bounded_process_capture

Objective: plan a recorded run of a target to answer a behaviour question.
Arguments: the behaviour question, the executable and working directory
(required); optional prior capture evidence id.
Start: write the plan for the user: command lookup, effective working
directory, inherited environment, scenario actions, stop condition,
finalization, and that the target runs with the user's permissions. Run
it when the user's request declared that run; otherwise show the plan and
wait for confirmation. Rules are in `runtime-and-engine.md`.
Fallback: `dtruss`/`strace`/`lsof` if the user has them and approves.
Gap: a capture shows one run, not every path.
