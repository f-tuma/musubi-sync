| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| pg | ingest solo large tenant page=250 | 411 | 6.843 | 9.842 | 10.674 | 34920 items/s |
| pg | ingest solo large tenant page=2500 | 49 | 66.659 | 84.614 | 84.614 | 35723 items/s |
| pg | ingest concurrent page=250 | 3008 | 11.928 | 62.594 | 109.092 | 100 tenants x 16 writers: 173206 items/s aggregate |
| pg | ingest concurrent page=2500 | 2000 | 11.823 | 174.438 | 259.204 | 100 tenants x 16 writers: 170284 items/s aggregate |
| pg | ingest solo regular tenants page=250 | 240 | 5.558 | 7.841 | 8.464 | 32151 items/s |
| pg | ingest solo regular tenants page=2500 | 160 | 5.490 | 39.489 | 40.380 | 30749 items/s |
<!-- pg VACUUM ANALYZE: 1.8 s -->
| pg | incremental sync page=25 (txn only) | 300 | 1.275 | 2.255 | 2.451 | writer connection already open |
| pg | disk size | - | - | - | - | database 4.79 GB (excl. WAL); items 1884 MB, raw 1418 MB, occurrences 915 MB, placements 238 MB, entities 169 MB, changes 160 MB, collections 1 MB, tenants 0 MB, mutations 0 MB, outbox 0 MB |
| pg | range regular week 5 coll | 2000 | 0.308 | 1.072 | 2.128 | avg 35 rows |
| pg | range regular week 20 coll | 2000 | 0.917 | 1.818 | 2.491 | avg 140 rows |
| pg | range regular month 5 coll | 2000 | 0.614 | 1.616 | 2.986 | avg 150 rows |
| pg | range regular month 20 coll | 2000 | 2.517 | 3.717 | 4.557 | avg 591 rows |
| pg | range regular month 20 coll ids-only | 2000 | 0.570 | 1.138 | 1.474 | avg 590 rows |
| pg | range large week 5 coll | 2000 | 3.211 | 8.691 | 10.223 | avg 705 rows |
| pg | range large week 20 coll | 2000 | 13.685 | 17.722 | 20.660 | avg 2783 rows |
| pg | range large month 5 coll | 2000 | 10.355 | 30.518 | 36.372 | avg 2920 rows |
| pg | range large month 20 coll | 2000 | 52.744 | 64.093 | 91.451 | avg 11761 rows |
| pg | range large month 20 coll ids-only | 2000 | 8.642 | 10.607 | 13.705 | avg 11759 rows |
| pg | changes regular since head-100 limit 500 | 2000 | 0.484 | 1.150 | 1.719 | avg 92 rows |
| pg | changes regular since random limit 500 | 2000 | 2.139 | 2.523 | 3.262 | avg 474 rows |
| pg | changes large since head-100 limit 500 | 2000 | 0.395 | 0.575 | 1.158 | avg 100 rows |
| pg | changes large since random limit 500 | 2000 | 2.145 | 2.539 | 4.894 | avg 498 rows |
| pg | range regular month 20 coll, 8 reader connections | 16000 | 2.682 | 3.714 | 5.324 | 2929 queries/s |
| pg | mixed read (month range + changes) baseline, no writers | 51658 | 3.094 | 3.965 | 11.404 | 8 readers, same 8 tenants |
| pg | mixed read (month range + changes) with 8 writers | 35212 | 4.375 | 8.128 | 36.882 | 8 readers, same 8 tenants |
| pg | mixed write page=250 updates | 19958 | 7.697 | 19.191 | 46.399 | 166562 items/s while reading |
| pg | outbox accept, synchronous_commit=on, 1 writer | 2000 | 1.278 | 3.267 | 32.691 | mutation+entity rev+journal+outbox in one fsync'd txn |
| pg | outbox accept, synchronous_commit=on, 16 tenants concurrently | 4800 | 2.039 | 7.785 | 11.264 |  |
| pg | new connection + prepare + first query | 200 | 4.014 | 5.240 | 5.610 | what a pool saves; compare with SQLite cold open |
