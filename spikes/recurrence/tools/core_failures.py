"""Splits each engine's FAIL/ERR cases into the policy/tz layer (owned by our wrapper) and the core.

  python3 tools/core_failures.py      # reads results/*.json via compare.py
"""
import pathlib, sys
sys.path.insert(0, str(pathlib.Path(__file__).parent.parent))
from compare import load, outcome, verdict

# Cases whose failure the report assigns to the wrapper (DTSTART rule, EXDATE/EXRULE/RDATE policy, TZID resolution).
POLICY_TZ = {"until-before-dtstart", "dtstart-not-in-rule", "rdate-only", "rdate-period", "exdate-date-on-timed",
             "exrule", "vtimezone-custom-rules", "vtimezone-windows-name", "vtimezone-mozilla-prefix"}
cases, expected, _, engines = load()
for n, d in engines.items():
    bad = [c["id"] for c in cases if verdict(expected.get(c["id"]), outcome(c, d["results"].get(c["id"]))) in ("FAIL", "ERR")]
    core = [b for b in bad if b not in POLICY_TZ]
    print(f"{n}: {len(bad)} failing, policy/tz {len(bad) - len(core)}, core {len(core)} {core}")
