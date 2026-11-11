
| backend | test | ms | notes |
|---|---|---|---|
| sqlite | backup = copy of checkpointed tenant files + sync | 2717 | 5,4G |
| sqlite | backup = tar + zstd -1 of checkpointed tenant files + sync | 2057 | 808M |
| sqlite | restore = unzstd + untar + sync | 3360 | |
| pg | pg_dump -Fc -Z lz4 | 6197 | 955M |
| pg | pg_restore -j 8 (incl. index build) | 9719 | |
