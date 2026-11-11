# calcard-fidelity (Phase 0 spike)

A harness that answers whether `calcard` 0.3.14 can serve as musubi-sync's iCalendar/vCard/JSCalendar/JSContact layer.
The report is at [`docs/research/phase0/calcard.md`](../../docs/research/phase0/calcard.md).

```sh
cd spikes/calcard-fidelity
cargo build --release          # Rust 1.99; deps: calcard =0.3.14, chrono-tz, serde_json
cargo test --release           # self-check of the raw-patch prototype
cargo fetch                    # makes calcard's own fixtures available in the registry
R=$(ls -d ~/.cargo/registry/src/*/calcard-0.3.14/resources)

# parse / round-trip / patch / JS* conversion; writes results/<corpus>.tsv and .lost.txt
./target/release/calcard-fidelity corpus corpus/musubi corpus/synth $R/ical $R/vcard > results/summary.txt
# recurrence expansion through calcard's expand_dates
./target/release/calcard-fidelity expand corpus/synth/*.ics corpus/musubi/*.ics > results/expand.txt
# throughput on 50k generated events / cards
./target/release/calcard-fidelity bench 50000 > results/bench.txt
# helpers for inspecting one file
./target/release/calcard-fidelity rt corpus/synth/icloud_contact_items.vcf      # calcard's re-serialisation
./target/release/calcard-fidelity js corpus/synth/google_contacts_export.vcf    # X -> JSContact JSON -> X
```

- `src/main.rs`: the measurements.
- `src/rawpatch.rs`: a prototype raw-patch layer. It aligns raw logical lines with calcard entries and replaces only the target line, or only its value.
- `tools/extract_musubi.py`: extracts the Musubi corpus.
- `corpus/SOURCES.md`: where the corpus files come from.
