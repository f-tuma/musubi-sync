#!/usr/bin/env python3
"""Tenant file size with the raw layer compressed (finding: the proposal's ~2 KB/item assumes compressed raw).

Copies a tenant file, rewrites raw.body as zstd (per row, optionally with a dictionary trained on that
tenant's own bodies), VACUUMs and measures the real file. Usage: size_zstd.py <tenant.db>...
Needs Python >= 3.14 (compression.zstd); stdlib only.
"""
import os
import random
import shutil
import sqlite3
import sys
import tempfile
from compression import zstd


def measure(src, mode, tmp):
    dst = os.path.join(tmp, os.path.basename(src) + "." + mode)
    shutil.copyfile(src, dst)
    c = sqlite3.connect(dst)
    bodies = [b for (b,) in c.execute("SELECT body FROM raw")]
    raw_bytes = sum(len(b.encode()) for b in bodies)
    if mode != "plain":
        zd = None
        if mode == "zstd-dict":
            sample = [b.encode() for b in random.Random(1).sample(bodies, min(2000, len(bodies)))]
            zd = zstd.train_dict(sample, 32 * 1024)
        c.create_function("z", 1, lambda b: zstd.compress(b.encode(), level=3, zstd_dict=zd), deterministic=True)
        c.execute("UPDATE raw SET body = z(body)")  # BLOB in a TEXT column: SQLite keeps it as a BLOB
        c.commit()
    stored = c.execute("SELECT sum(length(body)) FROM raw").fetchone()[0]
    raw_pages = c.execute("SELECT sum(pgsize) FROM dbstat WHERE name='raw'").fetchone()[0]
    c.execute("VACUUM")
    c.close()
    size = os.path.getsize(dst)
    os.remove(dst)
    return raw_bytes, stored, raw_pages, size


def main():
    with tempfile.TemporaryDirectory(dir=os.path.dirname(os.path.abspath(sys.argv[1]))) as tmp:
        print("| tenant | raw layer | items | raw body MB | stored body MB | raw table MB (pages, before VACUUM) | file MB (after VACUUM) | file bytes / item |")
        print("|---|---|---|---|---|---|---|---|")
        for src in sys.argv[1:]:
            items = sqlite3.connect(f"file:{src}?mode=ro", uri=True).execute("SELECT count(*) FROM items").fetchone()[0]
            for mode in ["plain", "zstd", "zstd-dict"]:
                raw_bytes, stored, raw_pages, size = measure(src, mode, tmp)
                print(f"| {os.path.basename(src)} | {mode} | {items} | {raw_bytes / 1e6:.1f} | {stored / 1e6:.1f} "
                      f"| {raw_pages / 1e6:.1f} | {size / 1e6:.1f} | {size / items:.0f} |")


if __name__ == "__main__":
    main()
