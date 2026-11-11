"""Writes oracles/icaljs/vtimezones.json: an exact VTIMEZONE (one sub-component
per transition, 1850-2120) for every IANA TZID used by the corpus, built from
the system tzdb via zoneinfo. ical.js has no tzdb of its own.
(The `icalendar` package's Timezone.from_tzid was rejected: 6.3.2 writes the
DAYLIGHT onset in the post-transition local time, one hour late.)"""
import datetime as dt, json, re, sys
from zoneinfo import ZoneInfo

UTC = dt.timezone.utc


def off(z, t):
    return t.astimezone(z).utcoffset()


def transitions(z, a=1850, b=2120):
    t, end, step = dt.datetime(a, 1, 1, tzinfo=UTC), dt.datetime(b, 1, 1, tzinfo=UTC), dt.timedelta(days=1)
    while t < end:
        n = t + step
        if off(z, t) != off(z, n):
            lo, hi = t, n
            while hi - lo > dt.timedelta(minutes=1):
                mid = lo + (hi - lo) / 2
                lo, hi = (mid, hi) if off(z, mid) == off(z, lo) else (lo, mid)
            yield hi.replace(second=0, microsecond=0)
        t = n


def fmt_off(td):
    s = int(td.total_seconds()); sign = "+" if s >= 0 else "-"; s = abs(s)
    return f"{sign}{s // 3600:02d}{s % 3600 // 60:02d}" + (f"{s % 60:02d}" if s % 60 else "")


def vtimezone(tzid):
    z = ZoneInfo(tzid)
    first = dt.datetime(1850, 1, 1, tzinfo=UTC)
    o0 = off(z, first)
    out = ["BEGIN:VTIMEZONE", f"TZID:{tzid}", "BEGIN:STANDARD",
           f"DTSTART:{(first + o0).strftime('%Y%m%dT%H%M%S')}",
           f"TZOFFSETFROM:{fmt_off(o0)}", f"TZOFFSETTO:{fmt_off(o0)}", "END:STANDARD"]
    prev = o0
    for t in transitions(z):
        new, loc = off(z, t), t.astimezone(z)
        kind = "DAYLIGHT" if loc.dst() else "STANDARD"
        out += [f"BEGIN:{kind}", f"DTSTART:{(t + prev).strftime('%Y%m%dT%H%M%S')}",
                f"TZOFFSETFROM:{fmt_off(prev)}", f"TZOFFSETTO:{fmt_off(new)}", f"END:{kind}"]
        prev = new
    return "\n".join(out + ["END:VTIMEZONE"]) + "\n"


cases = json.load(open(sys.argv[1]))
tzids = {m for c in cases for m in re.findall(r"TZID=([^:;]+)", c["ics"])} - \
        {m for c in cases for m in re.findall(r"^TZID:(.*)$", c["ics"], re.M)}
res = {}
for tzid in sorted(tzids):
    try:
        res[tzid] = vtimezone(tzid)
    except Exception as e:
        print(f"skip {tzid}: {e}", file=sys.stderr)
json.dump(res, open(sys.argv[2], "w"), indent=0)
print(f"{len(res)} zones -> {sys.argv[2]}", file=sys.stderr)
