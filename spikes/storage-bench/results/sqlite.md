| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | ingest solo large tenant page=250 | 411 | 8.796 | 18.594 | 25.212 | 26612 items/s |
| sqlite | ingest solo large tenant page=2500 | 49 | 75.597 | 114.649 | 114.649 | 28211 items/s |
| sqlite | ingest concurrent page=250 | 3008 | 11.787 | 25.241 | 36.380 | 100 tenants x 16 writers: 194293 items/s aggregate |
| sqlite | ingest concurrent page=2500 | 2000 | 13.444 | 47.537 | 53.822 | 100 tenants x 16 writers: 217186 items/s aggregate |
| sqlite | ingest solo regular tenants page=250 | 240 | 3.186 | 9.810 | 10.417 | 45544 items/s |
| sqlite | ingest solo regular tenants page=2500 | 160 | 3.379 | 16.566 | 16.947 | 48634 items/s |
<!-- sqlite ANALYZE all tenants: 2.1 s -->
| sqlite | incremental sync page=25 (txn only) | 300 | 1.507 | 3.007 | 3.154 | writer connection already open |
| sqlite | disk size | - | - | - | - | all tenant files 5.40 GB; large tenant (100k) 433 MB; regular tenant (5k) 20.9 MB |
| sqlite | range regular week 5 coll | 2000 | 0.132 | 0.355 | 0.457 | avg 35 rows |
| sqlite | range regular week 20 coll | 2000 | 0.362 | 0.460 | 0.581 | avg 140 rows |
| sqlite | range regular month 5 coll | 2000 | 0.221 | 0.563 | 6.705 | avg 150 rows |
| sqlite | range regular month 20 coll | 2000 | 0.864 | 1.066 | 1.172 | avg 591 rows |
| sqlite | range regular month 20 coll ids-only | 2000 | 0.344 | 0.406 | 0.449 | avg 590 rows |
| sqlite | range large week 5 coll | 2000 | 1.467 | 4.297 | 6.058 | avg 705 rows |
| sqlite | range large week 20 coll | 2000 | 7.094 | 9.192 | 12.137 | avg 2783 rows |
| sqlite | range large month 5 coll | 2000 | 4.656 | 14.523 | 41.087 | avg 2920 rows |
| sqlite | range large month 20 coll | 2000 | 23.107 | 28.637 | 54.493 | avg 11761 rows |
| sqlite | range large month 20 coll ids-only | 2000 | 6.808 | 8.219 | 9.078 | avg 11759 rows |
| sqlite | changes regular since head-100 limit 500 | 2000 | 0.103 | 0.324 | 0.778 | avg 92 rows |
| sqlite | changes regular since random limit 500 | 2000 | 0.404 | 0.839 | 1.201 | avg 474 rows |
| sqlite | changes large since head-100 limit 500 | 2000 | 0.044 | 0.048 | 4.897 | avg 100 rows |
| sqlite | changes large since random limit 500 | 2000 | 0.454 | 5.541 | 12.046 | avg 498 rows |
| sqlite | range regular month 20 coll, 8 reader threads | 16000 | 0.846 | 1.506 | 4.448 | 7436 queries/s |
| sqlite | mixed read (month range + changes) baseline, no writers | 194576 | 0.824 | 1.056 | 3.785 | 8 readers, same 8 tenants |
| sqlite | mixed read (month range + changes) with 8 writers | 60923 | 2.430 | 6.166 | 38.607 | 8 readers, same 8 tenants |
| sqlite | mixed write page=250 updates | 47219 | 3.156 | 10.573 | 39.148 | 393668 items/s while reading |
| sqlite | outbox accept, synchronous=FULL, 1 writer | 2000 | 1.068 | 4.744 | 24.842 | mutation+entity rev+journal+outbox in one fsync'd txn |
| sqlite | outbox accept, synchronous=FULL, 16 tenants concurrently | 4800 | 2.748 | 12.179 | 20.541 |  |
<!-- created 10000 small tenant files in 19.2 s, 2705 MB -->
| sqlite | open + pragmas + first query, cold (page cache dropped) | 10000 | 0.522 | 0.961 | 2.864 | 10000 tenants in 6.3 s |
| sqlite | close, cold (page cache dropped) | 10000 | 0.046 | 0.085 | 0.236 |  |
| sqlite | open + pragmas + first query, warm page cache | 10000 | 0.138 | 0.220 | 0.416 | 10000 tenants in 1.8 s |
| sqlite | close, warm page cache | 10000 | 0.037 | 0.066 | 0.181 |  |
| sqlite | request via LRU of open DBs (open on miss + query) | 100000 | 0.013 | 0.328 | 3.402 | LRU 1000 of 10000, Zipf s=1.1, hit rate 77.8 %, RSS +164 MB with 1000 open |
| sqlite | schema migration at open (ADD COLUMN) | 10000 | 2.120 | 13.804 | 81.192 | 10000 tenant files |
