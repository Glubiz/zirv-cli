# Evidence and unknowns ledger

Two JSONL files, append-only, in the ledger directory chosen in SKILL.md:
`evidence.jsonl` and `unknowns.jsonl`. REA keeps its own Evidence and
unknown records and validates and exports bundles with `evidence-import` /
`evidence-export`. A ledger you write yourself mirrors those fields for
zirv-local use only: its ids are not REA's canonical digests, so it is NOT
importable into REA, and you must never report it as imported.

## Evidence record

One line per observation:

    {"evidence_id":"ev_<64 hex sha256 of the record body>",
     "subject":{"name":"app.exe","sha256":"...","local_path":"/abs/app.exe","format":"pe","architecture":"x86_64"},
     "provider":{"id":"objdump","name":"objdump","version":"<tool --version>"},
     "operation":"disassemble","parameters":{"command":"objdump -d app.exe"},
     "raw_result_file":"out/0007.txt","raw_result_sha256":"...",
     "normalized_result":{"...":"..."},
     "confidence":"observed","authority":"shipped-artifact",
     "environment":null,
     "limitations":["no symbols","stripped"],
     "locations":[{"kind":"address","address":"0x1000"}],
     "evidence_links":["ev_..."]}

- `evidence_id` is content-addressed: the same observation yields the same
  id; never reuse an id for different content.
- `locations` kinds: `address`, `address-range`, `artifact-path`,
  `file-offset`, `file-offset-range`.
- `environment` is null for static reads; for runs record platform,
  architecture and isolation (`none`, `process`, `container`,
  `virtual-machine`).
- `limitations` is mandatory when coverage is partial, a preview was
  truncated, or a feature was unsupported.

## Vocabulary

Confidence:

- `observed`: the tool reported it directly.
- `derived`: computed from observed records (cite them in `evidence_links`).
- `inferred`: your reasoning; name competing explanations.

Authority: `shipped-artifact` (the target's own bytes), `controlled-replay`
(a run you set up), `historical-reference` (older source or docs),
`external-service` (a lookup elsewhere), `analyst-inference`.

A claim is only as strong as the weakest link it rests on.

## Unknown record

    {"unknown_id":"unk_<64 hex from question + domain + scope>",
     "question":"Does the updater verify signatures?",
     "domain":"updater","scope_digest":null,
     "status":"open","severity":"high",
     "supporting_evidence_ids":[],"contradicting_evidence_ids":[],
     "required_authority":"controlled-replay","required_confidence":"observed",
     "required_environment":{"id":null,"platform":null,"architecture":null,"isolation":"process"},
     "recommended_probes":[{"operation":"capture_process_scenario","rationale":"static code is obfuscated"}],
     "relationships":[{"type":"depends-on","unknown_id":"unk_..."}],
     "resolution":null,"revision":1}

A verified resolution (status `resolved`):

    "resolution":{"disposition":"verified","rationale":"capture shows the check",
                  "evidence_ids":["ev_..."]}

- Status: `open`, `investigating`, `blocked`, `contradicted`, `resolved`.
- Severity: `low`, `medium`, `high`, `critical`.
- Relationship types: `depends-on`, `related-to`, `contradicts`; an
  unknown never relates to itself.
- `contradicted` requires at least one contradicting evidence id.
- `resolved` carries a resolution: `verified` (needs at least one evidence
  id), `withdrawn`, or `out-of-scope`, each with a `rationale`. Other statuses carry
  none.
- Update with the expected revision; if it moved, re-read before writing.

## Comparison and verification

- Equivalence or absence needs complete, compatible page sets on both
  sides. An incomplete inventory, an unsupported format or conflicting
  evidence leaves the answer Unknown.
- Static differences are candidates for a behavioural difference, not
  proof of cause.
- A reconstruction check that passes covers only the declared claim and
  dimension (for example "exports match"), nothing broader.
- If the target hash changes mid-investigation, record `artifact_changed`,
  re-hash, and keep the old evidence marked as describing the old digest;
never apply it to the new bytes.
