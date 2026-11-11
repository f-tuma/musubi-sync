<!-- SQLITE_CKPT=bg -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | mixed read (month range + changes) baseline, no writers | 189216 | 0.855 | 1.069 | 5.313 | 8 readers, same 8 tenants; ckpt=bg: max WAL 0.0 MB, 0 passive, 0 truncate (max 0 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | mixed read (month range + changes) with 8 writers | 76111 | 2.046 | 4.051 | 12.172 | 8 readers, same 8 tenants; ckpt=bg: max WAL 169.0 MB, 1296 passive, 326 truncate (max 569 ms; 38 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | mixed write page=250 updates | 49246 | 2.750 | 12.998 | 573.563 | 410572 items/s while reading |
<!-- SQLITE_CKPT=passive -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | mixed read (month range + changes) baseline, no writers | 193963 | 0.830 | 1.084 | 6.190 | 8 readers, same 8 tenants; ckpt=passive-50ms: max WAL 0.0 MB, 3200 passive, 0 truncate (max 0 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | mixed read (month range + changes) with 8 writers | 74613 | 2.083 | 3.830 | 12.951 | 8 readers, same 8 tenants; ckpt=passive-50ms: max WAL 3141.3 MB, 1644 passive, 0 truncate (max 0 ms; 0 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | mixed write page=250 updates | 54394 | 2.759 | 8.323 | 37.667 | 453438 items/s while reading |
