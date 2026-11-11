<!-- SQLITE_CKPT=bg -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | ingest solo large tenant page=250 | 411 | 8.301 | 82.654 | 91.798 | 19687 items/s; ckpt=bg: max WAL 81.9 MB, 48 passive, 38 truncate (12 busy: readers past 100 ms or another checkpoint running) |
| sqlite | ingest solo large tenant page=2500 | 49 | 83.284 | 215.051 | 215.051 | 23777 items/s; ckpt=bg: max WAL 118.9 MB, 24 passive, 17 truncate (3 busy: readers past 100 ms or another checkpoint running) (n < 100: p99 is the max, not a stable percentile) |
<!-- SQLITE_CKPT=inline -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | ingest solo large tenant page=250 | 411 | 17.717 | 24.585 | 29.666 | 14215 items/s; ckpt=inline (wal_autocheckpoint=1000) |
| sqlite | ingest solo large tenant page=2500 | 49 | 97.088 | 165.178 | 165.178 | 20691 items/s; ckpt=inline (wal_autocheckpoint=1000) (n < 100: p99 is the max, not a stable percentile) |
<!-- SQLITE_CKPT=passive -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | ingest solo large tenant page=250 | 411 | 8.810 | 17.305 | 20.444 | 26601 items/s; ckpt=passive-50ms: max WAL 1849.1 MB, 0 passive, 0 truncate (0 busy: readers past 100 ms or another checkpoint running) |
| sqlite | ingest solo large tenant page=2500 | 49 | 67.633 | 109.957 | 109.957 | 29745 items/s; ckpt=passive-50ms: max WAL 1272.9 MB, 0 passive, 0 truncate (0 busy: readers past 100 ms or another checkpoint running) (n < 100: p99 is the max, not a stable percentile) |
<!-- SQLITE_WAL_HARD_MB=256 -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | ingest solo large tenant page=250 | 411 | 8.264 | 73.646 | 118.728 | 24793 items/s; ckpt=bg: max WAL 280.8 MB, 46 passive, 13 truncate (7 busy: readers past 100 ms or another checkpoint running) |
| sqlite | ingest solo large tenant page=2500 | 49 | 67.409 | 184.112 | 184.112 | 27689 items/s; ckpt=bg: max WAL 302.8 MB, 32 passive, 5 truncate (1 busy: readers past 100 ms or another checkpoint running) (n < 100: p99 is the max, not a stable percentile) |
