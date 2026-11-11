| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | ingest solo large tenant page=250 | 411 | 7.880 | 220.902 | 362.452 | 15712 items/s; ckpt=bg: max WAL 108.7 MB, 34 passive, 25 truncate (max 355 ms; 3 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | ingest solo large tenant page=2500 | 49 | 79.951 | 436.401 | 436.401 | 20756 items/s; ckpt=bg: max WAL 184.1 MB, 21 passive, 14 truncate (max 342 ms; 3 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) (n < 100: p99 is the max, not a stable percentile) |
| sqlite | ingest concurrent page=250 | 3008 | 12.170 | 27.464 | 46.008 | 100 tenants x 16 writers: 188986 items/s aggregate; ckpt=bg: max WAL 60.5 MB, 506 passive, 0 truncate (max 0 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | ingest concurrent page=2500 | 2000 | 13.446 | 46.224 | 50.962 | 100 tenants x 16 writers: 213272 items/s aggregate; ckpt=bg: max WAL 51.1 MB, 404 passive, 0 truncate (max 0 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | ingest solo regular tenants page=250 | 240 | 3.032 | 8.909 | 9.487 | 47753 items/s; ckpt=bg: max WAL 60.4 MB, 64 passive, 0 truncate (max 0 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | ingest solo regular tenants page=2500 | 160 | 3.272 | 15.845 | 16.102 | 52216 items/s; ckpt=bg: max WAL 50.9 MB, 60 passive, 0 truncate (max 0 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
<!-- sqlite ANALYZE all tenants: 1.9 s -->
| sqlite | incremental sync page=25 (txn only) | 3000 | 1.051 | 4.004 | 12.069 | writer connection already open; ckpt=bg: max WAL 7.3 MB, 1119 passive, 0 truncate (max 0 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | disk size | - | - | - | - | all tenant files 5.58 GB; large tenant (100k) 433 MB; regular tenant (5k) 22.1 MB |
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | range regular week 5 coll | 2000 | 0.134 | 0.395 | 0.564 | avg 35 rows |
| sqlite | range regular week 20 coll | 2000 | 0.362 | 0.500 | 0.624 | avg 140 rows |
| sqlite | range regular month 5 coll | 2000 | 0.210 | 0.538 | 0.646 | avg 150 rows |
| sqlite | range regular month 20 coll | 2000 | 0.825 | 1.001 | 1.155 | avg 591 rows |
| sqlite | range regular month 20 coll ids-only | 2000 | 0.337 | 0.398 | 0.451 | avg 590 rows |
| sqlite | range large week 5 coll | 2000 | 1.243 | 3.439 | 3.986 | avg 705 rows |
| sqlite | range large week 20 coll | 2000 | 6.444 | 8.630 | 9.427 | avg 2783 rows |
| sqlite | range large month 5 coll | 2000 | 4.243 | 13.375 | 17.420 | avg 2920 rows |
| sqlite | range large month 20 coll | 2000 | 24.010 | 28.142 | 30.216 | avg 11761 rows |
| sqlite | range large month 20 coll ids-only | 2000 | 5.832 | 6.796 | 7.905 | avg 11759 rows |
| sqlite | changes regular since head-100 limit 500 | 2000 | 0.066 | 0.111 | 0.151 | avg 60 rows |
| sqlite | changes regular since random limit 500 | 2000 | 0.395 | 0.433 | 0.484 | avg 456 rows |
| sqlite | changes large since head-100 limit 500 | 2000 | 0.045 | 0.047 | 0.163 | avg 100 rows |
| sqlite | changes large since random limit 500 | 2000 | 0.429 | 0.497 | 0.531 | avg 498 rows |
| sqlite | range regular month 20 coll, 8 reader threads | 16000 | 0.872 | 1.988 | 45.368 | 6369 queries/s |
| sqlite | mixed read (month range + changes) baseline, no writers | 205340 | 0.785 | 0.986 | 8.058 | 8 readers, same 8 tenants; ckpt=bg: max WAL 0.0 MB, 0 passive, 0 truncate (max 0 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | mixed read (month range + changes) with 8 writers | 82844 | 1.908 | 3.664 | 11.794 | 8 readers, same 8 tenants; ckpt=bg: max WAL 134.6 MB, 1181 passive, 321 truncate (max 797 ms; 42 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | mixed write page=250 updates | 45257 | 2.783 | 15.893 | 800.704 | 377361 items/s while reading |
| sqlite | outbox accept, synchronous=FULL, 1 writer | 2000 | 1.022 | 1.742 | 10.424 | mutation+entity rev+journal+outbox in one fsync'd txn |
| sqlite | outbox accept, synchronous=FULL, 16 tenants concurrently | 4800 | 2.109 | 2.885 | 3.814 | ckpt=bg: max WAL 79.7 MB, 81 passive, 0 truncate (max 0 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
<!-- created 10000 small tenant files in 16.0 s, 2705 MB -->
| sqlite | open + pragmas + first query, cold (page cache dropped) | 10000 | 0.554 | 0.943 | 3.408 | 10000 tenants in 6.5 s |
| sqlite | close, cold (page cache dropped) | 10000 | 0.050 | 0.097 | 0.172 |  |
| sqlite | open + pragmas + first query, warm page cache | 10000 | 0.136 | 0.197 | 0.413 | 10000 tenants in 1.8 s |
| sqlite | close, warm page cache | 10000 | 0.036 | 0.055 | 0.205 |  |
| sqlite | request via LRU of open DBs (open on miss + query) | 100000 | 0.010 | 0.282 | 0.844 | LRU 1000 of 10000, Zipf s=1.1, hit rate 77.8 %, RSS +164 MB with 1000 open |
| sqlite | schema migration at open (ADD COLUMN) | 10000 | 1.998 | 8.320 | 40.029 | 10000 tenant files |
