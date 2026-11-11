<!-- SQLITE_MEMSTATUS=0 -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | range regular week 5 coll | 2000 | 0.131 | 0.377 | 0.520 | avg 35 rows |
| sqlite | range regular week 20 coll | 2000 | 0.362 | 0.495 | 0.566 | avg 140 rows |
| sqlite | range regular month 5 coll | 2000 | 0.212 | 0.545 | 0.674 | avg 150 rows |
| sqlite | range regular month 20 coll | 2000 | 0.834 | 1.042 | 1.339 | avg 591 rows |
| sqlite | range regular month 20 coll ids-only | 2000 | 0.333 | 0.394 | 0.462 | avg 590 rows |
| sqlite | range large week 5 coll | 2000 | 1.122 | 2.855 | 3.891 | avg 705 rows |
| sqlite | range large week 20 coll | 2000 | 5.901 | 8.394 | 9.229 | avg 2783 rows |
| sqlite | range large month 5 coll | 2000 | 4.058 | 13.176 | 15.943 | avg 2920 rows |
| sqlite | range large month 20 coll | 2000 | 22.464 | 25.724 | 27.237 | avg 11761 rows |
| sqlite | range large month 20 coll ids-only | 2000 | 5.825 | 8.061 | 8.659 | avg 11759 rows |
| sqlite | changes regular since head-100 limit 500 | 2000 | 0.075 | 0.164 | 0.204 | avg 63 rows |
| sqlite | changes regular since random limit 500 | 2000 | 0.405 | 0.486 | 0.808 | avg 459 rows |
| sqlite | changes large since head-100 limit 500 | 2000 | 0.044 | 0.047 | 0.188 | avg 100 rows |
| sqlite | changes large since random limit 500 | 2000 | 0.449 | 0.568 | 0.667 | avg 498 rows |
| sqlite | range regular month 20 coll, 8 reader threads | 16000 | 0.874 | 2.122 | 14.963 | 6345 queries/s |
<!-- SQLITE_MEMSTATUS=1 -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | range regular week 5 coll | 2000 | 0.126 | 0.369 | 0.472 | avg 35 rows |
| sqlite | range regular week 20 coll | 2000 | 0.349 | 0.465 | 0.589 | avg 140 rows |
| sqlite | range regular month 5 coll | 2000 | 0.224 | 0.594 | 0.713 | avg 150 rows |
| sqlite | range regular month 20 coll | 2000 | 0.844 | 1.024 | 1.180 | avg 591 rows |
| sqlite | range regular month 20 coll ids-only | 2000 | 0.350 | 0.413 | 0.636 | avg 590 rows |
| sqlite | range large week 5 coll | 2000 | 1.262 | 3.506 | 4.367 | avg 705 rows |
| sqlite | range large week 20 coll | 2000 | 5.338 | 7.305 | 7.899 | avg 2783 rows |
| sqlite | range large month 5 coll | 2000 | 3.896 | 12.050 | 14.436 | avg 2920 rows |
| sqlite | range large month 20 coll | 2000 | 24.355 | 28.729 | 34.314 | avg 11761 rows |
| sqlite | range large month 20 coll ids-only | 2000 | 5.919 | 7.164 | 8.161 | avg 11759 rows |
| sqlite | changes regular since head-100 limit 500 | 2000 | 0.065 | 0.159 | 0.211 | avg 63 rows |
| sqlite | changes regular since random limit 500 | 2000 | 0.400 | 0.446 | 0.614 | avg 459 rows |
| sqlite | changes large since head-100 limit 500 | 2000 | 0.046 | 0.048 | 0.166 | avg 100 rows |
| sqlite | changes large since random limit 500 | 2000 | 0.428 | 0.492 | 0.527 | avg 498 rows |
| sqlite | range regular month 20 coll, 8 reader threads | 16000 | 4.421 | 6.420 | 9.653 | 1633 queries/s |
