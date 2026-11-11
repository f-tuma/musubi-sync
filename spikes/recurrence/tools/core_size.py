"""Measures the extractable recurrence core of each candidate: code lines and coupling.

  python3 tools/core_size.py > results/core_size.json
Code line = non-blank, non-comment line outside `#[cfg(test)]` modules.
Coupling = `crate::` paths that leave the core file set, plus external crates used.
"""
import glob, json, os, re, sys

REG = glob.glob(os.path.expanduser("~/.cargo/registry/src/index.crates.io-*"))[0]
GIT = glob.glob(os.path.expanduser("~/.cargo/git/checkouts/rust-rrule-*/50db3e2"))[0]
RRULE_CORE = ["src/iter/*.rs", "src/core/*.rs", "src/validator/*.rs", "src/error.rs"]
CORES = {
    # rrule-derived iterator inside calcard-next, plus its iCalendar expansion glue (window, overrides, RDATE/EXDATE)
    "calcard-next": (f"{REG}/calcard-next-3.0.1", ["src/datecalc.rs", "src/datecalc/*.rs", "src/icalendar/dates.rs"], "src/datecalc"),
    "rrule-0.14": (f"{REG}/rrule-0.14.0", RRULE_CORE, "src/iter|src/core|src/validator|src/error"),
    "rrule-jiff-port": (f"{GIT}/rrule", RRULE_CORE, "src/iter|src/core|src/validator|src/error"),
}
# RRULE/RDATE string parser: not needed when calcard already parses the ICS
PARSER = {"rrule-0.14": f"{REG}/rrule-0.14.0", "rrule-jiff-port": f"{GIT}/rrule"}


def code_lines(text):
    text = re.split(r"\n#\[cfg\(test\)\]\s*\nmod ", text)[0]
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    return [l for l in text.splitlines() if l.strip() and not l.strip().startswith("//")]


def measure(root, pats):
    files = sorted({f for p in pats for f in glob.glob(f"{root}/{p}")})
    lines, crate_refs, ext = 0, set(), set()
    for f in files:
        src = open(f).read()
        body = code_lines(src)
        lines += len(body)
        joined = "\n".join(body)
        rel = os.path.relpath(f, root)[4:-3].removesuffix("/mod")
        for tree in re.findall(r"^\s*(?:pub(?:\(crate\))?\s+)?use\s+([^;]+);", joined, re.M):
            for path in flatten(re.sub(r"\s+", "", tree)):
                head, *rest = path.split("::")
                if head == "super":  # resolve relative to this file's module
                    path = "::".join(rel.split("/")[:-1] + rest)
                elif head == "crate":
                    path = "::".join(rest)
                elif head not in ("std", "core", "self", "alloc"):
                    ext.add(head)
                    continue
                else:
                    continue
                crate_refs.add(path)
        crate_refs |= {m for m in re.findall(r"\bcrate::((?:\w+::)*\w+)", joined)}
    deps = set(re.findall(r"^\[(?:target\.[^\]]+\.)?dependencies\.([\w-]+)\]", open(f"{root}/Cargo.toml").read(), re.M))
    deps |= set(re.findall(r"^([\w-]+)\s*=", re.split(r"^\[dependencies\]", open(f"{root}/Cargo.toml").read(), flags=re.M)[-1].split("\n[")[0], re.M))
    used = {d for d in deps if re.search(rf"\b{d.replace('-', '_')}::", "\n".join(open(f).read() for f in files))}
    return files, lines, crate_refs, used


def flatten(tree):
    """`a::{b, c::{d}}` -> ["a::b", "a::c::d"]."""
    m = re.match(r"^([\w:]*?)(?:::)?\{(.*)\}$", tree)
    if not m:
        return [tree]
    prefix, inner = m.groups()
    parts, depth, cur = [], 0, ""
    for ch in inner:
        depth += ch == "{"
        depth -= ch == "}"
        if ch == "," and depth == 0:
            parts.append(cur); cur = ""
        else:
            cur += ch
    parts.append(cur)
    return [f"{prefix}::{q}" if prefix else q for part in parts if part for q in flatten(part)]


out = {}
for name, (root, pats, inside) in CORES.items():
    files, lines, refs, ext = measure(root, pats)
    own = {os.path.splitext(os.path.relpath(f, root))[0].replace("src/", "") for f in files}
    mods = {p.split("::")[0] for p in refs}
    foreign = sorted({p for p in refs if p.split("::")[0][:1].islower() and not re.match(inside.replace("src/", ""), p)})
    out[name] = {"files": len(files), "code_lines": lines, "external_crates": sorted(ext), "crate_paths_outside_core": foreign}
    if name in PARSER:
        out[name]["parser_code_lines"] = measure(PARSER[name], ["src/parser/*.rs", "src/parser/*/*.rs"])[1]
print(json.dumps(out, indent=1))
