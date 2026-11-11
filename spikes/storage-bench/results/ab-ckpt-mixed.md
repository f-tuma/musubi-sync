<!-- SQLITE_CKPT=bg -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | mixed read (month range + changes) baseline, no writers | 176285 | 0.906 | 1.188 | 9.054 | 8 readers, same 8 tenants; ckpt=bg: max WAL 0.0 MB, 0 passive, 0 truncate (0 busy: readers past 100 ms or another checkpoint running) |
| sqlite | mixed read (month range + changes) with 8 writers | 70455 | 2.172 | 4.562 | 12.826 | 8 readers, same 8 tenants; ckpt=bg: max WAL 71.2 MB, 1444 passive, 389 truncate (55 busy: readers past 100 ms or another checkpoint running) |
| sqlite | mixed write page=250 updates | 50295 | 2.840 | 9.155 | 191.234 | 419341 items/s while reading |
<!-- SQLITE_CKPT=passive -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | mixed read (month range + changes) baseline, no writers | 186989 | 0.852 | 1.062 | 2.508 | 8 readers, same 8 tenants; ckpt=passive-50ms: max WAL 0.0 MB, 0 passive, 0 truncate (0 busy: readers past 100 ms or another checkpoint running) |
| sqlite | mixed read (month range + changes) with 8 writers | 67158 | 2.268 | 4.849 | 30.118 | 8 readers, same 8 tenants; ckpt=passive-50ms: max WAL 3095.7 MB, 0 passive, 0 truncate (0 busy: readers past 100 ms or another checkpoint running) |
| sqlite | mixed write page=250 updates | 53330 | 2.819 | 8.552 | 36.294 | 444697 items/s while reading |
