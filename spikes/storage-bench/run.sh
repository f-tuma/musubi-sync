#!/usr/bin/env bash
# Full storage benchmark: SQLite-per-tenant vs shared PostgreSQL 17. See README.md.
# KEEP=1 keeps the container and data afterwards (also on failure).
# SKIP_PG=1 skips the PostgreSQL phases and the backup table (SQLite-only rerun; results/pg.md and backup.md stay).
# ACCEPTMIX_ONLY=1 runs only the accept-during-ingest scenario on both backends (results/acceptmix.md).
set -euo pipefail
cd "$(dirname "$0")"
# One run at a time: two runs share the container and data, and one's cleanup would wipe the other.
# Children inherit the descriptor, so an orphaned benchmark process keeps the lock too.
exec 9>.run.lock
flock -n 9 || { echo "another run.sh (or its benchmark process) is still running" >&2; exit 1; }
export BENCH_DATA=${BENCH_DATA:-$PWD/data}
PG=musubi-storage-bench-pg
MARK=.storage-bench-data
# Never delete a directory this script did not create: it must be new/empty or carry our marker.
if [ -e "$BENCH_DATA" ] && [ ! -e "$BENCH_DATA/$MARK" ] && [ -n "$(ls -A "$BENCH_DATA")" ]; then
  echo "BENCH_DATA=$BENCH_DATA exists, is not empty and has no $MARK marker; refusing to use it" >&2; exit 1
fi
mkdir -p "$BENCH_DATA/pg" results
touch "$BENCH_DATA/$MARK"
cleanup() {
  [ "${KEEP:-0}" = 1 ] && return
  docker rm -f $PG >/dev/null 2>&1 || true
  [ -e "$BENCH_DATA/$MARK" ] && rm -rf "$BENCH_DATA"
}
trap cleanup EXIT
# Same filesystem for both backends; nodatacow so btrfs CoW/compression does not skew either one.
chattr +C "$BENCH_DATA" 2>/dev/null || echo "chattr +C failed (not btrfs?), continuing"

# A stopped container with the same name (e.g. after a crash) would make `docker run` fail.
if docker ps -a --filter status=exited --filter status=created --format '{{.Names}}' | grep -qx $PG; then
  docker rm $PG >/dev/null
fi
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

# Everything up to acceptmix; ACCEPTMIX_ONLY=1 skips it (and SKIP_* then have no effect).
if [ "${ACCEPTMIX_ONLY:-0}" != 1 ]; then
if [ "${SKIP_SQLITE:-0}" != 1 ]; then
  { $B sqlite load size; $B sqlite read mixed outbox coldopen; } | tee results/sqlite.md
fi
if [ "${SKIP_PG:-0}" != 1 ]; then
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
fi

# Raw layer compressed with zstd (measured on one regular and one large tenant). Runs after mixed/outbox,
# which only touch tenants 0..15, so a regular tenant >= 16 still has the state of the size row.
python3 -I size_zstd.py "$BENCH_DATA/sqlite/t00100.db" "$BENCH_DATA/sqlite/t100001.db" | tee results/size-zstd.md

# A/B switches on the loaded data (review finding 4): MEMSTATUS and checkpoint strategy.
for m in 0 1; do SQLITE_MEMSTATUS=$m $B sqlite read; done | tee results/ab-memstatus.md
{ for c in bg inline passive; do SQLITE_CKPT=$c $B sqlite ingest-large; done
  SQLITE_WAL_HARD_MB=256 $B sqlite ingest-large; } | tee results/ab-ckpt-ingest.md
for c in bg passive; do SQLITE_CKPT=$c MIXED_SECS=20 $B sqlite mixed; done | tee results/ab-ckpt-mixed.md
fi
# Outbox accept on a tenant under ingest, with and without a long reader; SQLite also with mitigations A (separate
# outbox file) and B (shorter ingest txns). Last: it adds tenant files / a PG tenant.
{ $B sqlite acceptmix; [ "${SKIP_PG:-0}" = 1 ] && [ "${ACCEPTMIX_ONLY:-0}" != 1 ] || $B pg acceptmix; } | tee results/acceptmix.md
