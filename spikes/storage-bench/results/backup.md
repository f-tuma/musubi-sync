
| backend | test | ms | notes |
|---|---|---|---|
| sqlite | backup = copy of checkpointed tenant files + sync | 3030 | 5,5G |
| sqlite | backup = tar + zstd -1 of checkpointed tenant files + sync | 2051 | 827M |
| sqlite | restore = unzstd + untar + sync | 3299 | |
| pg | pg_dump -Fc -Z lz4 | 5732 | 975M |
| pg | pg_restore -j 8 (incl. index build) | 8493 | |
