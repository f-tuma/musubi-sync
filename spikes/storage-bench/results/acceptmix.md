| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | acceptmix accept during ingest, no reader | 1056 | 12.476 | 104.890 | 134.749 | enqueue->commit, synchronous=FULL, same writer as ingest; ckpt=bg: max WAL 108.8 MB, 169 passive, 83 truncate (max 113 ms; 9 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix ingest page=250, no reader | 1189 | 8.918 | 98.598 | 122.690 | 13667 items/s incl. accepts |
| sqlite | acceptmix accept during ingest, long reader (1.5 s snapshot) | 892 | 9.353 | 242.341 | 337.334 | enqueue->commit, synchronous=FULL, same writer as ingest; ckpt=bg: max WAL 2979.2 MB, 26 passive, 139 truncate (max 57 ms; 138 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix ingest page=250, long reader (1.5 s snapshot) | 1189 | 9.941 | 23.975 | 275.696 | 15485 items/s incl. accepts |
