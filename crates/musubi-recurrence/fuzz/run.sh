#!/bin/sh
# Fuzz `musubi_recurrence::expand` for $1 seconds (default 600) and fail if AFL saved a crash.
# Needs cargo-afl: `cargo install cargo-afl --locked --version 0.18.2 && cargo afl config --build`
# (on LLVM newer than its AFL++ supports, `cargo afl config --update` instead).
set -eu
cd "$(dirname "$0")"
cargo afl build --release --locked
rm -rf out
AFL_NO_UI=1 AFL_SKIP_CPUFREQ=1 AFL_I_DONT_CARE_ABOUT_MISSING_CRASHES=1 \
    cargo afl fuzz -V "${1:-600}" -i in -o out target/release/musubi-recurrence-fuzz
grep -E 'execs_done|saved_crashes|saved_hangs|bitmap_cvg' out/default/fuzzer_stats
# Hangs are inputs slower than AFL's timeout; `expand` is bounded by MAX_GENERATED, so they
# are expensive but finite. Only crashes (panics, failed invariants) fail the run.
# Each crash is printed (base64 and replay) so the log alone reproduces it, and copied to
# `crashes/` under a name without AFL's colons, which artifact upload rejects.
rm -rf crashes
n=0
for f in out/default/crashes/id:*; do
    [ -e "$f" ] || continue
    n=$((n + 1))
    mkdir -p crashes
    cp "$f" "crashes/crash-$n"
    echo "== crash-$n ($f)" >&2
    base64 -w0 "$f" >&2
    echo >&2
    target/release/musubi-recurrence-fuzz < "$f" 2>&1 | grep -v '^$' | head -20 >&2 || true
done
if [ "$n" -gt 0 ]; then
    echo "$n crashes; replay: echo BASE64 | base64 -d | target/release/musubi-recurrence-fuzz" >&2
    exit 1
fi
