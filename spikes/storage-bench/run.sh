#!/usr/bin/env bash
# Full storage benchmark: SQLite-per-tenant vs shared PostgreSQL 17. See README.md.
# KEEP=1 keeps the container and data afterwards.
set -euo pipefail
cd "$(dirname "$0")"
export BENCH_DATA=${BENCH_DATA:-$PWD/data}
PG=musubi-storage-bench-pg
mkdir -p "$BENCH_DATA/pg" results
# Same filesystem for both backends; nodatacow so btrfs CoW/compression does not skew either one.
chattr +C "$BENCH_DATA" 2>/dev/null || echo "chattr +C failed (not btrfs?), continuing"

if ! docker ps --format '{{.Names}}' | grep -qx $PG; then
  docker run -d --name $PG --network host --shm-size=10g \
    --user "$(id -u):$(id -g)" -v /etc/passwd:/etc/passwd:ro \
    -e POSTGRES_USER=bench -e POSTGRES_PASSWORD=bench -e POSTGRES_DB=bench \
    -v "$BENCH_DATA/pg:/var/lib/postgresql/data" postgres:17 \
    -c listen_addresses=127.0.0.1 -c port=55432 -c shared_buffers=8GB \
    -c effective_cache_size=32GB -c work_mem=32MB -c maintenance_work_mem=1GB \
    -c max_wal_size=16GB -c min_wal_size=2GB -c checkpoint_timeout=15min \
    -c checkpoint_completion_target=0.9 -c wal_buffers=64MB -c max_connections=200 \
    -c random_page_cost=1.1 -c effective_io_concurrency=200 -c wal_compression=lz4
  until docker exec $PG pg_isready -h 127.0.0.1 -p 55432 -U bench -q; do sleep 1; done
fi

cargo test --release
cargo build --release
B=./target/release/storage-bench

{
  echo "# Environment"; date -u
  lscpu | grep -E 'Model name|^CPU\(s\)|max MHz'; free -g | head -2; uname -r
  findmnt -T "$BENCH_DATA" -o SOURCE,FSTYPE,OPTIONS | tail -1
  lsblk -dno MODEL "$(findmnt -T "$BENCH_DATA" -no SOURCE | sed 's/p\?[0-9]*$//')" 2>/dev/null || true
  docker exec $PG postgres --version; cargo --version
  echo "other running containers: $(docker ps -q | wc -l)"
} | tee results/env.md

[ "${SKIP_SQLITE:-0}" = 1 ] || $B sqlite load size read mixed outbox coldopen | tee results/sqlite.md
$B pg load size read mixed outbox connect | tee results/pg.md

# Backup / restore of the full data set (rough, single run).
{
  echo; echo "| backend | test | ms | notes |"; echo "|---|---|---|---|"
  rm -rf "$BENCH_DATA/sqlite-backup"
  s=$(date +%s%N); cp -r --reflink=never "$BENCH_DATA/sqlite" "$BENCH_DATA/sqlite-backup"; sync -f "$BENCH_DATA/sqlite-backup"; e=$(date +%s%N)
  echo "| sqlite | backup = copy of checkpointed tenant files + sync | $(( (e - s) / 1000000 )) | $(du -sh "$BENCH_DATA/sqlite" | cut -f1) |"
  rm -rf "$BENCH_DATA/sqlite-backup"
  s=$(date +%s%N); tar -C "$BENCH_DATA" -cf - sqlite | zstd -1 -T0 -q -o "$BENCH_DATA/sqlite.tar.zst"; sync -f "$BENCH_DATA/sqlite.tar.zst"; e=$(date +%s%N)
  echo "| sqlite | backup = tar + zstd -1 of checkpointed tenant files + sync | $(( (e - s) / 1000000 )) | $(du -h "$BENCH_DATA/sqlite.tar.zst" | cut -f1) |"
  mkdir -p "$BENCH_DATA/sqlite-restore"
  s=$(date +%s%N); zstd -dc -T0 "$BENCH_DATA/sqlite.tar.zst" | tar -C "$BENCH_DATA/sqlite-restore" -xf -; sync -f "$BENCH_DATA/sqlite-restore"; e=$(date +%s%N)
  echo "| sqlite | restore = unzstd + untar + sync | $(( (e - s) / 1000000 )) | |"
  rm -rf "$BENCH_DATA/sqlite-restore" "$BENCH_DATA/sqlite.tar.zst"
  s=$(date +%s%N); docker exec $PG pg_dump -U bench -h 127.0.0.1 -p 55432 -Fc -Z lz4 -f /var/lib/postgresql/data/dump.pgc bench; e=$(date +%s%N)
  echo "| pg | pg_dump -Fc -Z lz4 | $(( (e - s) / 1000000 )) | $(du -h "$BENCH_DATA/pg/dump.pgc" | cut -f1) |"
  docker exec $PG psql -U bench -h 127.0.0.1 -p 55432 -qc "DROP DATABASE IF EXISTS restore" -c "CREATE DATABASE restore" bench
  s=$(date +%s%N); docker exec $PG pg_restore -U bench -h 127.0.0.1 -p 55432 -j 8 -d restore /var/lib/postgresql/data/dump.pgc; e=$(date +%s%N)
  echo "| pg | pg_restore -j 8 (incl. index build) | $(( (e - s) / 1000000 )) | |"
  rm -f "$BENCH_DATA/pg/dump.pgc"
} | tee results/backup.md

if [ "${KEEP:-0}" != 1 ]; then
  docker rm -f $PG >/dev/null
  rm -rf "$BENCH_DATA"
fi
