"""Oracle 1: python-dateutil rruleset + zoneinfo (system tzdb).

dateutil only expands RRULE/RDATE/EXDATE/EXRULE. This wrapper adds what an RFC
5545 reader must do around it, and nothing more:
  * DTSTART is always part of the set (rset.rdate(dtstart));
  * TZID -> zoneinfo, or dateutil.tz.tzical for an embedded VTIMEZONE;
  * PERIOD RDATE -> its start; DATE values -> naive midnight;
  * RECURRENCE-ID overrides replace the instance; RANGE=THISANDFUTURE shifts
    every later instance by the same delta.
Usage: .venv/bin/python oracles/dateutil_oracle.py corpus/cases.json > results/dateutil.json
"""
import datetime as dt, io, json, re, signal, sys
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError
from dateutil import rrule as du, tz as dutz

UTC = dt.timezone.utc


def unfold(text):
    return re.sub(r"\r?\n[ \t]", "", text).splitlines()


def components(lines, name):
    out, cur = [], None
    for ln in lines:
        if ln == f"BEGIN:{name}":
            cur = []
        elif ln == f"END:{name}" and cur is not None:
            out.append(cur)
            cur = None
        elif cur is not None:
            cur.append(ln)
    return out


def prop(line):
    head, _, value = line.partition(":")
    name, *params = head.split(";")
    return name.upper(), dict(p.split("=", 1) for p in params), value


class Ctx:
    def __init__(self, ics):
        lines = unfold(ics)
        self.vtz = {}
        for comp in re.findall(r"BEGIN:VTIMEZONE\n.*?END:VTIMEZONE\n", ics, re.S):
            tzid = re.search(r"^TZID:(.*)$", comp, re.M).group(1)
            self.vtz[tzid] = dutz.tzical(io.StringIO(comp)).get()
        self.events = [[prop(l) for l in e if not l.startswith("BEGIN:") and not l.startswith("END:")]
                       for e in components(lines, "VEVENT")]

    def zone(self, tzid):
        if tzid in self.vtz:
            return self.vtz[tzid]
        try:
            return ZoneInfo(tzid)
        except (ZoneInfoNotFoundError, ValueError):
            raise ValueError(f"unknown TZID {tzid}")

    def value(self, params, v):
        """-> (kind, datetime)"""
        if params.get("VALUE") == "DATE" or re.fullmatch(r"\d{8}", v):
            return "date", dt.datetime.strptime(v, "%Y%m%d")
        if v.endswith("Z"):
            return "utc", dt.datetime.strptime(v, "%Y%m%dT%H%M%SZ").replace(tzinfo=UTC)
        naive = dt.datetime.strptime(v, "%Y%m%dT%H%M%S")
        if "TZID" in params:
            return "zoned", naive.replace(tzinfo=self.zone(params["TZID"]))
        return "floating", naive

    def values(self, params, raw):
        for v in raw.split(","):
            if params.get("VALUE") == "PERIOD" or "/" in v:
                v = v.split("/")[0]
            yield self.value(params, v)[1]


def fmt(kind, d):
    if kind == "date":
        return d.strftime("%Y-%m-%d")
    if kind == "floating":
        return d.strftime("%Y-%m-%dT%H:%M:%S")
    return d.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def expand(case):
    ctx = Ctx(case["ics"])
    master = next(e for e in ctx.events if not any(n == "RECURRENCE-ID" for n, _, _ in e))
    overrides = [e for e in ctx.events if e is not master]
    get = lambda e, name: [(p, v) for n, p, v in e if n == name]
    (sp, sv), = get(master, "DTSTART")
    kind, start = ctx.value(sp, sv)
    rset = du.rruleset()
    rset.rdate(start)
    for _, v in get(master, "RRULE"):
        rset.rrule(du.rrulestr(v, dtstart=start))
    for _, v in get(master, "EXRULE"):
        rset.exrule(du.rrulestr(v, dtstart=start))
    for p, v in get(master, "RDATE"):
        for d in ctx.values(p, v):
            rset.rdate(d)
    for p, v in get(master, "EXDATE"):
        for d in ctx.values(p, v):
            rset.exdate(d)

    until_utc = dt.datetime.strptime(case["iter_until"], "%Y%m%dT%H%M%SZ")
    out = []
    for d in rset:
        if (d.astimezone(UTC).replace(tzinfo=None) if d.tzinfo else d) > until_utc or len(out) >= case["iter_max"]:
            break
        out.append(d)

    for o in overrides:
        (rp, rv), = get(o, "RECURRENCE-ID")
        (op, ov), = get(o, "DTSTART")
        rid, new = ctx.value(rp, rv)[1], ctx.value(op, ov)[1]
        delta = new - rid
        if rp.get("RANGE") == "THISANDFUTURE":
            out = [d + delta if d >= rid else d for d in out]
        else:
            out = [d for d in out if d != rid] + [new]
    return sorted({fmt(kind, d) for d in out})


def main():
    def timeout(*_):
        raise TimeoutError("timeout 10s")
    signal.signal(signal.SIGALRM, timeout)
    res = {}
    for case in json.load(open(sys.argv[1])):
        signal.alarm(10)
        try:
            res[case["id"]] = {"ok": expand(case)}
        except Exception as e:  # every failure is a result
            res[case["id"]] = {"error": f"{type(e).__name__}: {e}"}
        signal.alarm(0)
    json.dump({"engine": "python-dateutil 2.9.0.post0", "results": res}, sys.stdout, indent=1)


if __name__ == "__main__":
    main()
