| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| pg | ingest solo large tenant page=250 | 411 | 10.044 | 17.118 | 30.733 | 23459 items/s |
| pg | ingest solo large tenant page=2500 | 49 | 63.362 | 102.092 | 102.092 | 35415 items/s (n < 100: p99 is the max, not a stable percentile) |
| pg | ingest concurrent page=250 | 3008 | 11.857 | 36.704 | 54.309 | 100 tenants x 16 writers: 206042 items/s aggregate |
| pg | ingest concurrent page=2500 | 2000 | 11.030 | 334.784 | 788.830 | 100 tenants x 16 writers: 95422 items/s aggregate |
| pg | ingest solo regular tenants page=250 | 240 | 4.188 | 7.259 | 7.742 | 39356 items/s |
| pg | ingest solo regular tenants page=2500 | 160 | 3.884 | 37.530 | 49.235 | 36000 items/s |
<!-- pg VACUUM ANALYZE: 1.9 s -->
| pg | incremental sync page=25 (txn only) | 3000 | 1.227 | 2.677 | 12.068 | writer connection already open |
| pg | disk size | - | - | - | - | database 4.95 GB (excl. WAL); items 1930 MB, raw 1494 MB, occurrences 945 MB, placements 237 MB, entities 170 MB, changes 164 MB, collections 1 MB, tenants 0 MB, mutations 0 MB, outbox 0 MB |
| pg | range regular week 5 coll | 2000 | 0.260 | 0.687 | 0.947 | avg 35 rows |
| pg | range regular week 20 coll | 2000 | 0.884 | 1.530 | 2.086 | avg 140 rows |
| pg | range regular month 5 coll | 2000 | 0.581 | 1.563 | 2.355 | avg 150 rows |
| pg | range regular month 20 coll | 2000 | 2.428 | 3.390 | 8.525 | avg 591 rows |
| pg | range regular month 20 coll ids-only | 2000 | 0.513 | 0.779 | 0.971 | avg 590 rows |
| pg | range large week 5 coll | 2000 | 2.625 | 7.027 | 8.512 | avg 705 rows |
| pg | range large week 20 coll | 2000 | 15.905 | 40.241 | 83.961 | avg 2783 rows |
| pg | range large month 5 coll | 2000 | 10.056 | 35.290 | 79.612 | avg 2920 rows |
| pg | range large month 20 coll | 2000 | 52.826 | 66.168 | 182.577 | avg 11761 rows |
| pg | range large month 20 coll ids-only | 2000 | 7.692 | 9.871 | 11.987 | avg 11759 rows |
| pg | changes regular since head-100 limit 500 | 2000 | 0.349 | 0.628 | 1.028 | avg 60 rows |
| pg | changes regular since random limit 500 | 2000 | 2.126 | 2.492 | 2.743 | avg 456 rows |
| pg | changes large since head-100 limit 500 | 2000 | 0.394 | 0.636 | 1.077 | avg 100 rows |
| pg | changes large since random limit 500 | 2000 | 2.101 | 2.430 | 6.773 | avg 498 rows |
| pg | range regular month 20 coll, 8 reader connections | 16000 | 2.603 | 3.561 | 5.442 | 3009 queries/s |
| pg | mixed read (month range + changes) baseline, no writers | 61958 | 2.521 | 3.497 | 6.909 | 8 readers, same 8 tenants |
| pg | mixed read (month range + changes) with 8 writers | 34435 | 4.220 | 11.225 | 276.495 | 8 readers, same 8 tenants |
| pg | mixed write page=250 updates | 18629 | 7.767 | 33.638 | 282.412 | 155508 items/s while reading |
| pg | outbox accept, synchronous_commit=on, 1 writer | 2000 | 1.159 | 8.090 | 14.417 | mutation+entity rev+journal+outbox in one fsync'd txn |
| pg | outbox accept, synchronous_commit=on, 16 tenants concurrently | 4800 | 2.125 | 2.961 | 11.816 |  |
| pg | new connection + prepare + first query | 200 | 3.865 | 5.706 | 5.875 | what a pool saves; compare with SQLite cold open |
