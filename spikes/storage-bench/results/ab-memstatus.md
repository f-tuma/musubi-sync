<!-- SQLITE_MEMSTATUS=0 -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | range regular week 5 coll | 2000 | 0.137 | 0.390 | 0.492 | avg 35 rows |
| sqlite | range regular week 20 coll | 2000 | 0.367 | 0.501 | 0.647 | avg 140 rows |
| sqlite | range regular month 5 coll | 2000 | 0.217 | 0.572 | 0.962 | avg 150 rows |
| sqlite | range regular month 20 coll | 2000 | 0.942 | 8.811 | 25.110 | avg 591 rows |
| sqlite | range regular month 20 coll ids-only | 2000 | 0.344 | 6.394 | 10.573 | avg 590 rows |
| sqlite | range large week 5 coll | 2000 | 1.458 | 3.998 | 4.463 | avg 705 rows |
| sqlite | range large week 20 coll | 2000 | 7.047 | 11.513 | 17.464 | avg 2783 rows |
| sqlite | range large month 5 coll | 2000 | 4.150 | 12.891 | 15.961 | avg 2920 rows |
| sqlite | range large month 20 coll | 2000 | 24.281 | 29.610 | 46.430 | avg 11761 rows |
| sqlite | range large month 20 coll ids-only | 2000 | 5.966 | 8.284 | 43.440 | avg 11759 rows |
| sqlite | changes regular since head-100 limit 500 | 2000 | 0.070 | 0.161 | 0.212 | avg 63 rows |
| sqlite | changes regular since random limit 500 | 2000 | 0.407 | 0.464 | 0.625 | avg 459 rows |
| sqlite | changes large since head-100 limit 500 | 2000 | 0.044 | 0.047 | 0.163 | avg 100 rows |
| sqlite | changes large since random limit 500 | 2000 | 0.448 | 0.534 | 0.587 | avg 498 rows |
| sqlite | range regular month 20 coll, 8 reader threads | 16000 | 0.886 | 1.762 | 7.175 | 6592 queries/s |
<!-- SQLITE_MEMSTATUS=1 -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | range regular week 5 coll | 2000 | 0.139 | 0.385 | 0.569 | avg 35 rows |
| sqlite | range regular week 20 coll | 2000 | 0.360 | 0.482 | 0.563 | avg 140 rows |
| sqlite | range regular month 5 coll | 2000 | 0.214 | 0.544 | 0.744 | avg 150 rows |
| sqlite | range regular month 20 coll | 2000 | 0.832 | 1.018 | 1.095 | avg 591 rows |
| sqlite | range regular month 20 coll ids-only | 2000 | 0.343 | 0.409 | 0.489 | avg 590 rows |
| sqlite | range large week 5 coll | 2000 | 1.149 | 3.019 | 4.216 | avg 705 rows |
| sqlite | range large week 20 coll | 2000 | 5.454 | 7.895 | 8.742 | avg 2783 rows |
| sqlite | range large month 5 coll | 2000 | 4.210 | 13.335 | 16.742 | avg 2920 rows |
| sqlite | range large month 20 coll | 2000 | 23.796 | 29.619 | 32.427 | avg 11761 rows |
| sqlite | range large month 20 coll ids-only | 2000 | 6.993 | 8.759 | 9.550 | avg 11759 rows |
| sqlite | changes regular since head-100 limit 500 | 2000 | 0.075 | 0.167 | 0.208 | avg 63 rows |
| sqlite | changes regular since random limit 500 | 2000 | 0.405 | 0.480 | 0.669 | avg 459 rows |
| sqlite | changes large since head-100 limit 500 | 2000 | 0.046 | 0.052 | 0.170 | avg 100 rows |
| sqlite | changes large since random limit 500 | 2000 | 0.440 | 0.545 | 0.626 | avg 498 rows |
| sqlite | range regular month 20 coll, 8 reader threads | 16000 | 4.479 | 6.893 | 23.968 | 1629 queries/s |
