import glob, json, os, sys
root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "runs")
rows = []
for f in sorted(glob.glob(os.path.join(root, "*", "result.json"))):
    r = json.load(open(f, encoding="utf-8"))
    z = r.get("zirv_cmds") or {}
    p = r.get("proxy") or {}
    rows.append("{:<15}{:<11}r{} {:<7}score {:<6}cost {:<7}wall {:<5}turns {:<4}tools {:<4}zw{} zs{} za{} sub{} den{} err{} {} | {}".format(
        r["task"], r["cond"], r["rep"], str(r.get("model_used")), r["score"],
        "-" if r["total_cost_usd"] is None else round(r["total_cost_usd"], 3),
        round(r["wall_s"]), r.get("num_turns"), r.get("tool_calls"),
        z.get("workflow", 0), z.get("skill", 0), z.get("agent", 0),
        r.get("subagents_spawned"), r.get("permission_denials"), int(bool(r["is_error"])),
        (p.get("complexity") or "") + ("/" + p["seat_tier"] if p.get("seat_tier") else "") + ("/wf=" + str(p.get("workflow")) if p else ""),
        (r.get("details") or "")[:70]))
print("\n".join(rows)); print(len(rows), "results")
