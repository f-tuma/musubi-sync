// Oracle 2: ical.js 2.2.1 (Mozilla/Thunderbird lineage). ical.js expands
// RRULE/RDATE/EXDATE and applies RECURRENCE-ID (incl. RANGE=THISANDFUTURE)
// itself via ICAL.Event. It has no tzdb, so IANA zones come from
// vtimezones.json (generated from the system tzdb by gen_vtimezones.py).
// Usage: node oracles/icaljs/oracle.mjs corpus/cases.json > results/icaljs.json
import ICAL from "ical.js";
import fs from "node:fs";

const cases = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
const zones = JSON.parse(fs.readFileSync(new URL("./vtimezones.json", import.meta.url), "utf8"));
const pad = (n, w = 2) => String(n).padStart(w, "0");
const parseUntil = (s) => Date.UTC(+s.slice(0, 4), +s.slice(4, 6) - 1, +s.slice(6, 8), +s.slice(9, 11), +s.slice(11, 13), +s.slice(13, 15)) / 1000;
const naive = (t) => Date.UTC(t.year, t.month - 1, t.day, t.hour, t.minute, t.second) / 1000;
const isFloating = (t) => !t.isDate && (!t.zone || t.zone.tzid === "floating");

function fmt(t) {
  const d = `${pad(t.year, 4)}-${pad(t.month)}-${pad(t.day)}`;
  if (t.isDate) return d;
  if (isFloating(t)) return `${d}T${pad(t.hour)}:${pad(t.minute)}:${pad(t.second)}`;
  return new Date(t.toUnixTime() * 1000).toISOString().replace(".000Z", "Z");
}

function expand(c) {
  ICAL.TimezoneService.reset();
  const declared = new Set([...c.ics.matchAll(/^TZID:(.*)$/gm)].map((m) => m[1]));
  const needed = new Set([...c.ics.matchAll(/TZID=([^:;]+)/g)].map((m) => m[1]));
  let vtz = "";
  for (const z of needed) if (!declared.has(z) && zones[z]) vtz += zones[z];
  const cal = new ICAL.Component(ICAL.parse(`BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:x\n${vtz}${c.ics}END:VCALENDAR\n`));
  for (const z of cal.getAllSubcomponents("vtimezone")) ICAL.TimezoneService.register(z);
  for (const z of needed)
    if (!ICAL.TimezoneService.has(z)) throw new Error(`unknown TZID ${z}`); // ical.js would silently float it
  const vevents = cal.getAllSubcomponents("vevent");
  const master = new ICAL.Event(vevents.find((v) => !v.hasProperty("recurrence-id")));
  for (const v of vevents) if (v.hasProperty("recurrence-id")) master.relateException(v);
  const until = parseUntil(c.iter_until);
  const out = new Set();
  if (!master.isRecurring()) {
    out.add(fmt(master.startDate));
  } else {
    const it = master.iterator();
    const deadline = Date.now() + 10000;
    for (let next, n = 0; (next = it.next()); n++) {
      if (Date.now() > deadline) throw new Error("timeout 10s");
      const key = next.isDate || isFloating(next) ? naive(next) : next.toUnixTime();
      if (key > until || n >= c.iter_max) break;
      out.add(fmt(master.getOccurrenceDetails(next).startDate));
    }
    // An override whose original instance lies past the iteration bound can
    // still move into it; ical.js only iterates original instances.
    for (const ex of Object.values(master.exceptions))
      if (ex.recurrenceId.toUnixTime() > until) out.add(fmt(ex.startDate));
  }
  return [...out].sort();
}

const results = {};
for (const c of cases) {
  try {
    results[c.id] = { ok: expand(c) };
  } catch (e) {
    results[c.id] = { error: String(e && e.message ? e.message : e) };
  }
}
process.stdout.write(JSON.stringify({ engine: "ical.js 2.2.1", results }, null, 1) + "\n");
