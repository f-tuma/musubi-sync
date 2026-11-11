"""Benchmark: expand a window of YEARS for N synthetic series per candidate, 3 runs each.

  python3 bench.py [N] [YEARS] > results/bench.json
Peak RSS comes from wait4() rusage of each child process (no /usr/bin/time needed).
`parse-only` is the baseline: same binary and inputs, no expansion.
"""
import json, os, pathlib, subprocess, sys

ROOT = pathlib.Path(__file__).parent
BIN = ROOT / "candidates/target/release/rrule-candidates"
N, YEARS = (sys.argv[1:3] + ["10000", "5"][len(sys.argv[1:3]):])
rows = []
for cand in ["parse-only", "rrule", "rrule2", "rrule-jiff", "calcard", "calcard-next"]:
    for run in range(3):
        p = subprocess.Popen([BIN, "bench", cand, N, YEARS], stdout=subprocess.PIPE,
                             env={**os.environ, "TZ": "Europe/Prague"})
        out = p.stdout.read()
        _, status, ru = os.wait4(p.pid, 0)
        assert status == 0, (cand, status)
        rows.append({**json.loads(out), "run": run, "peak_rss_mb": round(ru.ru_maxrss / 1024, 1),
                     "cpu_s": round(ru.ru_utime + ru.ru_stime, 2)})
        print(json.dumps(rows[-1]), file=sys.stderr)
print(json.dumps(rows, indent=1))
