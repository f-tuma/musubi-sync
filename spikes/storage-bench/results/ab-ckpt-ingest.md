<!-- SQLITE_CKPT=bg -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | ingest solo large tenant page=250 | 411 | 8.031 | 188.501 | 224.518 | 16383 items/s; ckpt=bg: max WAL 109.0 MB, 35 passive, 24 truncate (max 216 ms; 4 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | ingest solo large tenant page=2500 | 49 | 83.773 | 347.190 | 347.190 | 19245 items/s; ckpt=bg: max WAL 131.1 MB, 22 passive, 15 truncate (max 327 ms; 2 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) (n < 100: p99 is the max, not a stable percentile) |
<!-- SQLITE_CKPT=inline -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | ingest solo large tenant page=250 | 411 | 20.892 | 36.463 | 38.592 | 11770 items/s; ckpt=inline (wal_autocheckpoint=1000): max WAL 8.1 MB |
| sqlite | ingest solo large tenant page=2500 | 49 | 106.423 | 188.346 | 188.346 | 18375 items/s; ckpt=inline (wal_autocheckpoint=1000): max WAL 38.3 MB (n < 100: p99 is the max, not a stable percentile) |
<!-- SQLITE_CKPT=passive -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | ingest solo large tenant page=250 | 411 | 8.442 | 17.031 | 20.267 | 27903 items/s; ckpt=passive-50ms: max WAL 1861.2 MB, 48 passive, 0 truncate (max 0 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | ingest solo large tenant page=2500 | 49 | 65.751 | 118.192 | 118.192 | 29359 items/s; ckpt=passive-50ms: max WAL 1249.0 MB, 32 passive, 0 truncate (max 0 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) (n < 100: p99 is the max, not a stable percentile) |
<!-- SQLITE_WAL_HARD_MB=256 -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | ingest solo large tenant page=250 | 411 | 8.292 | 77.351 | 125.105 | 24695 items/s; ckpt=bg: max WAL 273.0 MB, 46 passive, 6 truncate (max 27 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | ingest solo large tenant page=2500 | 49 | 70.324 | 162.376 | 162.376 | 28009 items/s; ckpt=bg: max WAL 312.6 MB, 33 passive, 3 truncate (max 23 ms; 2 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) (n < 100: p99 is the max, not a stable percentile) |
