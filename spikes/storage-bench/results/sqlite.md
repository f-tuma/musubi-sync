| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | ingest solo large tenant page=250 | 411 | 8.295 | 135.168 | 205.942 | 16006 items/s; ckpt=bg: max WAL 80.7 MB, 46 passive, 37 truncate (11 busy: readers past 100 ms or another checkpoint running) |
| sqlite | ingest solo large tenant page=2500 | 49 | 74.681 | 414.076 | 414.076 | 21303 items/s; ckpt=bg: max WAL 151.1 MB, 22 passive, 22 truncate (11 busy: readers past 100 ms or another checkpoint running) (n < 100: p99 is the max, not a stable percentile) |
| sqlite | ingest concurrent page=250 | 3008 | 11.984 | 27.712 | 65.381 | 100 tenants x 16 writers: 189277 items/s aggregate; ckpt=bg: max WAL 60.3 MB, 445 passive, 0 truncate (0 busy: readers past 100 ms or another checkpoint running) |
| sqlite | ingest concurrent page=2500 | 2000 | 12.778 | 46.179 | 50.953 | 100 tenants x 16 writers: 218746 items/s aggregate; ckpt=bg: max WAL 51.1 MB, 400 passive, 0 truncate (0 busy: readers past 100 ms or another checkpoint running) |
| sqlite | ingest solo regular tenants page=250 | 240 | 3.311 | 8.563 | 8.938 | 44021 items/s; ckpt=bg: max WAL 60.4 MB, 71 passive, 0 truncate (0 busy: readers past 100 ms or another checkpoint running) |
| sqlite | ingest solo regular tenants page=2500 | 160 | 3.360 | 16.714 | 17.024 | 48869 items/s; ckpt=bg: max WAL 50.9 MB, 61 passive, 0 truncate (0 busy: readers past 100 ms or another checkpoint running) |
<!-- sqlite ANALYZE all tenants: 2.0 s -->
| sqlite | incremental sync page=25 (txn only) | 3000 | 1.065 | 3.601 | 42.122 | writer connection already open; ckpt=bg: max WAL 6.7 MB, 1152 passive, 0 truncate (0 busy: readers past 100 ms or another checkpoint running) |
| sqlite | disk size | - | - | - | - | all tenant files 5.58 GB; large tenant (100k) 433 MB; regular tenant (5k) 22.1 MB |
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | range regular week 5 coll | 2000 | 0.134 | 0.376 | 0.464 | avg 35 rows |
| sqlite | range regular week 20 coll | 2000 | 0.350 | 0.472 | 0.567 | avg 140 rows |
| sqlite | range regular month 5 coll | 2000 | 0.210 | 0.528 | 0.636 | avg 150 rows |
| sqlite | range regular month 20 coll | 2000 | 0.821 | 1.003 | 1.105 | avg 591 rows |
| sqlite | range regular month 20 coll ids-only | 2000 | 0.334 | 0.392 | 0.425 | avg 590 rows |
| sqlite | range large week 5 coll | 2000 | 1.202 | 3.363 | 4.172 | avg 705 rows |
| sqlite | range large week 20 coll | 2000 | 5.021 | 8.326 | 9.675 | avg 2783 rows |
| sqlite | range large month 5 coll | 2000 | 3.909 | 12.406 | 14.847 | avg 2920 rows |
| sqlite | range large month 20 coll | 2000 | 25.078 | 29.514 | 30.777 | avg 11761 rows |
| sqlite | range large month 20 coll ids-only | 2000 | 6.430 | 8.351 | 8.928 | avg 11759 rows |
| sqlite | changes regular since head-100 limit 500 | 2000 | 0.073 | 0.113 | 0.259 | avg 60 rows |
| sqlite | changes regular since random limit 500 | 2000 | 0.404 | 0.453 | 0.498 | avg 456 rows |
| sqlite | changes large since head-100 limit 500 | 2000 | 0.044 | 0.047 | 0.171 | avg 100 rows |
| sqlite | changes large since random limit 500 | 2000 | 0.441 | 0.519 | 0.563 | avg 498 rows |
| sqlite | range regular month 20 coll, 8 reader threads | 16000 | 0.901 | 1.973 | 9.589 | 6484 queries/s |
| sqlite | mixed read (month range + changes) baseline, no writers | 194261 | 0.822 | 1.075 | 5.906 | 8 readers, same 8 tenants; ckpt=bg: max WAL 0.0 MB, 0 passive, 0 truncate (0 busy: readers past 100 ms or another checkpoint running) |
| sqlite | mixed read (month range + changes) with 8 writers | 84830 | 1.781 | 4.234 | 10.497 | 8 readers, same 8 tenants; ckpt=bg: max WAL 88.6 MB, 922 passive, 569 truncate (315 busy: readers past 100 ms or another checkpoint running) |
| sqlite | mixed write page=250 updates | 35645 | 2.932 | 24.756 | 680.304 | 297295 items/s while reading |
| sqlite | outbox accept, synchronous=FULL, 1 writer | 2000 | 1.028 | 1.841 | 11.079 | mutation+entity rev+journal+outbox in one fsync'd txn |
| sqlite | outbox accept, synchronous=FULL, 16 tenants concurrently | 4800 | 2.710 | 8.826 | 12.554 | ckpt=bg: max WAL 66.5 MB, 92 passive, 1 truncate (0 busy: readers past 100 ms or another checkpoint running) |
<!-- created 10000 small tenant files in 16.6 s, 2705 MB -->
| sqlite | open + pragmas + first query, cold (page cache dropped) | 10000 | 0.534 | 0.857 | 5.906 | 10000 tenants in 6.3 s |
| sqlite | close, cold (page cache dropped) | 10000 | 0.047 | 0.100 | 0.192 |  |
| sqlite | open + pragmas + first query, warm page cache | 10000 | 0.137 | 0.205 | 0.448 | 10000 tenants in 1.8 s |
| sqlite | close, warm page cache | 10000 | 0.037 | 0.062 | 0.326 |  |
| sqlite | request via LRU of open DBs (open on miss + query) | 100000 | 0.012 | 0.302 | 0.970 | LRU 1000 of 10000, Zipf s=1.1, hit rate 77.8 %, RSS +163 MB with 1000 open |
| sqlite | schema migration at open (ADD COLUMN) | 10000 | 2.073 | 11.176 | 98.357 | 10000 tenant files |
