- `calcard-next`: calcard-next 3.0.1 (crates.io, jiff), ICalendar::expand_dates_between
- `calcard`: calcard 0.3.14 (crates.io, chrono-tz), ICalendar::expand_dates
- `dateutil`: python-dateutil 2.9.0.post0
- `icaljs`: ical.js 2.2.1
- `rrule-jiff`: rrule jiff port, ccope/rust-rrule@50db3e2 (closed PR #151)
- `rrule`: rrule 0.14.0 (crates.io, chrono-tz)
- `rrule2`: rrule2 0.15.0 (crates.io, chrono-tz)

| case | calcard-next | calcard | dateutil | icaljs | rrule-jiff | rrule | rrule2 |
|---|---|---|---|---|---|---|---|
| dst-prague-gap-daily | pass | FAIL | pass | FAIL | pass | pass | pass |
| dst-prague-fold-daily | pass | FAIL | pass | FAIL | pass | pass | pass |
| dst-prague-weekly-wallclock | pass | pass | pass | pass | pass | pass | pass |
| dst-ny-gap-daily | pass | FAIL | pass | FAIL | pass | pass | pass |
| dst-ny-fold-daily | pass | FAIL | pass | FAIL | pass | pass | pass |
| dst-lordhowe-gap-30min | pass | FAIL | pass | FAIL | pass | pass | pass |
| dst-lordhowe-fold-30min | pass | FAIL | pass | FAIL | pass | pass | pass |
| dst-apia-skipped-day | pass | FAIL | pass | pass | pass | pass | pass |
| dst-prague-hourly-gap | pass | FAIL | pass | pass | pass | pass | pass |
| dst-prague-explicit-gap-dtstart | pass | ERR | pass | FAIL | FAIL | ERR | FAIL |
| tz-saopaulo-dst-abolished | pass | pass | pass | pass | pass | pass | pass |
| tz-moscow-2011-2014 | pass | pass | pass | pass | pass | pass | pass |
| tz-casablanca-ramadan | pass | pass | pass | pass | pass | pass | pass |
| tz-kathmandu-0545 | pass | pass | pass | pass | pass | pass | pass |
| tz-utc-z | pass | FAIL | pass | pass | pass | pass | pass |
| bysetpos-last-weekday | pass | pass | pass | pass | pass | pass | pass |
| bysetpos-first-and-last | pass | pass | pass | pass | pass | pass | pass |
| bysetpos-first-monday | pass | pass | pass | pass | pass | pass | pass |
| bysetpos-yearly-weekly-mix | pass | pass | pass | FAIL | pass | pass | pass |
| byweekno-20-monday | pass | pass | pass | FAIL | pass | pass | pass |
| byweekno-53 | pass | pass | pass | FAIL | pass | pass | pass |
| byweekno-neg1-thursday | pass | pass | pass | FAIL | pass | pass | pass |
| byweekno-1-wkst-su | pass | pass | pass | FAIL | pass | pass | pass |
| byweekno-1-wkst-mo | pass | pass | pass | FAIL | pass | pass | pass |
| byyearday-every-3rd-year | pass | pass | pass | pass | pass | pass | pass |
| byyearday-neg366 | pass | pass | pass | pass | pass | pass | pass |
| byyearday-60-leap | pass | pass | pass | pass | pass | pass | pass |
| bymonthday-neg1 | pass | pass | pass | pass | pass | pass | pass |
| bymonthday-neg3 | pass | pass | pass | pass | pass | pass | pass |
| bymonthday-1-and-neg1 | pass | pass | pass | pass | pass | pass | pass |
| bymonthday-31-skips | pass | pass | pass | pass | pass | pass | pass |
| monthly-jan31-no-by | pass | pass | pass | pass | pass | pass | pass |
| leap-yearly-feb29 | pass | pass | pass | FAIL | pass | pass | pass |
| leap-bymonth2-bymonthday-neg1 | pass | pass | pass | pass | pass | pass | pass |
| wkst-mo | pass | pass | pass | pass | pass | pass | pass |
| wkst-su | pass | pass | pass | pass | pass | pass | pass |
| yearly-byday-20mo | pass | pass | pass | FAIL | pass | pass | pass |
| yearly-last-sunday-march | pass | pass | pass | pass | pass | pass | pass |
| monthly-second-to-last-monday | pass | pass | pass | pass | pass | pass | pass |
| friday-13th | pass | pass | pass | pass | pass | pass | pass |
| us-election-day | pass | pass | pass | pass | pass | pass | pass |
| daily-byhour-interval2 | pass | pass | pass | pass | pass | pass | pass |
| minutely-byhour | pass | pass | pass | pass | pass | pass | pass |
| yearly-bymonthday-no-bymonth | pass | pass | pass | FAIL | pass | pass | pass |
| yearly-byday-no-bymonth | pass | pass | pass | pass | pass | pass | pass |
| yearly-plain | pass | pass | pass | pass | pass | pass | pass |
| until-utc-inclusive | pass | FAIL | pass | pass | pass | pass | pass |
| until-local-no-z | pass | pass | pass | pass | pass | pass | pass |
| until-date-with-date | pass | pass | pass | pass | pass | pass | pass |
| until-date-with-datetime | pass | pass | pass | pass | pass | pass | pass |
| until-between-instances | pass | pass | pass | pass | pass | pass | pass |
| until-before-dtstart | ERR | ERR | pass | FAIL | ERR | ERR | ERR |
| dtstart-not-in-rule | pass | pass | pass | pass | pass | pass | pass |
| count-exdate-no-replenish | pass | pass | pass | pass | pass | pass | pass |
| count-1 | pass | pass | pass | pass | pass | pass | pass |
| allday-daily-exdate-date | pass | pass | pass | pass | pass | pass | pass |
| allday-weekly-until-date | pass | pass | pass | pass | pass | pass | pass |
| allday-rdate-date | pass | pass | pass | pass | pass | pass | pass |
| allday-exdate-dtstart | pass | pass | pass | pass | pass | pass | pass |
| allday-yearly-feb29 | pass | pass | pass | FAIL | pass | pass | pass |
| floating-daily-host-gap | pass | pass | pass | pass | pass | FAIL | FAIL |
| floating-exdate | pass | pass | pass | pass | pass | pass | pass |
| google-exdate-tzid | pass | pass | pass | pass | pass | pass | pass |
| exdate-utc-for-zoned | pass | pass | pass | pass | pass | pass | pass |
| exdate-other-zone | pass | pass | pass | pass | pass | pass | pass |
| exdate-nonmatching | pass | pass | pass | pass | pass | pass | pass |
| exdate-multi-lines | pass | pass | pass | pass | pass | pass | pass |
| exdate-date-on-timed | FAIL | FAIL | pass | pass | FAIL | FAIL | FAIL |
| rdate-tzid-and-duplicate | pass | pass | pass | pass | pass | pass | pass |
| rdate-period | pass | pass | pass | ERR | ERR | ERR | ERR |
| rdate-before-dtstart | pass | pass | pass | pass | pass | pass | pass |
| rdate-only | pass | pass | pass | FAIL | FAIL | FAIL | FAIL |
| rdate-gap-and-fold | pass | FAIL | pass | FAIL | pass | ERR | pass |
| exrule | FAIL | FAIL | pass | FAIL | pass | pass | pass |
| override-moved | pass | pass | pass | pass | pass | pass | pass |
| override-rid-utc | pass | pass | pass | pass | pass | pass | pass |
| override-of-rdate | pass | FAIL | pass | pass | pass | pass | pass |
| override-allday | pass | pass | pass | pass | pass | pass | pass |
| override-thisandfuture | pass | pass | pass | pass | pass | pass | pass |
| override-moved-into-window | pass | pass | pass | pass | pass | pass | pass |
| infinite-daily-window | pass | pass | pass | pass | pass | pass | pass |
| count-reconstructed-in-window | pass | pass | pass | pass | pass | pass | pass |
| infinite-leap-far-future | pass | pass | pass | pass | pass | pass | pass |
| infinite-limit-monthly-31 | pass | pass | pass | pass | pass | pass | pass |
| impossible-feb30 | pass | pass | pass | FAIL | pass | pass | pass |
| infinite-daily-since-1900 | pass | pass | pass | pass | pass | pass | pass |
| vtimezone-windows-name | pass | pass | pass | pass | ERR | ERR | ERR |
| vtimezone-custom-rules | FAIL | FAIL | pass | pass | ERR | ERR | ERR |
| vtimezone-mozilla-prefix | pass | pass | pass | pass | pass | pass | pass |
| total | 85 pass / 3 fail / 1 err | 72 pass / 15 fail / 2 err | 89 pass / 0 fail / 0 err | 66 pass / 22 fail / 1 err | 82 pass / 3 fail / 4 err | 80 pass / 3 fail / 6 err | 81 pass / 4 fail / 4 err |

### Failures

- **dst-prague-gap-daily** / `calcard`: got `['2026-03-28T01:30:00Z', '2026-03-29T02:30:00', '2026-03-30T00:30:00Z']`; accepted `[['2026-03-28T01:30:00Z', '2026-03-29T01:30:00Z', '2026-03-30T00:30:00Z'], ['2026-03-28T01:30:00Z', '2026-03-30T00:30:00Z', '2026-03-31T00:30:00Z']]`
- **dst-prague-gap-daily** / `icaljs`: got `['2026-03-28T01:30:00Z', '2026-03-29T00:30:00Z', '2026-03-30T00:30:00Z']`; accepted `[['2026-03-28T01:30:00Z', '2026-03-29T01:30:00Z', '2026-03-30T00:30:00Z'], ['2026-03-28T01:30:00Z', '2026-03-30T00:30:00Z', '2026-03-31T00:30:00Z']]`
- **dst-prague-fold-daily** / `calcard`: got `['2026-10-24T00:30:00Z', '2026-10-25T02:30:00', '2026-10-26T01:30:00Z']`; accepted `[['2026-10-24T00:30:00Z', '2026-10-25T00:30:00Z', '2026-10-26T01:30:00Z']]`
- **dst-prague-fold-daily** / `icaljs`: got `['2026-10-24T00:30:00Z', '2026-10-25T01:30:00Z', '2026-10-26T01:30:00Z']`; accepted `[['2026-10-24T00:30:00Z', '2026-10-25T00:30:00Z', '2026-10-26T01:30:00Z']]`
- **dst-ny-gap-daily** / `calcard`: got `['2026-03-07T07:30:00Z', '2026-03-08T02:30:00', '2026-03-09T06:30:00Z']`; accepted `[['2026-03-07T07:30:00Z', '2026-03-08T07:30:00Z', '2026-03-09T06:30:00Z'], ['2026-03-07T07:30:00Z', '2026-03-09T06:30:00Z', '2026-03-10T06:30:00Z']]`
- **dst-ny-gap-daily** / `icaljs`: got `['2026-03-07T07:30:00Z', '2026-03-08T06:30:00Z', '2026-03-09T06:30:00Z']`; accepted `[['2026-03-07T07:30:00Z', '2026-03-08T07:30:00Z', '2026-03-09T06:30:00Z'], ['2026-03-07T07:30:00Z', '2026-03-09T06:30:00Z', '2026-03-10T06:30:00Z']]`
- **dst-ny-fold-daily** / `calcard`: got `['2026-10-31T05:30:00Z', '2026-11-01T01:30:00', '2026-11-02T06:30:00Z']`; accepted `[['2026-10-31T05:30:00Z', '2026-11-01T05:30:00Z', '2026-11-02T06:30:00Z']]`
- **dst-ny-fold-daily** / `icaljs`: got `['2026-10-31T05:30:00Z', '2026-11-01T06:30:00Z', '2026-11-02T06:30:00Z']`; accepted `[['2026-10-31T05:30:00Z', '2026-11-01T05:30:00Z', '2026-11-02T06:30:00Z']]`
- **dst-lordhowe-gap-30min** / `calcard`: got `['2026-10-02T15:45:00Z', '2026-10-04T02:15:00', '2026-10-04T15:15:00Z']`; accepted `[['2026-10-02T15:45:00Z', '2026-10-03T15:45:00Z', '2026-10-04T15:15:00Z'], ['2026-10-02T15:45:00Z', '2026-10-04T15:15:00Z', '2026-10-05T15:15:00Z']]`
- **dst-lordhowe-gap-30min** / `icaljs`: got `['2026-10-02T15:45:00Z', '2026-10-03T15:15:00Z', '2026-10-04T15:15:00Z']`; accepted `[['2026-10-02T15:45:00Z', '2026-10-03T15:45:00Z', '2026-10-04T15:15:00Z'], ['2026-10-02T15:45:00Z', '2026-10-04T15:15:00Z', '2026-10-05T15:15:00Z']]`
- **dst-lordhowe-fold-30min** / `calcard`: got `['2026-04-03T14:45:00Z', '2026-04-05T01:45:00', '2026-04-05T15:15:00Z']`; accepted `[['2026-04-03T14:45:00Z', '2026-04-04T14:45:00Z', '2026-04-05T15:15:00Z']]`
- **dst-lordhowe-fold-30min** / `icaljs`: got `['2026-04-03T14:45:00Z', '2026-04-04T15:15:00Z', '2026-04-05T15:15:00Z']`; accepted `[['2026-04-03T14:45:00Z', '2026-04-04T14:45:00Z', '2026-04-05T15:15:00Z']]`
- **dst-apia-skipped-day** / `calcard`: got `['2011-12-28T20:00:00Z', '2011-12-29T20:00:00Z', '2011-12-30T10:00:00', '2011-12-30T20:00:00Z']`; accepted `[['2011-12-28T20:00:00Z', '2011-12-29T20:00:00Z', '2011-12-30T20:00:00Z'], ['2011-12-28T20:00:00Z', '2011-12-29T20:00:00Z', '2011-12-30T20:00:00Z', '2011-12-31T20:00:00Z']]`
- **dst-prague-hourly-gap** / `calcard`: got `['2026-03-28T23:00:00Z', '2026-03-29T00:00:00Z', '2026-03-29T01:00:00Z', '2026-03-29T02:00:00', '2026-03-29T02:00:00Z']`; accepted `[['2026-03-28T23:00:00Z', '2026-03-29T00:00:00Z', '2026-03-29T01:00:00Z', '2026-03-29T02:00:00Z'], ['2026-03-28T23:00:00Z', '2026-03-29T00:00:00Z', '2026-03-29T01:00:00Z', '2026-03-29T02:00:00Z', '2026-03-29T03:00:00Z']]`
- **dst-prague-explicit-gap-dtstart** / `calcard`: got `[CalendarError { comp_id: 1, error: InvalidDtStart }]`; accepted `[['2026-03-29T01:30:00Z', '2026-03-30T00:30:00Z']]`
- **dst-prague-explicit-gap-dtstart** / `icaljs`: got `['2026-03-29T00:30:00Z', '2026-03-30T00:30:00Z']`; accepted `[['2026-03-29T01:30:00Z', '2026-03-30T00:30:00Z']]`
- **dst-prague-explicit-gap-dtstart** / `rrule-jiff`: got `['2026-03-29T01:30:00Z', '2026-03-30T01:30:00Z']`; accepted `[['2026-03-29T01:30:00Z', '2026-03-30T00:30:00Z']]`
- **dst-prague-explicit-gap-dtstart** / `rrule`: got `RRule parsing error: DTSTART:20260329T023000 is not a valid datetime in local timezone.`; accepted `[['2026-03-29T01:30:00Z', '2026-03-30T00:30:00Z']]`
- **dst-prague-explicit-gap-dtstart** / `rrule2`: got `['2026-03-29T01:30:00Z', '2026-03-30T01:30:00Z']`; accepted `[['2026-03-29T01:30:00Z', '2026-03-30T00:30:00Z']]`
- **tz-utc-z** / `calcard`: got `['2026-03-02T09:00:00', '2026-03-09T09:00:00', '2026-03-16T09:00:00']`; accepted `[['2026-03-02T09:00:00Z', '2026-03-09T09:00:00Z', '2026-03-16T09:00:00Z']]`
- **bysetpos-yearly-weekly-mix** / `icaljs`: got `['2026-03-08T13:00:00Z', '2026-03-29T13:00:00Z', '2026-09-13T13:00:00Z', '2026-09-27T13:00:00Z', '2027-03-14T13:00:00Z', '2027-03-28T13:00:00Z']`; accepted `[['2026-03-08T13:00:00Z', '2026-09-27T13:00:00Z', '2027-03-14T13:00:00Z', '2027-09-26T13:00:00Z', '2028-03-12T13:00:00Z', '2028-09-24T13:00:00Z']]`
- **byweekno-20-monday** / `icaljs`: got `['1997-05-19T13:00:00Z', '1997-05-26T13:00:00Z', '1997-06-02T13:00:00Z']`; accepted `[['1997-05-12T13:00:00Z', '1998-05-11T13:00:00Z', '1999-05-17T13:00:00Z']]`
- **byweekno-53** / `icaljs`: got `['1999-01-04T14:00:00Z', '1999-01-11T14:00:00Z', '1999-01-18T14:00:00Z']`; accepted `[['1998-12-28T14:00:00Z', '2004-12-27T14:00:00Z', '2009-12-28T14:00:00Z']]`
- **byweekno-neg1-thursday** / `icaljs`: got `['2026-12-31T14:00:00Z', '2027-01-07T14:00:00Z', '2027-01-14T14:00:00Z', '2027-01-21T14:00:00Z']`; accepted `[['2026-12-31T14:00:00Z', '2027-12-30T14:00:00Z', '2028-12-28T14:00:00Z', '2029-12-27T14:00:00Z']]`
- **byweekno-1-wkst-su** / `icaljs`: got `['2026-01-04T14:00:00Z', '2026-01-05T14:00:00Z', '2026-01-11T14:00:00Z', '2026-01-12T14:00:00Z', '2026-01-18T14:00:00Z', '2026-01-19T14:00:00Z']`; accepted `[['2026-01-04T14:00:00Z', '2026-01-05T14:00:00Z', '2027-01-03T14:00:00Z', '2027-01-04T14:00:00Z', '2028-01-02T14:00:00Z', '2028-01-03T14:00:00Z']]`
- **byweekno-1-wkst-mo** / `icaljs`: got `['2026-01-05T14:00:00Z', '2026-01-11T14:00:00Z', '2026-01-12T14:00:00Z', '2026-01-18T14:00:00Z', '2026-01-19T14:00:00Z', '2026-01-25T14:00:00Z']`; accepted `[['2026-01-04T14:00:00Z', '2027-01-04T14:00:00Z', '2027-01-10T14:00:00Z', '2028-01-03T14:00:00Z', '2028-01-09T14:00:00Z', '2029-01-01T14:00:00Z']]`
- **leap-yearly-feb29** / `icaljs`: got `['2024-02-29T08:00:00Z', '2025-03-01T08:00:00Z', '2026-03-01T08:00:00Z']`; accepted `[['2024-02-29T08:00:00Z', '2028-02-29T08:00:00Z', '2032-02-29T08:00:00Z']]`
- **yearly-byday-20mo** / `icaljs`: got `['1997-05-19T13:00:00Z', '1997-05-26T13:00:00Z', '1997-06-02T13:00:00Z']`; accepted `[['1997-05-19T13:00:00Z', '1998-05-18T13:00:00Z', '1999-05-17T13:00:00Z']]`
- **yearly-bymonthday-no-bymonth** / `icaljs`: got `['2026-03-15T08:00:00Z', '2027-03-15T08:00:00Z', '2028-03-15T08:00:00Z', '2029-03-15T08:00:00Z']`; accepted `[['2026-03-15T08:00:00Z', '2026-04-15T07:00:00Z', '2026-05-15T07:00:00Z', '2026-06-15T07:00:00Z']]`
- **until-utc-inclusive** / `calcard`: got `['2026-03-25T08:00:00Z', '2026-03-26T08:00:00Z', '2026-03-27T08:00:00Z', '2026-03-28T08:00:00Z', '2026-03-29T07:00:00Z']`; accepted `[['2026-03-25T08:00:00Z', '2026-03-26T08:00:00Z', '2026-03-27T08:00:00Z', '2026-03-28T08:00:00Z', '2026-03-29T07:00:00Z', '2026-03-30T07:00:00Z']]`
- **until-before-dtstart** / `calcard-next`: got `[CalendarError { comp_id: 1, error: RRule(ValidationError(UntilBeforeStart { until: "2026-03-01T01:00:00", dt_start: "2026-03-02T09:00:00" })) }]`; accepted `[['2026-03-02T08:00:00Z']]`
- **until-before-dtstart** / `calcard`: got `[CalendarError { comp_id: 1, error: RRule(ValidationError(UntilBeforeStart { until: "2026-03-01T00:00:00+00:00", dt_start: "2026-03-02T09:00:00+00:00" })) }]`; accepted `[['2026-03-02T08:00:00Z']]`
- **until-before-dtstart** / `icaljs`: got `[]`; accepted `[['2026-03-02T08:00:00Z']]`
- **until-before-dtstart** / `rrule-jiff`: got `RRule validation error: `UNTIL` is `2026-03-01T00:00:00+00:00`, but `DTSTART` (`2026-03-02T09:00:00+01:00`) is later. That should not be happening.`; accepted `[['2026-03-02T08:00:00Z']]`
- **until-before-dtstart** / `rrule`: got `RRule validation error: `UNTIL` is `2026-03-01T00:00:00+00:00`, but `DTSTART` (`2026-03-02T09:00:00+01:00`) is later. That should not be happening.`; accepted `[['2026-03-02T08:00:00Z']]`
- **until-before-dtstart** / `rrule2`: got `RRule validation error: `UNTIL` is `2026-03-01T00:00:00+00:00`, but `DTSTART` (`2026-03-02T09:00:00+01:00`) is later. That should not be happening.`; accepted `[['2026-03-02T08:00:00Z']]`
- **allday-yearly-feb29** / `icaljs`: got `['2024-02-29', '2025-03-01']`; accepted `[['2024-02-29', '2028-02-29']]`
- **floating-daily-host-gap** / `rrule`: got `['2026-03-28T02:30:00', '2026-03-29T03:30:00', '2026-03-30T02:30:00']`; accepted `[['2026-03-28T02:30:00', '2026-03-29T02:30:00', '2026-03-30T02:30:00']]`
- **floating-daily-host-gap** / `rrule2`: got `['2026-03-28T02:30:00', '2026-03-29T03:30:00', '2026-03-30T02:30:00']`; accepted `[['2026-03-28T02:30:00', '2026-03-29T02:30:00', '2026-03-30T02:30:00']]`
- **exdate-date-on-timed** / `calcard-next`: got `['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-03T07:00:00Z']`; accepted `[['2026-04-01T07:00:00Z', '2026-04-03T07:00:00Z']]`
- **exdate-date-on-timed** / `calcard`: got `['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-03T07:00:00Z']`; accepted `[['2026-04-01T07:00:00Z', '2026-04-03T07:00:00Z']]`
- **exdate-date-on-timed** / `rrule-jiff`: got `['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-03T07:00:00Z']`; accepted `[['2026-04-01T07:00:00Z', '2026-04-03T07:00:00Z']]`
- **exdate-date-on-timed** / `rrule`: got `['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-03T07:00:00Z']`; accepted `[['2026-04-01T07:00:00Z', '2026-04-03T07:00:00Z']]`
- **exdate-date-on-timed** / `rrule2`: got `['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-03T07:00:00Z']`; accepted `[['2026-04-01T07:00:00Z', '2026-04-03T07:00:00Z']]`
- **rdate-period** / `icaljs`: got `occurrence.convertToZone is not a function`; accepted `[['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-10T08:00:00Z', '2026-04-11T08:00:00Z']]`
- **rdate-period** / `rrule-jiff`: got `RRule parsing error: `20260410T080000Z/20260410T100000Z` is not a valid datetime format for `RDATE`.`; accepted `[['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-10T08:00:00Z', '2026-04-11T08:00:00Z']]`
- **rdate-period** / `rrule`: got `RRule parsing error: `20260410T080000Z/20260410T100000Z` is not a valid datetime format for `RDATE`.`; accepted `[['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-10T08:00:00Z', '2026-04-11T08:00:00Z']]`
- **rdate-period** / `rrule2`: got `RRule parsing error: `20260410T080000Z/20260410T100000Z` is not a valid datetime format for `RDATE`.`; accepted `[['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-10T08:00:00Z', '2026-04-11T08:00:00Z']]`
- **rdate-only** / `icaljs`: got `['2026-04-05T07:00:00Z', '2026-04-07T07:00:00Z']`; accepted `[['2026-04-01T07:00:00Z', '2026-04-05T07:00:00Z', '2026-04-07T07:00:00Z']]`
- **rdate-only** / `rrule-jiff`: got `['2026-04-05T07:00:00Z', '2026-04-07T07:00:00Z']`; accepted `[['2026-04-01T07:00:00Z', '2026-04-05T07:00:00Z', '2026-04-07T07:00:00Z']]`
- **rdate-only** / `rrule`: got `['2026-04-05T07:00:00Z', '2026-04-07T07:00:00Z']`; accepted `[['2026-04-01T07:00:00Z', '2026-04-05T07:00:00Z', '2026-04-07T07:00:00Z']]`
- **rdate-only** / `rrule2`: got `['2026-04-05T07:00:00Z', '2026-04-07T07:00:00Z']`; accepted `[['2026-04-01T07:00:00Z', '2026-04-05T07:00:00Z', '2026-04-07T07:00:00Z']]`
- **rdate-gap-and-fold** / `calcard`: got `['2026-10-24T00:30:00Z', '2026-10-25T01:30:00Z']`; accepted `[['2026-10-24T00:30:00Z', '2026-10-25T00:30:00Z', '2026-10-25T01:30:00Z']]`
- **rdate-gap-and-fold** / `icaljs`: got `['2026-10-24T00:30:00Z', '2026-10-25T01:30:00Z']`; accepted `[['2026-10-24T00:30:00Z', '2026-10-25T00:30:00Z', '2026-10-25T01:30:00Z']]`
- **rdate-gap-and-fold** / `rrule`: got `RRule parsing error: RDATE:20261025T023000 is not a valid datetime in local timezone. This value is ambiguous and can be `2026-10-25T02:30:00+02:00` or `2026-10-25T02:30:00+01:00``; accepted `[['2026-10-24T00:30:00Z', '2026-10-25T00:30:00Z', '2026-10-25T01:30:00Z']]`
- **exrule** / `calcard-next`: got `['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-03T07:00:00Z', '2026-04-04T07:00:00Z', '2026-04-05T07:00:00Z', '2026-04-06T07:00:00Z', '2026-04-07T07:00:00Z']`; accepted `[['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-03T07:00:00Z', '2026-04-06T07:00:00Z', '2026-04-07T07:00:00Z']]`
- **exrule** / `calcard`: got `['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-03T07:00:00Z', '2026-04-04T07:00:00Z', '2026-04-05T07:00:00Z', '2026-04-06T07:00:00Z', '2026-04-07T07:00:00Z']`; accepted `[['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-03T07:00:00Z', '2026-04-06T07:00:00Z', '2026-04-07T07:00:00Z']]`
- **exrule** / `icaljs`: got `['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-03T07:00:00Z', '2026-04-04T07:00:00Z', '2026-04-05T07:00:00Z', '2026-04-06T07:00:00Z', '2026-04-07T07:00:00Z']`; accepted `[['2026-04-01T07:00:00Z', '2026-04-02T07:00:00Z', '2026-04-03T07:00:00Z', '2026-04-06T07:00:00Z', '2026-04-07T07:00:00Z']]`
- **override-of-rdate** / `calcard`: got `['2026-04-06T07:00:00Z', '2026-04-13T07:00:00Z', '2026-04-20T07:00:00Z', '2026-04-21T07:00:00Z']`; accepted `[['2026-04-06T07:00:00Z', '2026-04-13T07:00:00Z', '2026-04-21T07:00:00Z']]`
- **impossible-feb30** / `icaljs`: got `['2026-03-02T08:00:00Z', '2027-03-02T08:00:00Z', '2028-03-01T08:00:00Z', '2029-03-02T08:00:00Z']`; accepted `[['2026-01-01T08:00:00Z'], []]`
- **vtimezone-windows-name** / `rrule-jiff`: got `RRule parsing error: `W. Europe Standard Time` is not a valid timezone.`; accepted `[['2026-03-16T08:00:00Z', '2026-03-23T08:00:00Z', '2026-03-30T07:00:00Z']]`
- **vtimezone-windows-name** / `rrule`: got `RRule parsing error: `W. Europe Standard Time` is not a valid timezone.`; accepted `[['2026-03-16T08:00:00Z', '2026-03-23T08:00:00Z', '2026-03-30T07:00:00Z']]`
- **vtimezone-windows-name** / `rrule2`: got `RRule parsing error: `W. Europe Standard Time` is not a valid timezone.`; accepted `[['2026-03-16T08:00:00Z', '2026-03-23T08:00:00Z', '2026-03-30T07:00:00Z']]`
- **vtimezone-custom-rules** / `calcard-next`: got `['2026-03-06T09:00:00', '2026-03-13T09:00:00', '2026-03-20T09:00:00']`; accepted `[['2026-03-06T07:30:00Z', '2026-03-13T06:30:00Z', '2026-03-20T06:30:00Z']]`
- **vtimezone-custom-rules** / `calcard`: got `['2026-03-06T09:00:00', '2026-03-13T09:00:00', '2026-03-20T09:00:00']`; accepted `[['2026-03-06T07:30:00Z', '2026-03-13T06:30:00Z', '2026-03-20T06:30:00Z']]`
- **vtimezone-custom-rules** / `rrule-jiff`: got `RRule parsing error: `Musubi Test Zone` is not a valid timezone.`; accepted `[['2026-03-06T07:30:00Z', '2026-03-13T06:30:00Z', '2026-03-20T06:30:00Z']]`
- **vtimezone-custom-rules** / `rrule`: got `RRule parsing error: `Musubi Test Zone` is not a valid timezone.`; accepted `[['2026-03-06T07:30:00Z', '2026-03-13T06:30:00Z', '2026-03-20T06:30:00Z']]`
- **vtimezone-custom-rules** / `rrule2`: got `RRule parsing error: `Musubi Test Zone` is not a valid timezone.`; accepted `[['2026-03-06T07:30:00Z', '2026-03-13T06:30:00Z', '2026-03-20T06:30:00Z']]`
