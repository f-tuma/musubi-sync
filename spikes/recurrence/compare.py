"""Compares every results/<engine>.json against corpus/expected.json.

  python3 compare.py                 # table to stdout, details to results/report.md
  python3 compare.py --seed          # (re)write expected.json entries where both
                                     # oracles agree; keeps hand-decided entries
expected.json: {case_id: {"accept": [[starts...], ...], "error_ok": bool,
                          "decided_by": "oracles|rfc|policy", "why": "..."}}
"""
import json, pathlib, sys

ROOT = pathlib.Path(__file__).parent
ORACLES = ["dateutil", "icaljs"]


def key(s):
    s = s.replace("-", "").replace(":", "").rstrip("Z")
    return s + "T000000" if len(s) == 8 else s


def clip(case, starts):
    starts = sorted(set(starts))
    if case["window"]:
        lo, hi = map(key, case["window"])
        starts = [s for s in starts if lo <= key(s) < hi]
    if case["limit"]:
        starts = starts[: case["limit"]]
    return starts


def load():
    cases = json.loads((ROOT / "corpus/cases.json").read_text())
    exp_path = ROOT / "corpus/expected.json"
    expected = json.loads(exp_path.read_text()) if exp_path.exists() else {}
    engines = {}
    for p in sorted((ROOT / "results").glob("*.json")):
        d = json.loads(p.read_text())
        if isinstance(d, dict) and "results" in d:  # skip bench.json / tzcheck.json
            engines[p.stem] = d
    return cases, expected, exp_path, engines


def outcome(case, r):
    if r is None:
        return ("missing", None)
    if "error" in r:
        return ("error", r["error"])
    return ("ok", clip(case, r["ok"]))


def verdict(exp, out):
    if exp is None:
        return "?"
    kind, val = out
    if kind == "ok":
        return "pass" if val in exp["accept"] else "FAIL"
    if kind == "error":
        return "pass" if exp.get("error_ok") else "ERR"
    return "-"


def main():
    cases, expected, exp_path, engines = load()
    if "--seed" in sys.argv:
        for c in cases:
            outs = [outcome(c, engines[o]["results"].get(c["id"])) for o in ORACLES if o in engines]
            if c["id"] not in expected and all(k == "ok" for k, _ in outs) and all(v == outs[0][1] for _, v in outs):
                expected[c["id"]] = {"accept": [outs[0][1]], "error_ok": False, "decided_by": "oracles", "why": "dateutil and ical.js agree"}
        exp_path.write_text(json.dumps(expected, indent=1, sort_keys=True) + "\n")
        print(f"{len(expected)}/{len(cases)} cases have expectations")
        return
    names = list(engines)
    rows, details, totals = [], [], {n: {"pass": 0, "FAIL": 0, "ERR": 0, "?": 0, "-": 0} for n in names}
    for c in cases:
        exp = expected.get(c["id"])
        cells = []
        for n in names:
            out = outcome(c, engines[n]["results"].get(c["id"]))
            v = verdict(exp, out)
            totals[n][v] += 1
            cells.append(v)
            if v in ("FAIL", "ERR"):
                details.append(f"- **{c['id']}** / `{n}`: got `{out[1]}`; accepted `{exp['accept']}`")
        rows.append(f"| {c['id']} | {' | '.join(cells)} |")
    head = f"| case | {' | '.join(names)} |\n|---|{'---|' * len(names)}"
    summary = "| total | " + " | ".join(f"{t['pass']} pass / {t['FAIL']} fail / {t['ERR']} err" for t in totals.values()) + " |"
    engines_line = "\n".join(f"- `{n}`: {engines[n].get('engine', '')}" for n in names)
    text = f"{engines_line}\n\n{head}\n" + "\n".join(rows) + f"\n{summary}\n\n### Failures\n\n" + "\n".join(details) + "\n"
    (ROOT / "results/report.md").write_text(text)
    print(head + "\n" + "\n".join(rows) + "\n" + summary)


if __name__ == "__main__":
    main()
