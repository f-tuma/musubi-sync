#!/usr/bin/env bash
# Rebuilds the corpus, runs both oracles and every Rust candidate, and compares
# them against corpus/expected.json. Usage: ./run.sh [--bench]
set -euo pipefail
cd "$(dirname "$0")"
[ -x .venv/bin/python ] || { python3 -m venv .venv && .venv/bin/pip install -q -r requirements.txt; }
[ -d oracles/icaljs/node_modules ] || (cd oracles/icaljs && npm ci --silent)
export TZ=Europe/Prague   # a non-UTC host zone exposes engines that leak it into floating or zoned times
mkdir -p results
.venv/bin/python corpus/build_corpus.py
.venv/bin/python oracles/gen_vtimezones.py corpus/cases.json oracles/icaljs/vtimezones.json
.venv/bin/python oracles/dateutil_oracle.py corpus/cases.json > results/dateutil.json
node oracles/icaljs/oracle.mjs corpus/cases.json > results/icaljs.json
cargo build -q --release --manifest-path candidates/Cargo.toml
BIN=candidates/target/release/rrule-candidates
for c in rrule rrule2 rrule-jiff rrule+edge rrule-jiff+edge calcard calcard-next; do "$BIN" run "$c" corpus/cases.json > "results/$c.json"; done
"$BIN" tzcheck > results/tzcheck.json
.venv/bin/python compare.py
.venv/bin/python tools/core_failures.py
python3 tools/core_size.py > results/core_size.json
if [ "${1:-}" = "--bench" ]; then .venv/bin/python bench.py 10000 5 > results/bench.json; fi
