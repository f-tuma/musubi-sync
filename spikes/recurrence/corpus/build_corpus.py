"""Builds corpus/cases.json, the shared input of every oracle and candidate.

Each case is one VCALENDAR body (VEVENTs and optional VTIMEZONEs, without the
BEGIN/END:VCALENDAR wrapper) plus how much of the series to compare:
  window=(from, to)  occurrences whose start lies in [from, to)
  limit=N            the first N occurrences (for infinite series)
  neither            the whole (finite) series
Window bounds use the same kind as the series: UTC 'YYYYMMDDTHHMMSSZ' for
zoned series, floating 'YYYYMMDDTHHMMSS' or 'YYYYMMDD' for floating/all-day.
Occurrence starts are normalised as UTC 'YYYY-MM-DDTHH:MM:SSZ', floating
'YYYY-MM-DDTHH:MM:SS' or date 'YYYY-MM-DD'. Run: python3 corpus/build_corpus.py
"""
import json, pathlib, datetime as dt

CASES = []


def case(id, *events, window=None, limit=None, tags=(), source="", note="", vtimezones=""):
    CASES.append(dict(id=id, tags=list(tags), source=source, note=note,
                      ics=vtimezones + "".join(events), window=window, limit=limit))


def ev(dtstart, *props, uid="s1", extra=""):
    lines = ["BEGIN:VEVENT", f"UID:{uid}", "DTSTAMP:20260101T000000Z", f"DTSTART{dtstart}", *props]
    return "\n".join(lines) + ("\n" + extra if extra else "") + "\nEND:VEVENT\n"


P = ";TZID=Europe/Prague:"
NY = ";TZID=America/New_York:"
MUSUBI_EXP = "musubi:packages/calendar/src/time-expansion.test.ts"
MUSUBI_GOOGLE = "musubi:apps/api/src/sync/adapters/google.ts sanitizeRecurrence"
MUSUBI_DATE = "musubi:docs/sync/caldav-date-export.md"

# --- DST: gaps and folds -------------------------------------------------
case("dst-prague-gap-daily", ev(P + "20260328T023000", "RRULE:FREQ=DAILY;COUNT=3"),
     tags=["dst", "gap", "count"], source=MUSUBI_EXP,
     note="2026-03-29 02:30 does not exist. RFC 5545 3.3.10 says ignore+do not count; 3.8.5.3 says interpret like 3.3.5 (offset before gap).")
case("dst-prague-fold-daily", ev(P + "20261024T023000", "RRULE:FREQ=DAILY;COUNT=3"),
     tags=["dst", "fold"], source=MUSUBI_EXP, note="2026-10-25 02:30 occurs twice; RFC 3.3.5: first occurrence (CEST).")
case("dst-prague-weekly-wallclock", ev(P + "20260316T090000", "RRULE:FREQ=WEEKLY;COUNT=4"),
     tags=["dst"], note="Wall clock 09:00 is kept across the March change.")
case("dst-ny-gap-daily", ev(NY + "20260307T023000", "RRULE:FREQ=DAILY;COUNT=3"), tags=["dst", "gap"])
case("dst-ny-fold-daily", ev(NY + "20261031T013000", "RRULE:FREQ=DAILY;COUNT=3"), tags=["dst", "fold"])
case("dst-lordhowe-gap-30min", ev(";TZID=Australia/Lord_Howe:20261003T021500", "RRULE:FREQ=DAILY;COUNT=3"),
     tags=["dst", "gap"], note="Lord Howe moves 30 minutes: 02:00->02:30 on 2026-10-04.")
case("dst-lordhowe-fold-30min", ev(";TZID=Australia/Lord_Howe:20260404T014500", "RRULE:FREQ=DAILY;COUNT=3"),
     tags=["dst", "fold"], note="2026-04-05 01:30-02:00 repeats.")
case("dst-apia-skipped-day", ev(";TZID=Pacific/Apia:20111228T100000", "RRULE:FREQ=DAILY;COUNT=4"),
     tags=["dst", "gap", "historic"], note="Samoa skipped 2011-12-30 entirely.")
case("dst-prague-hourly-gap", ev(P + "20260329T000000", "RRULE:FREQ=HOURLY;COUNT=5"),
     tags=["dst", "gap", "subdaily"], note="Local 02:00 does not exist; shifting it collides with 03:00.")
case("dst-prague-explicit-gap-dtstart", ev(P + "20260329T023000", "RRULE:FREQ=DAILY;COUNT=2"),
     tags=["dst", "gap"], source=MUSUBI_EXP, note="An explicit nonexistent DTSTART: RFC 3.3.5 gives 01:30Z; later days keep 02:30 local.")
case("tz-saopaulo-dst-abolished", ev(";TZID=America/Sao_Paulo:20181015T090000", "RRULE:FREQ=MONTHLY;COUNT=14"),
     tags=["historic"], note="Brazil abolished DST in 2019.")
case("tz-moscow-2011-2014", ev(";TZID=Europe/Moscow:20100115T120000", "RRULE:FREQ=YEARLY;COUNT=6"),
     tags=["historic"], note="UTC+3 -> +4 (2011) -> +3 (2014).")
case("tz-casablanca-ramadan", ev(";TZID=Africa/Casablanca:20260204T120000", "RRULE:FREQ=WEEKLY;COUNT=8"),
     tags=["historic", "tzdb-version"], note="Morocco drops to +00 during Ramadan; depends on tzdb version.")
case("tz-kathmandu-0545", ev(";TZID=Asia/Kathmandu:20260101T090000", "RRULE:FREQ=MONTHLY;COUNT=3"), tags=["tz"])
case("tz-utc-z", ev(":20260302T090000Z", "RRULE:FREQ=WEEKLY;COUNT=3"), tags=["tz"])

# --- BY rules ------------------------------------------------------------
case("bysetpos-last-weekday", ev(NY + "20260130T090000", "RRULE:FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1;COUNT=6"), tags=["bysetpos"], source="RFC 5545 3.8.5.3 example")
case("bysetpos-first-and-last", ev(NY + "20260101T090000", "RRULE:FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=1,-1;COUNT=6"), tags=["bysetpos"])
case("bysetpos-first-monday", ev(P + "20260105T090000", "RRULE:FREQ=MONTHLY;BYDAY=MO;BYSETPOS=1;COUNT=4"), tags=["bysetpos"], source="musubi tests")
case("bysetpos-yearly-weekly-mix", ev(NY + "20260308T090000", "RRULE:FREQ=YEARLY;BYMONTH=3,9;BYDAY=SU;BYSETPOS=2,-1;COUNT=6"), tags=["bysetpos"])
case("byweekno-20-monday", ev(NY + "19970512T090000", "RRULE:FREQ=YEARLY;BYWEEKNO=20;BYDAY=MO;COUNT=3"), tags=["byweekno"], source="RFC 5545 3.8.5.3 example")
case("byweekno-53", ev(NY + "19981228T090000", "RRULE:FREQ=YEARLY;BYWEEKNO=53;BYDAY=MO;COUNT=3"), tags=["byweekno"])
case("byweekno-neg1-thursday", ev(NY + "20261231T090000", "RRULE:FREQ=YEARLY;BYWEEKNO=-1;BYDAY=TH;COUNT=4"), tags=["byweekno"])
case("byweekno-1-wkst-su", ev(NY + "20260104T090000", "RRULE:FREQ=YEARLY;BYWEEKNO=1;BYDAY=SU,MO;WKST=SU;COUNT=6"), tags=["byweekno", "wkst"])
case("byweekno-1-wkst-mo", ev(NY + "20260104T090000", "RRULE:FREQ=YEARLY;BYWEEKNO=1;BYDAY=SU,MO;WKST=MO;COUNT=6"), tags=["byweekno", "wkst"])
case("byyearday-every-3rd-year", ev(NY + "19970101T090000", "RRULE:FREQ=YEARLY;INTERVAL=3;COUNT=10;BYYEARDAY=1,100,200"), tags=["byyearday"], source="RFC 5545 3.8.5.3 example")
case("byyearday-neg366", ev(NY + "20280101T090000", "RRULE:FREQ=YEARLY;BYYEARDAY=-366;COUNT=3"), tags=["byyearday", "leap"])
case("byyearday-60-leap", ev(NY + "20250301T090000", "RRULE:FREQ=YEARLY;BYYEARDAY=60;COUNT=4"), tags=["byyearday", "leap"])
case("bymonthday-neg1", ev(P + "20260131T090000", "RRULE:FREQ=MONTHLY;BYMONTHDAY=-1;COUNT=6"), tags=["bymonthday"])
case("bymonthday-neg3", ev(NY + "19970928T090000", "RRULE:FREQ=MONTHLY;BYMONTHDAY=-3;COUNT=6"), tags=["bymonthday"], source="RFC 5545 3.8.5.3 example")
case("bymonthday-1-and-neg1", ev(NY + "19970930T090000", "RRULE:FREQ=MONTHLY;COUNT=10;BYMONTHDAY=1,-1"), tags=["bymonthday"], source="RFC 5545 3.8.5.3 example")
case("bymonthday-31-skips", ev(P + "20260131T090000", "RRULE:FREQ=MONTHLY;BYMONTHDAY=31;COUNT=5"), tags=["bymonthday", "invalid-date"])
case("monthly-jan31-no-by", ev(P + "20260131T090000", "RRULE:FREQ=MONTHLY;COUNT=5"), tags=["invalid-date"], note="RFC 3.3.10: invalid dates (Feb 31) are ignored.")
case("leap-yearly-feb29", ev(P + "20240229T090000", "RRULE:FREQ=YEARLY;COUNT=3"), tags=["leap"])
case("leap-bymonth2-bymonthday-neg1", ev(P + "20250228T090000", "RRULE:FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=-1;COUNT=4"), tags=["leap"])
case("wkst-mo", ev(NY + "19970805T090000", "RRULE:FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=TU,SU;WKST=MO"), tags=["wkst"], source="RFC 5545 3.8.5.3 example")
case("wkst-su", ev(NY + "19970805T090000", "RRULE:FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=TU,SU;WKST=SU"), tags=["wkst"], source="RFC 5545 3.8.5.3 example")
case("yearly-byday-20mo", ev(NY + "19970519T090000", "RRULE:FREQ=YEARLY;BYDAY=20MO;COUNT=3"), tags=["byday"], source="RFC 5545 3.8.5.3 example")
case("yearly-last-sunday-march", ev(P + "20260329T090000", "RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU;COUNT=3"), tags=["byday"], source="musubi tests")
case("monthly-second-to-last-monday", ev(P + "20260119T090000", "RRULE:FREQ=MONTHLY;BYDAY=-2MO;COUNT=4"), tags=["byday"])
case("friday-13th", ev(NY + "19970902T090000", "EXDATE;TZID=America/New_York:19970902T090000", "RRULE:FREQ=MONTHLY;BYDAY=FR;BYMONTHDAY=13;COUNT=5"), tags=["byday", "bymonthday"], source="RFC 5545 3.8.5.3 example")
case("us-election-day", ev(NY + "19961105T090000", "RRULE:FREQ=YEARLY;INTERVAL=4;BYMONTH=11;BYDAY=TU;BYMONTHDAY=2,3,4,5,6,7,8;COUNT=3"), tags=["byday"], source="RFC 5545 3.8.5.3 example")
case("daily-byhour-interval2", ev(P + "20260401T080000", "RRULE:INTERVAL=2;FREQ=DAILY;BYHOUR=8,9;COUNT=6"), tags=["byhour"], source="musubi tests")
case("minutely-byhour", ev(NY + "19970902T090000", "RRULE:FREQ=MINUTELY;INTERVAL=20;BYHOUR=9,10;COUNT=7"), tags=["subdaily"], source="RFC 5545 3.8.5.3 example")
case("yearly-bymonthday-no-bymonth", ev(P + "20260315T090000", "RRULE:FREQ=YEARLY;BYMONTHDAY=15;COUNT=4"),
     tags=["google-quirk", "yearly"], source=MUSUBI_GOOGLE,
     note="RFC table: BYMONTHDAY expands YEARLY to every month. Google means yearly; Musubi adds BYMONTH. Engine must follow RFC; the quirk belongs to the Google adapter.")
case("yearly-byday-no-bymonth", ev(P + "20260112T090000", "RRULE:FREQ=YEARLY;BYDAY=2MO;COUNT=3"), tags=["yearly"], note="2nd Monday of the year.")
case("yearly-plain", ev(P + "20260310T090000", "RRULE:FREQ=YEARLY;COUNT=3"), tags=["yearly"])

# --- UNTIL / COUNT -------------------------------------------------------
case("until-utc-inclusive", ev(P + "20260325T090000", "RRULE:FREQ=DAILY;UNTIL=20260330T070000Z"), tags=["until", "dst"], source="musubi tests",
     note="Last instance 2026-03-30 09:00 CEST == 07:00Z is included.")
case("until-local-no-z", ev(P + "20260330T090000", "RRULE:FREQ=DAILY;UNTIL=20260403T090000"), tags=["until", "invalid"], source="musubi tests",
     note="RFC: UNTIL MUST be UTC when DTSTART has TZID. Lenient readers interpret it in the DTSTART zone.")
case("until-date-with-date", ev(";VALUE=DATE:20260325", "RRULE:FREQ=DAILY;UNTIL=20260331"), tags=["until", "all-day"], source="musubi tests")
case("until-date-with-datetime", ev(P + "20260330T090000", "RRULE:FREQ=DAILY;UNTIL=20260403"), tags=["until", "invalid"], source="musubi tests",
     note="RFC: UNTIL value type MUST match DTSTART. Behaviour is implementation defined.")
case("until-between-instances", ev(P + "20260302T090000", "RRULE:FREQ=WEEKLY;UNTIL=20260320T000000Z"), tags=["until"])
case("until-before-dtstart", ev(P + "20260302T090000", "RRULE:FREQ=DAILY;UNTIL=20260301T000000Z"), tags=["until", "dtstart"],
     note="RFC 3.8.5.3: DTSTART always counts as the first occurrence.")
case("dtstart-not-in-rule", ev(P + "20260310T090000", "RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=3"), tags=["dtstart", "count"],
     note="RFC: DTSTART counts as the first instance, but an unsynchronised set is 'undefined'. dateutil drops DTSTART.")
case("count-exdate-no-replenish", ev(P + "20260401T090000", "RRULE:FREQ=DAILY;COUNT=4", "EXDATE;TZID=Europe/Prague:20260402T090000"), tags=["count", "exdate"], source=MUSUBI_DATE)
case("count-1", ev(P + "20260401T090000", "RRULE:FREQ=DAILY;COUNT=1"), tags=["count"])

# --- all-day and floating ------------------------------------------------
case("allday-daily-exdate-date", ev(";VALUE=DATE:20260327", "RRULE:FREQ=DAILY;COUNT=5", "EXDATE;VALUE=DATE:20260328,20260330"), tags=["all-day", "exdate"], source="musubi tests (Google all-day EXDATE)")
case("allday-weekly-until-date", ev(";VALUE=DATE:20260302", "RRULE:FREQ=WEEKLY;UNTIL=20260330"), tags=["all-day", "until"])
case("allday-rdate-date", ev(";VALUE=DATE:20260301", "RRULE:FREQ=MONTHLY;COUNT=2", "RDATE;VALUE=DATE:20260415"), tags=["all-day", "rdate"], source="musubi:docs/sync/caldav-rdate-writes.md")
case("allday-exdate-dtstart", ev(";VALUE=DATE:20260327", "RRULE:FREQ=DAILY;COUNT=3", "EXDATE;VALUE=DATE:20260327"), tags=["all-day", "exdate"], source=MUSUBI_DATE, note="An excluded DTSTART stays excluded.")
case("allday-yearly-feb29", ev(";VALUE=DATE:20240229", "RRULE:FREQ=YEARLY;COUNT=2"), tags=["all-day", "leap"])
case("floating-daily-host-gap", ev(":20260328T023000", "RRULE:FREQ=DAILY;COUNT=3"), tags=["floating", "gap"],
     note="Floating time: 02:30 every day regardless of any zone (run under TZ=Europe/Prague to expose host-zone leaks).")
case("floating-exdate", ev(":20260401T090000", "RRULE:FREQ=DAILY;COUNT=3", "EXDATE:20260402T090000"), tags=["floating", "exdate"])

# --- EXDATE / RDATE ------------------------------------------------------
case("google-exdate-tzid", ev(NY + "20260302T100000", "RRULE:FREQ=WEEKLY;COUNT=4", "EXDATE;TZID=America/New_York:20260316T100000"), tags=["exdate", "google-quirk"], source=MUSUBI_GOOGLE, note="Musubi found JS rrule silently ignores EXDATE;TZID.")
case("exdate-utc-for-zoned", ev(NY + "20260302T100000", "RRULE:FREQ=WEEKLY;COUNT=4", "EXDATE:20260316T140000Z"), tags=["exdate"])
case("exdate-other-zone", ev(NY + "20260302T100000", "RRULE:FREQ=WEEKLY;COUNT=4", "EXDATE;TZID=Europe/London:20260316T140000"), tags=["exdate"])
case("exdate-nonmatching", ev(NY + "20260302T100000", "RRULE:FREQ=WEEKLY;COUNT=3", "EXDATE;TZID=America/New_York:20260309T110000"), tags=["exdate"])
case("exdate-multi-lines", ev(P + "20260401T090000", "RRULE:FREQ=DAILY;COUNT=6", "EXDATE;TZID=Europe/Prague:20260401T090000,20260403T090000", "EXDATE;TZID=Europe/Prague:20260405T090000"), tags=["exdate"])
case("exdate-date-on-timed", ev(P + "20260401T090000", "RRULE:FREQ=DAILY;COUNT=3", "EXDATE;VALUE=DATE:20260402"), tags=["exdate", "invalid"],
     note="EXDATE DATE on a DATE-TIME series is not defined by RFC 5545; some readers exclude the whole day.")
case("rdate-tzid-and-duplicate", ev(P + "20260401T090000", "RRULE:FREQ=DAILY;COUNT=2", "RDATE;TZID=Europe/Prague:20260402T090000,20260410T150000"), tags=["rdate"], note="The recurrence set is a set: a duplicate RDATE is one instance.")
case("rdate-period", ev(P + "20260401T090000", "RRULE:FREQ=DAILY;COUNT=2", "RDATE;VALUE=PERIOD:20260410T080000Z/20260410T100000Z,20260411T080000Z/PT2H"), tags=["rdate", "period"])
case("rdate-before-dtstart", ev(P + "20260401T090000", "RRULE:FREQ=DAILY;COUNT=2", "RDATE;TZID=Europe/Prague:20260320T090000"), tags=["rdate"])
case("rdate-only", ev(P + "20260401T090000", "RDATE;TZID=Europe/Prague:20260405T090000,20260407T090000"), tags=["rdate"])
case("rdate-gap-and-fold", ev(P + "20261024T023000", "RRULE:FREQ=DAILY;COUNT=1", "RDATE:20261025T013000Z", "RDATE;TZID=Europe/Prague:20261025T023000"), tags=["rdate", "fold"], source=MUSUBI_EXP,
     note="Both fold instants: 00:30Z (local, first) and 01:30Z (explicit UTC).")
case("exrule", ev(P + "20260401T090000", "RRULE:FREQ=DAILY;COUNT=7", "EXRULE:FREQ=WEEKLY;BYDAY=SA,SU"), tags=["exrule"], note="EXRULE is deprecated (RFC 2445 only).")

# --- overrides (RECURRENCE-ID) -----------------------------------------------
case("override-moved", ev(P + "20260406T090000", "RRULE:FREQ=WEEKLY;COUNT=4"),
     ev(P + "20260414T110000", "RECURRENCE-ID;TZID=Europe/Prague:20260413T090000"), tags=["override"])
case("override-rid-utc", ev(P + "20260406T090000", "RRULE:FREQ=WEEKLY;COUNT=4"),
     ev(P + "20260414T110000", "RECURRENCE-ID:20260413T070000Z"), tags=["override"], note="RECURRENCE-ID in another zone still names the same instant.")
case("override-of-rdate", ev(P + "20260406T090000", "RRULE:FREQ=WEEKLY;COUNT=2", "RDATE;TZID=Europe/Prague:20260420T090000"),
     ev(P + "20260421T090000", "RECURRENCE-ID;TZID=Europe/Prague:20260420T090000"), tags=["override", "rdate"])
case("override-allday", ev(";VALUE=DATE:20260406", "RRULE:FREQ=DAILY;COUNT=3"),
     ev(";VALUE=DATE:20260410", "RECURRENCE-ID;VALUE=DATE:20260407"), tags=["override", "all-day"])
case("override-thisandfuture", ev(P + "20260406T090000", "RRULE:FREQ=DAILY;COUNT=5"),
     ev(P + "20260408T100000", "RECURRENCE-ID;RANGE=THISANDFUTURE;TZID=Europe/Prague:20260408T090000"), tags=["override", "thisandfuture"],
     note="RFC 5545 3.8.4.4/3.2.13: the +1h shift applies to this and all later instances.")
case("override-moved-into-window", ev(P + "20260406T090000", "RRULE:FREQ=WEEKLY;COUNT=10"),
     ev(P + "20260501T090000", "RECURRENCE-ID;TZID=Europe/Prague:20260601T090000"),
     window=("20260427T000000Z", "20260504T000000Z"), tags=["override", "window"], note="Instance from June moved into the window.")

# --- infinite / windowed -------------------------------------------------
case("infinite-daily-window", ev(P + "20000101T090000", "RRULE:FREQ=DAILY"), window=("20300101T000000Z", "20300105T000000Z"), tags=["infinite", "window"])
case("count-reconstructed-in-window", ev(P + "20260101T090000", "RRULE:FREQ=DAILY;COUNT=1000"), window=("20280920T000000Z", "20281001T000000Z"), tags=["count", "window"],
     note="COUNT must be counted from DTSTART, not from the window.")
case("infinite-leap-far-future", ev(P + "20240229T090000", "RRULE:FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=29"), window=("20950101T000000Z", "21100101T000000Z"), tags=["infinite", "leap", "window"], note="2100 is not a leap year.")
case("infinite-limit-monthly-31", ev(P + "20260131T090000", "RRULE:FREQ=MONTHLY;BYMONTHDAY=31"), limit=10, tags=["infinite", "limit"])
case("impossible-feb30", ev(P + "20260101T090000", "RRULE:FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=30"), window=("20260101T000000Z", "20300101T000000Z"), tags=["infinite", "impossible"],
     note="No instance ever; an engine must terminate (only DTSTART, if DTSTART counts).")
case("infinite-daily-since-1900", ev(";TZID=Europe/Prague:19000101T090000", "RRULE:FREQ=DAILY"), window=("20260601T000000Z", "20260603T000000Z"), tags=["infinite", "window", "historic"],
     note="Prague used LMT until 1891 and CET/CEST later; 1900 start exercises historic offsets.")

# --- non-IANA VTIMEZONE --------------------------------------------------
WEST = """BEGIN:VTIMEZONE
TZID:W. Europe Standard Time
BEGIN:STANDARD
DTSTART:16010101T030000
TZOFFSETFROM:+0200
TZOFFSETTO:+0100
RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=-1SU;BYMONTH=10
END:STANDARD
BEGIN:DAYLIGHT
DTSTART:16010101T020000
TZOFFSETFROM:+0100
TZOFFSETTO:+0200
RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=-1SU;BYMONTH=3
END:DAYLIGHT
END:VTIMEZONE
"""
CUSTOM = """BEGIN:VTIMEZONE
TZID:Musubi Test Zone
BEGIN:STANDARD
DTSTART:19700101T030000
TZOFFSETFROM:+0230
TZOFFSETTO:+0130
RRULE:FREQ=YEARLY;BYDAY=1SU;BYMONTH=11
END:STANDARD
BEGIN:DAYLIGHT
DTSTART:19700101T020000
TZOFFSETFROM:+0130
TZOFFSETTO:+0230
RRULE:FREQ=YEARLY;BYDAY=2SU;BYMONTH=3
END:DAYLIGHT
END:VTIMEZONE
"""
case("vtimezone-windows-name", ev(";TZID=W. Europe Standard Time:20260316T090000", "RRULE:FREQ=WEEKLY;COUNT=3"), vtimezones=WEST,
     tags=["vtimezone"], note="Outlook/Exchange TZID; equivalent to Europe/Berlin since 1996.")
case("vtimezone-custom-rules", ev(";TZID=Musubi Test Zone:20260306T090000", "RRULE:FREQ=WEEKLY;COUNT=3"), vtimezones=CUSTOM,
     tags=["vtimezone"], note="Zone that exists nowhere in IANA: +01:30 / +02:30, DST from 2nd Sunday of March.")
case("vtimezone-mozilla-prefix", ev(";TZID=/mozilla.org/20050126_1/Europe/Prague:20260316T090000", "RRULE:FREQ=WEEKLY;COUNT=3"),
     tags=["vtimezone"], note="Old Lightning TZID without VTIMEZONE; must map by suffix or fail loudly.")


def with_bounds(c):
    # adapters stop iterating after iter_until (window end + 2 days) or iter_max starts
    if c["window"]:
        end = c["window"][1]
        fmt = "%Y%m%d" if len(end) == 8 else ("%Y%m%dT%H%M%SZ" if end.endswith("Z") else "%Y%m%dT%H%M%S")
        c["iter_until"] = (dt.datetime.strptime(end, fmt) + dt.timedelta(days=2)).strftime("%Y%m%dT%H%M%SZ")
    else:
        c["iter_until"] = "99991231T000000Z"
    c["iter_max"] = c["limit"] + 50 if c["limit"] else (200000 if c["window"] else 5000)
    return c


if __name__ == "__main__":
    ids = [c["id"] for c in CASES]
    assert len(ids) == len(set(ids)), "duplicate case id"
    out = pathlib.Path(__file__).with_name("cases.json")
    out.write_text(json.dumps([with_bounds(c) for c in CASES], indent=1) + "\n")
    print(f"{len(CASES)} cases -> {out}")
