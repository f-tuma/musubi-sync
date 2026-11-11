<!-- run 1 of 3 (ACCEPTMIX_ONLY=1 ./run.sh) -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | acceptmix[same] accept during ingest, no reader | 1261 | 13.437 | 204.787 | 268.080 | enqueue->commit, synchronous=FULL, same writer as ingest; ckpt=bg: max WAL 109.0 MB, 169 passive, 83 truncate (max 261 ms; 7 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[same] ingest page=250, no reader | 1189 | 12.200 | 197.192 | 275.692 | 11451 items/s incl. accepts |
| sqlite | acceptmix[same] accept during ingest, long reader (1.5 s snapshot) | 955 | 10.268 | 316.196 | 414.176 | enqueue->commit, synchronous=FULL, same writer as ingest; ckpt=bg: max WAL 4857.2 MB, 26 passive, 148 truncate (max 286 ms; 148 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[same] ingest page=250, long reader (1.5 s snapshot) | 1189 | 12.608 | 46.397 | 421.957 | 14195 items/s incl. accepts |
| sqlite | acceptmix[split] accept during ingest, no reader | 935 | 1.574 | 10.861 | 17.940 | enqueue->commit, synchronous=FULL, separate outbox file and writer, projected by the ingest writer; ckpt=bg: max WAL 109.2 MB, 136 passive, 78 truncate (max 205 ms; 11 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[split] ingest page=250, no reader | 1189 | 9.712 | 139.320 | 214.274 | 15425 items/s incl. accepts |
| sqlite | acceptmix[split] accept during ingest, long reader (1.5 s snapshot) | 817 | 1.264 | 53.534 | 82.448 | enqueue->commit, synchronous=FULL, separate outbox file and writer, projected by the ingest writer; ckpt=bg: max WAL 6003.1 MB, 34 passive, 164 truncate (max 410 ms; 164 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[split] ingest page=250, long reader (1.5 s snapshot) | 1189 | 9.900 | 23.290 | 513.844 | 17036 items/s incl. accepts |
| sqlite | acceptmix[chunked] accept during ingest, no reader | 1529 | 6.630 | 172.901 | 254.465 | enqueue->commit, synchronous=FULL, same writer, ingest page split into 25-item txns; ckpt=bg: max WAL 102.4 MB, 237 passive, 122 truncate (max 261 ms; 13 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[chunked] ingest page=250, no reader | 1189 | 17.633 | 142.699 | 485.067 | 9443 items/s incl. accepts |
| sqlite | acceptmix[chunked] accept during ingest, long reader (1.5 s snapshot) | 1566 | 4.964 | 613.036 | 806.493 | enqueue->commit, synchronous=FULL, same writer, ingest page split into 25-item txns; ckpt=bg: max WAL 7172.6 MB, 39 passive, 242 truncate (max 532 ms; 242 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[chunked] ingest page=250, long reader (1.5 s snapshot) | 1189 | 18.019 | 350.335 | 897.147 | 8965 items/s incl. accepts |
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| pg | acceptmix accept during ingest, no reader | 528 | 3.839 | 25.778 | 37.885 | enqueue->commit, synchronous_commit=on, own connection next to the ingest one |
| pg | acceptmix ingest page=250, no reader | 1189 | 8.676 | 26.193 | 38.219 | 26078 items/s incl. accepts, synchronous_commit=off |
| pg | acceptmix accept during ingest, long reader (1.5 s snapshot) | 511 | 2.947 | 22.576 | 50.927 | enqueue->commit, synchronous_commit=on, own connection next to the ingest one |
| pg | acceptmix ingest page=250, long reader (1.5 s snapshot) | 1189 | 8.671 | 21.700 | 50.919 | 26945 items/s incl. accepts, synchronous_commit=off |
<!-- run 2 of 3 (ACCEPTMIX_ONLY=1 ./run.sh) -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | acceptmix[same] accept during ingest, no reader | 1193 | 12.742 | 280.730 | 339.615 | enqueue->commit, synchronous=FULL, same writer as ingest; ckpt=bg: max WAL 144.9 MB, 149 passive, 81 truncate (max 337 ms; 12 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[same] ingest page=250, no reader | 1189 | 11.964 | 191.541 | 345.694 | 12099 items/s incl. accepts |
| sqlite | acceptmix[same] accept during ingest, long reader (1.5 s snapshot) | 915 | 9.952 | 264.337 | 306.188 | enqueue->commit, synchronous=FULL, same writer as ingest; ckpt=bg: max WAL 4802.7 MB, 23 passive, 148 truncate (max 13 ms; 148 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[same] ingest page=250, long reader (1.5 s snapshot) | 1189 | 12.500 | 29.721 | 321.022 | 15486 items/s incl. accepts |
| sqlite | acceptmix[split] accept during ingest, no reader | 941 | 1.386 | 7.676 | 54.301 | enqueue->commit, synchronous=FULL, separate outbox file and writer, projected by the ingest writer; ckpt=bg: max WAL 109.5 MB, 148 passive, 81 truncate (max 175 ms; 7 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[split] ingest page=250, no reader | 1189 | 9.865 | 142.613 | 184.174 | 15335 items/s incl. accepts |
| sqlite | acceptmix[split] accept during ingest, long reader (1.5 s snapshot) | 865 | 1.377 | 83.277 | 248.819 | enqueue->commit, synchronous=FULL, separate outbox file and writer, projected by the ingest writer; ckpt=bg: max WAL 6003.8 MB, 38 passive, 164 truncate (max 318 ms; 164 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[split] ingest page=250, long reader (1.5 s snapshot) | 1189 | 10.249 | 34.012 | 465.480 | 15485 items/s incl. accepts |
| sqlite | acceptmix[chunked] accept during ingest, no reader | 1425 | 6.632 | 78.812 | 110.749 | enqueue->commit, synchronous=FULL, same writer, ingest page split into 25-item txns; ckpt=bg: max WAL 103.2 MB, 258 passive, 123 truncate (max 103 ms; 10 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[chunked] ingest page=250, no reader | 1189 | 17.316 | 121.716 | 284.507 | 10090 items/s incl. accepts |
| sqlite | acceptmix[chunked] accept during ingest, long reader (1.5 s snapshot) | 1361 | 4.545 | 206.421 | 263.404 | enqueue->commit, synchronous=FULL, same writer, ingest page split into 25-item txns; ckpt=bg: max WAL 3426.0 MB, 45 passive, 164 truncate (max 40 ms; 163 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[chunked] ingest page=250, long reader (1.5 s snapshot) | 1189 | 16.460 | 244.995 | 288.706 | 10019 items/s incl. accepts |
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| pg | acceptmix accept during ingest, no reader | 482 | 2.734 | 21.749 | 28.903 | enqueue->commit, synchronous_commit=on, own connection next to the ingest one |
| pg | acceptmix ingest page=250, no reader | 1189 | 7.992 | 22.937 | 32.857 | 28581 items/s incl. accepts, synchronous_commit=off |
| pg | acceptmix accept during ingest, long reader (1.5 s snapshot) | 3011 | 2.010 | 10.834 | 72.668 | enqueue->commit, synchronous_commit=on, own connection next to the ingest one |
| pg | acceptmix ingest page=250, long reader (1.5 s snapshot) | 1189 | 19.097 | 208.907 | 224.354 | 4565 items/s incl. accepts, synchronous_commit=off |
<!-- run 3 of 3 (ACCEPTMIX_ONLY=1 ./run.sh) -->
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| sqlite | acceptmix[same] accept during ingest, no reader | 1290 | 13.927 | 372.697 | 541.371 | enqueue->commit, synchronous=FULL, same writer as ingest; ckpt=bg: max WAL 108.4 MB, 155 passive, 82 truncate (max 528 ms; 12 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[same] ingest page=250, no reader | 1189 | 11.927 | 227.828 | 539.017 | 11192 items/s incl. accepts |
| sqlite | acceptmix[same] accept during ingest, long reader (1.5 s snapshot) | 943 | 9.542 | 280.822 | 330.241 | enqueue->commit, synchronous=FULL, same writer as ingest; ckpt=bg: max WAL 6021.6 MB, 22 passive, 164 truncate (max 11 ms; 164 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[same] ingest page=250, long reader (1.5 s snapshot) | 1189 | 12.636 | 33.039 | 347.906 | 14195 items/s incl. accepts |
| sqlite | acceptmix[split] accept during ingest, no reader | 936 | 3.119 | 16.091 | 111.463 | enqueue->commit, synchronous=FULL, separate outbox file and writer, projected by the ingest writer; ckpt=bg: max WAL 108.9 MB, 137 passive, 78 truncate (max 214 ms; 11 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[split] ingest page=250, no reader | 1189 | 9.842 | 145.491 | 222.006 | 15395 items/s incl. accepts |
| sqlite | acceptmix[split] accept during ingest, long reader (1.5 s snapshot) | 869 | 1.396 | 62.099 | 94.118 | enqueue->commit, synchronous=FULL, separate outbox file and writer, projected by the ingest writer; ckpt=bg: max WAL 6004.0 MB, 38 passive, 164 truncate (max 298 ms; 164 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[split] ingest page=250, long reader (1.5 s snapshot) | 1189 | 10.457 | 40.660 | 415.827 | 15485 items/s incl. accepts |
| sqlite | acceptmix[chunked] accept during ingest, no reader | 1368 | 6.581 | 76.202 | 93.918 | enqueue->commit, synchronous=FULL, same writer, ingest page split into 25-item txns; ckpt=bg: max WAL 102.4 MB, 235 passive, 118 truncate (max 85 ms; 15 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[chunked] ingest page=250, no reader | 1189 | 16.977 | 124.814 | 222.046 | 10494 items/s incl. accepts |
| sqlite | acceptmix[chunked] accept during ingest, long reader (1.5 s snapshot) | 1212 | 4.236 | 218.535 | 293.936 | enqueue->commit, synchronous=FULL, same writer, ingest page split into 25-item txns; ckpt=bg: max WAL 5174.2 MB, 34 passive, 207 truncate (max 149 ms; 206 busy: readers past 10 ms or another checkpoint running; then back-off 1 s or +32 MB WAL) |
| sqlite | acceptmix[chunked] ingest page=250, long reader (1.5 s snapshot) | 1189 | 15.725 | 213.880 | 336.589 | 11355 items/s incl. accepts |
| backend | test | n | p50 ms | p99 ms | max ms | notes |
|---|---|---|---|---|---|---|
| pg | acceptmix accept during ingest, no reader | 516 | 4.292 | 37.231 | 85.921 | enqueue->commit, synchronous_commit=on, own connection next to the ingest one |
| pg | acceptmix ingest page=250, no reader | 1189 | 8.265 | 29.034 | 87.467 | 26656 items/s incl. accepts, synchronous_commit=off |
| pg | acceptmix accept during ingest, long reader (1.5 s snapshot) | 2973 | 2.048 | 10.781 | 85.174 | enqueue->commit, synchronous_commit=on, own connection next to the ingest one |
| pg | acceptmix ingest page=250, long reader (1.5 s snapshot) | 1189 | 23.347 | 194.396 | 210.522 | 4619 items/s incl. accepts, synchronous_commit=off |
