| tenant | raw layer | items | raw body MB | stored body MB | raw table MB (pages, before VACUUM) | file MB (after VACUUM) | file bytes / item |
|---|---|---|---|---|---|---|---|
| t00100.db | plain | 5000 | 5.2 | 5.1 | 14.0 | 21.2 | 4245 |
| t00100.db | zstd | 5000 | 5.2 | 3.1 | 5.5 | 11.7 | 2343 |
| t00100.db | zstd-dict | 5000 | 5.2 | 1.0 | 5.5 | 9.3 | 1869 |
| t100001.db | plain | 100000 | 94.4 | 93.4 | 263.8 | 412.5 | 4125 |
| t100001.db | zstd | 100000 | 94.4 | 56.3 | 99.6 | 230.5 | 2305 |
| t100001.db | zstd-dict | 100000 | 94.4 | 17.7 | 99.2 | 187.1 | 1871 |
