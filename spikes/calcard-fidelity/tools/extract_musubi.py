#!/usr/bin/env python3
"""Extract ICS/vCard payloads embedded in the Musubi repo (read-only) into corpus/musubi/.

Sources: JSON behaviour scenarios (every string holding BEGIN:VCALENDAR/VCARD) and TS test
files (template literals without ${} and ["...", ...].join("\\r\\n"|"\\n") arrays made only of
string literals) and quoted strings with \\r\\n escapes. Payloads built from variables/spreads are skipped and counted.
Usage: python3 -I tools/extract_musubi.py <musubi-root> <out-dir>
"""
import hashlib, json, os, re, sys

root, out = sys.argv[1], sys.argv[2]
os.makedirs(out, exist_ok=True)
seen, stats = {}, {"json": 0, "ts_template": 0, "ts_array": 0, "ts_string": 0, "ts_skipped": 0}
STR = r'"(?:[^"\\]|\\.)*"'
ARRAY = re.compile(r'\[\s*(' + STR + r'(?:\s*,\s*' + STR + r')*)\s*,?\s*\]\s*\.join\(\s*("\\r\\n"|"\\n"|\'\\r\\n\'|\'\\n\')\s*\)')

SPAN = re.compile(r"BEGIN:(VCALENDAR|VCARD)\r?\n.*?END:\1(?:\r?\n)?", re.S)

def save(text, origin, kind):
    # one payload per BEGIN..END span; strips CDATA/XML wrappers and surrounding code
    for m in SPAN.finditer(text):
        save_one(m.group(0), origin, kind)

def save_one(text, origin, kind):
    h = hashlib.sha256(text.encode()).hexdigest()[:12]
    if h in seen:
        return
    ext = "vcf" if text.lstrip().startswith("BEGIN:VCARD") else "ics"
    name = re.sub(r"[^A-Za-z0-9]+", "_", origin)[-60:]
    seen[h] = f"{name}_{h}.{ext}"
    with open(os.path.join(out, seen[h]), "w", newline="") as f:
        f.write(text)
    stats[kind] += 1

for dirpath, dirs, files in os.walk(root):
    dirs[:] = [d for d in dirs if d not in ("node_modules", ".git", "backups", "dist", "build")]
    for fn in files:
        p = os.path.join(dirpath, fn)
        rel = os.path.relpath(p, root)
        if fn.endswith(".json"):
            try:
                data = json.load(open(p))
            except Exception:
                continue
            def walk(x):
                if isinstance(x, str): save(x, rel, "json")
                elif isinstance(x, dict): [walk(v) for v in x.values()]
                elif isinstance(x, list): [walk(v) for v in x]
            walk(data)
        elif fn.endswith(".ts"):
            src = open(p, errors="replace").read()
            if "BEGIN:VCALENDAR" not in src and "BEGIN:VCARD" not in src:
                continue
            for m in ARRAY.finditer(src):
                items = json.loads("[" + m.group(1) + "]")
                sep = "\r\n" if "r" in m.group(2) else "\n"
                save(sep.join(items), rel, "ts_array")
            for m in re.finditer(r"`\s*(BEGIN:(?:VCALENDAR|VCARD)[^`]*?END:(?:VCALENDAR|VCARD)\s*)`", src):
                if "${" in m.group(1):
                    stats["ts_skipped"] += 1
                else:
                    save(m.group(1), rel, "ts_template")
            for m in re.finditer(r'"((?:[^"\\\n]|\\.)*BEGIN:V(?:CALENDAR|CARD)(?:[^"\\\n]|\\.)*)"|\'((?:[^\'\\\n]|\\.)*BEGIN:V(?:CALENDAR|CARD)(?:[^\'\\\n]|\\.)*)\'', src):
                raw = m.group(1) if m.group(1) is not None else m.group(2).replace("\\'", "'").replace('"', '\\"')
                try:
                    save(json.loads('"' + raw + '"'), rel, "ts_string")
                except ValueError:
                    pass
            stats["ts_skipped"] += max(0, src.count("BEGIN:VCALENDAR") - len(ARRAY.findall(src)))

print(json.dumps(stats), len(seen), "unique files")
