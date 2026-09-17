## Memory
- Key: typesafe-jev-api-facts
- Written-by: claude
- Written: 1789634742
- Verified: 1789634742
- Source: explicit

Verified 2026-09-17 from docs.typesafe.ai (not runtime-probed; no key on the dev machine). TypeSafe Jev is a decision-only model: POST https://api.typesafe.ai/v1/systemone, Authorization: Bearer <key from TYPESAFE_API_KEY>, body {state: string|object|array, model: "jev-latest", questions: {id: {type: choice|score|noul, instructions, criteria}}}; choice criteria = map option->description (<=255), score criteria = ordered array >=2 levels, noul optional {true,false} boundaries. Response {model, an
[truncated]
