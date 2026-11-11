//! Deterministic synthetic calendar data. Same input -> same items on both backends.

use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};

/// 2026-11-11T00:00:00Z, the "now" of the dataset.
pub const NOW: i64 = 1_794_355_200;
pub const DAY: i64 = 86_400;
pub const WEEK: i64 = 7 * DAY;
pub const NCOLL: i64 = 20;
/// Longest single occurrence; range queries scan start_utc from `from - MAX_DUR`.
/// shortcut: events longer than this need the series-span path from proposal 2.5, not modelled here.
pub const MAX_DUR: i64 = 2 * DAY;
/// Index horizon from the proposal (2.5): 2 years back, 5 years forward.
const HORIZON_FROM: i64 = NOW - 730 * DAY;
const HORIZON_TO: i64 = NOW + 1826 * DAY;

pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    pub fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo) as u64) as i64
    }
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
}

pub struct Occ {
    pub start: i64,
    pub end: i64,
    /// 0 for single events, original start for series occurrences.
    pub rid: i64,
}

pub struct Item {
    pub key: String,
    pub validator: String,
    pub raw_hash: Vec<u8>,
    pub raw: String,
    pub canonical: String,
    pub occ: Vec<Occ>,
    /// Second placement of the entity that owns this remote key in collection 0 (chapter 12).
    pub link: Option<String>,
}

pub struct Page {
    pub coll: i64,
    pub cursor: String,
    pub items: Vec<Item>,
}

const WORDS: &[&str] = &[
    "planning", "review", "sync", "team", "weekly", "budget", "design", "lunch", "call", "client",
    "dentist", "gym", "school", "project", "release", "retro", "standup", "board", "travel", "flight",
    "hotel", "dinner", "birthday", "meeting", "workshop", "offsite", "interview", "demo", "training",
    "doctor", "yoga", "football", "concert", "deadline", "roadmap", "quarterly", "onboarding", "Praha",
    "Brno", "kancelář", "porada", "oběd", "schůzka", "úterý", "zasedačka", "the", "and", "with", "for",
];

fn words(r: &mut Rng, s: &mut String, chars: usize) {
    let start = s.len();
    while s.len() - start < chars {
        if s.len() > start {
            s.push(' ');
        }
        s.push_str(WORDS[r.below(WORDS.len() as u64) as usize]);
    }
}

fn hash16(s: &str) -> Vec<u8> {
    // shortcut: SipHash x2, not a cryptographic content hash; fine for a storage benchmark.
    let mut out = Vec::with_capacity(16);
    for salt in [1u8, 2] {
        let mut h = DefaultHasher::new();
        salt.hash(&mut h);
        s.hash(&mut h);
        out.extend_from_slice(&h.finish().to_le_bytes());
    }
    out
}

/// Collection weights ~ 1/(c+1): a few busy calendars, a long tail of small ones.
fn pick_coll(r: &mut Rng) -> i64 {
    let total: f64 = (1..=NCOLL).map(|c| 1.0 / c as f64).sum();
    let mut x = (r.next() >> 11) as f64 / (1u64 << 53) as f64 * total;
    for c in 0..NCOLL {
        x -= 1.0 / (c + 1) as f64;
        if x <= 0.0 {
            return c;
        }
    }
    NCOLL - 1
}

struct Shape {
    coll: i64,
    start: i64,
    dur: i64,
    /// (step seconds, count) for series; None for single events.
    rule: Option<(&'static str, i64, i64)>,
}

fn shape(r: &mut Rng) -> Shape {
    let coll = pick_coll(r);
    let all_day = r.chance(0.08);
    let start = r.range(NOW - 730 * DAY, NOW + 365 * DAY);
    let start = if all_day { start - start % DAY } else { start - start % 900 };
    let dur = if all_day { DAY } else { [1800, 3600, 3600, 5400, 7200][r.below(5) as usize] };
    // ~10 % series (assumption, see report): mostly weekly, some daily/monthly/yearly.
    let rule = if r.chance(0.10) {
        Some(match r.below(10) {
            0 | 1 => ("DAILY", DAY, r.range(5, 30)),
            2..=6 => ("WEEKLY", WEEK, r.range(8, 105)),
            7 | 8 => ("MONTHLY", 30 * DAY, r.range(6, 37)),
            _ => ("YEARLY", 365 * DAY, 10),
        })
    } else {
        None
    };
    Shape { coll, start, dur, rule }
}

fn occurrences(s: &Shape) -> Vec<Occ> {
    match s.rule {
        None => vec![Occ { start: s.start, end: s.start + s.dur, rid: 0 }],
        Some((_, step, count)) => (0..count)
            .map(|k| s.start + k * step)
            .filter(|t| *t >= HORIZON_FROM && *t < HORIZON_TO)
            .map(|t| Occ { start: t, end: t + s.dur, rid: t })
            .collect(),
    }
}

fn item(tenant: u64, i: u64, s: &Shape, version: u32, link: Option<String>) -> Item {
    let mut r = Rng::new(tenant.wrapping_mul(1_000_003) ^ (i << 20) ^ version as u64);
    let key = format!("t{tenant}e{i}");
    let mut title = String::new();
    words(&mut r, &mut title, 24);
    if version > 0 {
        let _ = write!(title, " v{version}");
    }
    let mut desc = String::new();
    let dlen = r.range(250, 650) as usize;
    words(&mut r, &mut desc, dlen);
    let mut loc = String::new();
    words(&mut r, &mut loc, 20);
    let nparts = r.below(5);
    let mut parts = String::new();
    for p in 0..nparts {
        let _ = write!(
            parts,
            r#"{}"p{p}":{{"@type":"Participant","email":"person{}@example.org","roles":{{"attendee":true}},"participationStatus":"accepted"}}"#,
            if p > 0 { "," } else { "" },
            r.below(10_000)
        );
    }
    let rrule = match s.rule {
        Some((freq, _, count)) => format!(r#","recurrenceRules":[{{"@type":"RecurrenceRule","frequency":"{}","count":{count}}}]"#, freq.to_lowercase()),
        None => String::new(),
    };
    let canonical = format!(
        r#"{{"@type":"Event","uid":"{key}@musubi","updated":{},"title":"{title}","start":{},"duration":"PT{}S","timeZone":"Europe/Prague","description":"{desc}","locations":{{"l1":{{"name":"{loc}"}}}},"participants":{{{parts}}},"alerts":{{"a1":{{"trigger":{{"offset":"-PT15M"}}}}}}{rrule},"schema_version":1}}"#,
        NOW + version as i64,
        s.start,
        s.dur
    );
    let validator = format!("\"{}-{version}\"", r.next() % 1_000_000_000);
    let raw = format!(
        r#"{{"kind":"calendar#event","etag":{validator},"id":"{key}","status":"confirmed","htmlLink":"https://calendar.example.com/event?eid={key}","created":"2025-01-01T00:00:00Z","updated":"{}","summary":"{title}","description":"{desc}","location":"{loc}","start":{{"dateTime":{},"timeZone":"Europe/Prague"}},"end":{{"dateTime":{}}},"iCalUID":"{key}@musubi","sequence":{version},"reminders":{{"useDefault":true}},"attendees_count":{nparts}}}"#,
        NOW + version as i64,
        s.start,
        s.start + s.dur
    );
    Item {
        raw_hash: hash16(&raw),
        key,
        validator,
        raw,
        canonical,
        occ: occurrences(s),
        link,
    }
}

/// All items of a tenant at `version`, grouped into per-collection pages of `page` items,
/// collections in order 0..NCOLL (so link targets in collection 0 exist before their links).
/// Version > 0 changes validator and title of every item but keeps times, as an update would.
pub fn tenant_pages(tenant: u64, n: u64, page: usize, version: u32) -> Vec<Page> {
    let mut r = Rng::new(tenant);
    let shapes: Vec<Shape> = (0..n).map(|_| shape(&mut r)).collect();
    let coll0: Vec<u64> = (0..n).filter(|i| shapes[*i as usize].coll == 0).collect();
    let mut by_coll: Vec<Vec<Item>> = (0..NCOLL).map(|_| Vec::new()).collect();
    for i in 0..n {
        let s = &shapes[i as usize];
        // ~2 % of items outside collection 0 are a second placement of a collection-0 entity.
        let target = (s.coll != 0 && !coll0.is_empty() && r.chance(0.02))
            .then(|| coll0[r.below(coll0.len() as u64) as usize]);
        let it = match target {
            Some(t) => {
                let ts = &shapes[t as usize];
                let copy = Shape { coll: s.coll, start: ts.start, dur: ts.dur, rule: ts.rule };
                let mut it = item(tenant, i, &copy, version, Some(format!("t{tenant}e{t}")));
                // Same event, so same canonical document as the target (materialized entity).
                it.canonical = item(tenant, t, ts, version, None).canonical;
                it
            }
            None => item(tenant, i, s, version, None),
        };
        by_coll[s.coll as usize].push(it);
    }
    let mut pages = Vec::new();
    for (c, items) in by_coll.into_iter().enumerate() {
        let mut items = items.into_iter().peekable();
        let mut k = 0;
        while items.peek().is_some() {
            let chunk: Vec<Item> = items.by_ref().take(page).collect();
            pages.push(Page { coll: c as i64, cursor: format!("sync-token-{tenant}-{c}-{version}-{k}"), items: chunk });
            k += 1;
        }
    }
    pages
}

/// A small incremental-sync page: `n` items of collection `coll` changed to `version`.
pub fn incremental_page(tenant: u64, total: u64, coll: i64, n: usize, version: u32) -> Page {
    let mut p: Vec<Item> = tenant_pages(tenant, total, usize::MAX, version)
        .into_iter()
        .find(|p| p.coll == coll)
        .expect("collection has items")
        .items;
    p.truncate(n);
    Page { coll, cursor: format!("inc-{tenant}-{coll}-{version}"), items: p }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_sized() {
        let a = tenant_pages(7, 5000, 250, 0);
        let b = tenant_pages(7, 5000, 250, 0);
        assert_eq!(a.len(), b.len());
        let n: usize = a.iter().map(|p| p.items.len()).sum();
        assert_eq!(n, 5000);
        assert_eq!(a[3].items[0].canonical, b[3].items[0].canonical);
        let bytes: usize = a.iter().flat_map(|p| &p.items).map(|i| i.canonical.len() + i.raw.len()).sum();
        let per = bytes / n;
        assert!((1500..4000).contains(&per), "payload per item {per}");
        let occ: usize = a.iter().flat_map(|p| &p.items).map(|i| i.occ.len()).sum();
        assert!(occ > 5000 && occ < 40_000, "occurrences {occ}");
        assert!(a.iter().flat_map(|p| &p.items).any(|i| i.link.is_some()));
        // Every page holds one collection and collections come in order.
        assert!(a.windows(2).all(|w| w[0].coll <= w[1].coll));
        // Update keeps keys and times, changes validators.
        let u = tenant_pages(7, 5000, 250, 1);
        assert_eq!(u[3].items[0].key, a[3].items[0].key);
        assert_ne!(u[3].items[0].validator, a[3].items[0].validator);
        assert_eq!(u[3].items[0].occ[0].start, a[3].items[0].occ[0].start);
    }
}
