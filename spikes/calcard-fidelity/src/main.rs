//! calcard fidelity harness (Phase 0 spike).
//!
//! cargo run --release -- corpus <dir>...   parse / round-trip / patch / JS* conversion per file
//! cargo run --release -- expand <file>...  recurrence expansion via ICalendar::expand_dates
//! cargo run --release -- rt <file>...      print calcard's re-serialisation (for diffing)
//! cargo run --release -- js <file> [--no-raw]  print X -> JSCalendar/JSContact JSON -> X
//! cargo run --release -- bench [n]         throughput on generated n-event / n-card payloads
//!
//! Per-file results go to results/<dir-name>.tsv, aggregated losses to results/<dir-name>.lost.txt.

mod rawpatch;

use calcard::icalendar::{ICalendar, ICalendarComponentType, ICalendarProperty, ICalendarValue};
use calcard::jscalendar::JSCalendar;
use calcard::jscontact::JSContact;
use calcard::vcard::{VCard, VCardProperty, VCardValue, VCardVersion};
use calcard::{Entry, Parser};
use rawpatch::{logical_lines, RawLine};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, PartialEq, Debug)]
enum Obj {
    Ical(ICalendar),
    Card(VCard),
}

impl Obj {
    fn write(&self) -> String {
        match self {
            Obj::Ical(c) => c.to_string(),
            Obj::Card(c) => c.to_string(),
        }
    }
}

struct Parsed {
    objs: Vec<Obj>,
    warnings: usize,
    errors: Vec<String>,
}

fn parse_all(text: &str) -> Parsed {
    let mut p = Parser::new(text);
    let mut r = Parsed { objs: vec![], warnings: 0, errors: vec![] };
    for _ in 0..1_000_000 {
        match p.entry() {
            Entry::ICalendar(c) => r.objs.push(Obj::Ical(c)),
            Entry::VCard(c) => r.objs.push(Obj::Card(c)),
            Entry::InvalidLine(_) => r.warnings += 1,
            Entry::Eof => break,
            other => r.errors.push(format!("{other:?}")),
        }
    }
    r
}

fn write_all(objs: &[Obj]) -> String {
    objs.iter().map(Obj::write).collect()
}

fn texts(lines: &[RawLine]) -> Vec<String> {
    lines.iter().map(|l| l.text.clone()).collect()
}

/// Multiset difference a - b.
fn minus(a: &[String], b: &[String]) -> Vec<String> {
    let mut bag: BTreeMap<&str, usize> = BTreeMap::new();
    for x in b {
        *bag.entry(x).or_default() += 1;
    }
    a.iter()
        .filter(|x| match bag.get_mut(x.as_str()) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .cloned()
        .collect()
}

fn line_name(t: &str) -> String {
    RawLine { start: 0, end: 0, text: t.to_string() }.name()
}

/// Split the parameter section of a content line, honouring quotes.
fn params_of(text: &str) -> Vec<String> {
    let (mut out, mut cur, mut q) = (vec![], String::new(), false);
    for ch in text.chars() {
        match ch {
            '"' => {
                q = !q;
                cur.push(ch)
            }
            ':' if !q => break,
            ';' if !q => out.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    out.push(cur);
    out.remove(0);
    out
}

/// Top-level objects of a raw payload as logical-line ranges.
fn split_objects(lines: &[RawLine]) -> Vec<Vec<RawLine>> {
    let (mut out, mut cur, mut depth) = (vec![], vec![], 0i32);
    for l in lines {
        match l.begin_end() {
            Some((true, _)) => depth += 1,
            Some((false, _)) => depth -= 1,
            None if depth == 0 => continue,
            None => {}
        }
        cur.push(l.clone());
        if depth == 0 {
            out.push(std::mem::take(&mut cur));
        }
    }
    out
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Summary,
    Dtstart,
    Tel,
}

/// Locate and mutate the patch target. Returns (object idx, entry locator, mutated object).
/// Entry locator: (component id, entry idx) for iCal, (0, entry idx) for vCard.
fn mutate(objs: &[Obj], op: Op) -> Option<(usize, (u32, usize), Obj)> {
    for (oi, o) in objs.iter().enumerate() {
        match (o, op) {
            (Obj::Ical(c), Op::Summary | Op::Dtstart) => {
                let want = if matches!(op, Op::Summary) { ICalendarProperty::Summary } else { ICalendarProperty::Dtstart };
                for (cid, comp) in c.components.iter().enumerate() {
                    if !matches!(comp.component_type, ICalendarComponentType::VEvent | ICalendarComponentType::VTodo) {
                        continue;
                    }
                    let Some(ei) = comp.entries.iter().position(|e| e.name == want) else { continue };
                    let mut c2 = c.clone();
                    let e = &mut c2.components[cid].entries[ei];
                    match op {
                        Op::Summary => e.values = vec![ICalendarValue::Text("Patched summary".into())],
                        _ => match e.values.first_mut() {
                            Some(ICalendarValue::PartialDateTime(dt)) => {
                                if let Some(h) = dt.hour.as_mut() {
                                    *h = (*h + 1) % 24;
                                } else if let Some(d) = dt.day.as_mut() {
                                    *d = if *d < 28 { *d + 1 } else { 1 };
                                }
                            }
                            _ => return None,
                        },
                    }
                    return Some((oi, (cid as u32, ei), Obj::Ical(c2)));
                }
            }
            (Obj::Card(c), Op::Tel) => {
                let Some(ei) = c.entries.iter().position(|e| e.name == VCardProperty::Tel) else { continue };
                let mut c2 = c.clone();
                c2.entries[ei].values = vec![VCardValue::Text("+420 111 222 333".into())];
                return Some((oi, (0, ei), Obj::Card(c2)));
            }
            _ => {}
        }
    }
    None
}

#[derive(Default)]
struct PatchResult {
    /// Non-target input lines that are not in the AST-reserialised patched output.
    ast_collateral_vs_input: usize,
    /// Lines that differ between calcard's own round-trip and the patched AST output, minus target.
    ast_collateral_vs_rt: usize,
    /// "ok" | "refused: …" | "verify_fail: …"
    raw: String,
    /// Same, but value-only rewrite of the target line (original name/params bytes kept).
    raw_value: String,
    /// Raw patch rewrote the target line exactly as the input had it apart from the value?
    raw_target_line_changed_beyond_value: bool,
}

fn patch(raw: &str, parsed: &Parsed, rt: &str, op: Op) -> Option<PatchResult> {
    let (oi, (cid, ei), mutated) = mutate(&parsed.objs, op)?;
    let mut objs = parsed.objs.clone();
    objs[oi] = mutated.clone();
    let patched = write_all(&objs);
    let (in_l, rt_l, p_l) = (texts(&logical_lines(raw)), texts(&logical_lines(rt)), texts(&logical_lines(&patched)));
    let mut r = PatchResult {
        ast_collateral_vs_input: minus(&in_l, &p_l).len().saturating_sub(1),
        ast_collateral_vs_rt: minus(&rt_l, &p_l).len().saturating_sub(1),
        ..Default::default()
    };

    // Raw route: align, re-serialise only the target entry, splice it in.
    let all = logical_lines(raw);
    let objects = split_objects(&all);
    if objects.len() != parsed.objs.len() {
        r.raw = format!("refused: {} raw objects vs {} parsed", objects.len(), parsed.objs.len());
        return Some(r);
    }
    let lines = &objects[oi];
    let (line, new_entry) = match (&parsed.objs[oi], &mutated) {
        (Obj::Ical(orig), Obj::Ical(m)) => match rawpatch::align_ical(lines, orig) {
            Ok(map) => {
                let li = map.iter().find(|(k, _)| *k == (cid, ei)).unwrap().1;
                (lines[li].clone(), rawpatch::write_ical_entry(&m.components[cid as usize].entries[ei]))
            }
            Err(e) => {
                r.raw = format!("refused: {e}");
                return Some(r);
            }
        },
        (Obj::Card(orig), Obj::Card(m)) => match rawpatch::align_vcard(lines, orig) {
            Ok(map) => {
                let li = map.iter().find(|(e, _)| *e == ei).unwrap().1;
                let v = orig.version().unwrap_or_default();
                (lines[li].clone(), rawpatch::write_vcard_entry(&m.entries[ei], v))
            }
            Err(e) => {
                r.raw = format!("refused: {e}");
                return Some(r);
            }
        },
        _ => unreachable!(),
    };
    let mut expect = parsed.objs.clone();
    expect[oi] = mutated;
    let verify = |new_line: &str| {
        let out = rawpatch::splice(raw, &line, new_line);
        let tail = raw.len() - line.end;
        let bytes_ok = out.as_bytes()[..line.start] == raw.as_bytes()[..line.start]
            && out.as_bytes()[out.len() - tail..] == raw.as_bytes()[line.end..];
        if !bytes_ok {
            "verify_fail: bytes outside span changed".to_string()
        } else if parse_all(&out).objs != expect {
            "verify_fail: re-parse differs from mutated AST".to_string()
        } else {
            "ok".to_string()
        }
    };
    r.raw = verify(&new_entry);
    r.raw_value = match rawpatch::value_only_line(&line, &new_entry) {
        Some(l) => verify(&l),
        None => "refused: no ':' in line".into(),
    };
    // Does the rewritten line keep the original name casing / param spelling? Compare everything
    // before the first ':' (outside quotes) of the unfolded old vs new line.
    let head = |t: &str| t[..t.len() - t.split(':').next_back().unwrap_or("").len()].to_string();
    let new_text = logical_lines(&new_entry).first().map(|l| l.text.clone()).unwrap_or_default();
    r.raw_target_line_changed_beyond_value = !matches!(op, Op::Dtstart) && head(&line.text) != head(&new_text);
    Some(r)
}

#[derive(Default)]
struct ConvResult {
    ok: bool,
    lines_rt: usize,
    lines_lost: Vec<String>,
    lines_added: Vec<String>,
    /// Property names (multiset) present in calcard's rt but absent after X->JS->X.
    props_lost: Vec<String>,
    err: String,
}

/// X -> JSCalendar/JSContact JSON -> X. Returns (json, back-converted text).
fn js_round(obj: &Obj, keep_raw: bool) -> (String, Result<String, String>) {
    match obj.clone() {
        Obj::Ical(c) => {
            let opt = calcard::jscalendar::import::ConversionOptions::default().include_ical_components(keep_raw);
            let js = c.into_jscalendar_with_opt::<String, String>(opt);
            let json = serde_json::to_string(&js.0).unwrap();
            let back = match JSCalendar::<String, String>::parse(&json) {
                Ok(j) => j.into_icalendar().map(|c| c.to_string()).ok_or("into_icalendar: None".to_string()),
                Err(e) => Err(format!("JSCalendar::parse: {e}")),
            };
            (json, back)
        }
        Obj::Card(c) => {
            let opt = calcard::jscontact::import::ConversionOptions::default().include_vcard_parameters(keep_raw);
            let js = c.into_jscontact_with_options::<String, String>(opt);
            let json = serde_json::to_string(&js.0).unwrap();
            let back = match JSContact::<String, String>::parse(&json) {
                Ok(j) => j.into_vcard().map(|c| c.to_string()).ok_or("into_vcard: None".to_string()),
                Err(e) => Err(format!("JSContact::parse: {e}")),
            };
            (json, back)
        }
    }
}

fn convert(obj: &Obj, keep_raw: bool) -> ConvResult {
    let rt = obj.write();
    let back = js_round(obj, keep_raw).1;
    let rt_l = texts(&logical_lines(&rt));
    match back {
        Ok(out) => {
            let out_l = texts(&logical_lines(&out));
            let names = |v: &[String]| v.iter().map(|t| line_name(t)).collect::<Vec<_>>();
            let props_lost = minus(&names(&rt_l), &names(&out_l));
            ConvResult { ok: true, lines_rt: rt_l.len(), lines_lost: minus(&rt_l, &out_l), lines_added: minus(&out_l, &rt_l), props_lost, err: String::new() }
        }
        Err(e) => ConvResult { lines_rt: rt_l.len(), err: e, ..Default::default() },
    }
}

#[derive(Default)]
struct Agg {
    files: usize,
    parse_ok: usize,
    objects: usize,
    warnings: usize,
    errors: usize,
    level: [usize; 4],
    lines_in: usize,
    lines_lost: usize,
    xprop: (usize, usize),
    xparam: (usize, usize),
    group: (usize, usize),
    vtz: (usize, usize),
    order_ok: usize,
    idempotent: usize,
    ast_eq: usize,
    patch: BTreeMap<String, (usize, usize, usize, usize, usize, usize)>, // n, ast clean vs input, ast clean vs rt, raw ok, raw head changed, raw value-only ok
    raw_refusals: BTreeMap<String, usize>,
    conv: BTreeMap<String, (usize, usize, usize, usize, usize)>, // objs, ok, rt lines, lost lines, lost props
    lost_names: BTreeMap<String, (usize, String)>,
    conv_lost_names: BTreeMap<String, BTreeMap<String, (usize, String)>>,
    conv_lost_props: BTreeMap<String, BTreeMap<String, usize>>,
}

fn bump(m: &mut BTreeMap<String, (usize, String)>, line: &str) {
    let e = m.entry(line_name(line)).or_insert((0, line.chars().take(110).collect()));
    e.0 += 1;
}

fn files_in(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("ics" | "vcf")))
        .collect();
    v.sort();
    v
}

fn corpus(dirs: &[String]) {
    std::fs::create_dir_all("results").unwrap();
    for d in dirs {
        let dir = Path::new(d);
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let name = match dir.parent().and_then(|p| p.file_name()).map(|p| p.to_string_lossy().to_string()) {
            Some(p) if p == "resources" => format!("calcard-{name}"),
            _ => name,
        };
        let mut a = Agg::default();
        let mut tsv = String::from("file\tparse_ok\tobjs\twarn\terr\trt_level\tlost\tadded\tidempotent\tast_eq\tpatch_summary\tpatch_dtstart\tpatch_tel\tjs_lost\n");
        let mut non_utf8 = 0;
        for f in files_in(dir) {
            let bytes = std::fs::read(&f).unwrap();
            let raw = match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(e) => {
                    non_utf8 += 1;
                    String::from_utf8_lossy(e.as_bytes()).into_owned()
                }
            };
            a.files += 1;
            let p = parse_all(&raw);
            let ok = !p.objs.is_empty() && p.errors.is_empty();
            a.parse_ok += ok as usize;
            a.objects += p.objs.len();
            a.warnings += p.warnings;
            a.errors += p.errors.len();
            let fname = f.file_name().unwrap().to_string_lossy().to_string();
            if p.objs.is_empty() {
                let _ = writeln!(tsv, "{fname}\tfalse\t0\t{}\t{}\t-\t-\t-\t-\t-\t-\t-\t-\t-", p.warnings, p.errors.len());
                continue;
            }

            // Round trip
            let rt = write_all(&p.objs);
            let in_lines = logical_lines(&raw);
            let (in_l, out_l) = (texts(&in_lines), texts(&logical_lines(&rt)));
            let lost = minus(&in_l, &out_l);
            let added = minus(&out_l, &in_l);
            let level = if rt == raw {
                0
            } else if in_l == out_l {
                1
            } else if lost.is_empty() && added.is_empty() {
                2
            } else {
                3
            };
            a.level[level] += 1;
            a.lines_in += in_l.len();
            a.lines_lost += lost.len();
            for l in &lost {
                bump(&mut a.lost_names, l);
            }
            let out_set: std::collections::HashSet<&str> = out_l.iter().map(String::as_str).collect();
            let out_joined = out_l.join("\n");
            for l in &in_l {
                let nm = line_name(l);
                if nm.starts_with("X-") {
                    a.xprop.0 += 1;
                    a.xprop.1 += out_set.contains(l.as_str()) as usize;
                }
                if l.split([':', ';']).next().is_some_and(|h| h.contains('.')) {
                    a.group.0 += 1;
                    a.group.1 += out_set.contains(l.as_str()) as usize;
                }
                for prm in params_of(l) {
                    if prm.to_ascii_uppercase().starts_with("X-") {
                        a.xparam.0 += 1;
                        a.xparam.1 += out_joined.contains(&format!(";{prm}")) as usize;
                    }
                }
            }
            let mut i = 0;
            while i < in_l.len() {
                if in_l[i].eq_ignore_ascii_case("BEGIN:VTIMEZONE") {
                    let j = (i..in_l.len()).find(|&j| in_l[j].eq_ignore_ascii_case("END:VTIMEZONE")).unwrap_or(in_l.len() - 1);
                    a.vtz.0 += 1;
                    a.vtz.1 += out_joined.contains(&in_l[i..=j].join("\n")) as usize;
                    i = j;
                }
                i += 1;
            }
            let names = |v: &[String]| v.iter().map(|t| line_name(t)).collect::<Vec<_>>();
            a.order_ok += (names(&in_l) == names(&out_l)) as usize;
            let rt2 = write_all(&parse_all(&rt).objs);
            let idem = rt2 == rt;
            a.idempotent += idem as usize;
            let ast_eq = parse_all(&rt).objs == p.objs;
            a.ast_eq += ast_eq as usize;

            // Patch
            let mut pcols = vec![];
            for op in [Op::Summary, Op::Dtstart, Op::Tel] {
                match patch(&raw, &p, &rt, op) {
                    None => pcols.push("-".to_string()),
                    Some(r) => {
                        let e = a.patch.entry(format!("{op:?}")).or_default();
                        e.0 += 1;
                        e.1 += (r.ast_collateral_vs_input == 0) as usize;
                        e.2 += (r.ast_collateral_vs_rt == 0) as usize;
                        e.3 += (r.raw == "ok") as usize;
                        e.4 += r.raw_target_line_changed_beyond_value as usize;
                        e.5 += (r.raw_value == "ok") as usize;
                        if r.raw_value.starts_with("verify_fail") {
                            *a.raw_refusals.entry(format!("value-only {}", r.raw_value)).or_default() += 1;
                        }
                        if r.raw != "ok" {
                            *a.raw_refusals.entry(r.raw.clone()).or_default() += 1;
                        }
                        pcols.push(format!("ast_coll_in={} ast_coll_rt={} raw={} raw_value={}", r.ast_collateral_vs_input, r.ast_collateral_vs_rt, r.raw, r.raw_value));
                    }
                }
            }

            // JS* conversion
            let mut js_lost = 0;
            for o in &p.objs {
                for keep in [true, false] {
                    let key = format!("{}{}", if matches!(o, Obj::Ical(_)) { "JSCalendar" } else { "JSContact" }, if keep { "+raw-props" } else { "-raw-props" });
                    let c = convert(o, keep);
                    let e = a.conv.entry(key.clone()).or_default();
                    e.0 += 1;
                    e.1 += c.ok as usize;
                    e.2 += c.lines_rt;
                    e.3 += if c.ok { c.lines_lost.len() } else { c.lines_rt };
                    e.4 += if c.ok { c.props_lost.len() } else { c.lines_rt };
                    let pm = a.conv_lost_props.entry(key.clone()).or_default();
                    for n in &c.props_lost {
                        *pm.entry(n.clone()).or_default() += 1;
                    }
                    if keep {
                        js_lost += c.lines_lost.len();
                    }
                    let m = a.conv_lost_names.entry(key).or_default();
                    for l in &c.lines_lost {
                        bump(m, l);
                    }
                    if !c.ok {
                        bump(m, &format!("CONVERSION-ERROR:{}", c.err));
                    }
                    let _ = c.lines_added;
                }
            }
            let _ = writeln!(
                tsv,
                "{fname}\t{ok}\t{}\t{}\t{}\tL{level}\t{}\t{}\t{idem}\t{ast_eq}\t{}\t{}\t{}\t{js_lost}",
                p.objs.len(),
                p.warnings,
                p.errors.len(),
                lost.len(),
                added.len(),
                pcols[0],
                pcols[1],
                pcols[2]
            );
        }
        std::fs::write(format!("results/{name}.tsv"), tsv).unwrap();
        report(&name, &a, non_utf8);
    }
}

fn pct(n: usize, d: usize) -> String {
    if d == 0 {
        "n/a".into()
    } else {
        format!("{n}/{d} ({:.1} %)", 100.0 * n as f64 / d as f64)
    }
}

fn report(name: &str, a: &Agg, non_utf8: usize) {
    let mut s = String::new();
    let _ = writeln!(s, "== {name}: {} files ({non_utf8} non-UTF-8, read lossy), {} objects", a.files, a.objects);
    let _ = writeln!(s, "parse ok             {}   InvalidLine warnings {}   errors {}", pct(a.parse_ok, a.files), a.warnings, a.errors);
    let parsed = a.level.iter().sum::<usize>();
    let _ = writeln!(s, "round-trip L0 byte-identical {}", pct(a.level[0], parsed));
    let _ = writeln!(s, "           L1 same logical lines (fold/EOL only) {}", pct(a.level[1], parsed));
    let _ = writeln!(s, "           L2 same lines, different order {}", pct(a.level[2], parsed));
    let _ = writeln!(s, "           L3 lines changed {}", pct(a.level[3], parsed));
    let _ = writeln!(s, "input logical lines not reproduced verbatim {}", pct(a.lines_lost, a.lines_in));
    let _ = writeln!(s, "X- property lines verbatim {}   X- params verbatim {}", pct(a.xprop.1, a.xprop.0), pct(a.xparam.1, a.xparam.0));
    let _ = writeln!(s, "grouped vCard lines verbatim {}   VTIMEZONE blocks verbatim {}", pct(a.group.1, a.group.0), pct(a.vtz.1, a.vtz.0));
    let _ = writeln!(s, "property order kept {}   idempotent (rt(rt)==rt) {}   AST(rt)==AST(in) {}", pct(a.order_ok, parsed), pct(a.idempotent, parsed), pct(a.ast_eq, parsed));
    for (op, (n, ci, cr, raw, head, rv)) in &a.patch {
        let _ = writeln!(s, "patch {op:8} n={n}: AST route: rest byte-equal to input {}, rest equal to calcard rt {}", pct(*ci, *n), pct(*cr, *n));
        let _ = writeln!(s, "               raw-splice whole line ok {} (target name/params respelled {}); raw-splice value-only ok {}", pct(*raw, *n), pct(*head, *n), pct(*rv, *n));
    }
    for (k, v) in &a.raw_refusals {
        let _ = writeln!(s, "  raw-splice not ok x{v}: {k}");
    }
    for (k, (n, ok, l, lost, props)) in &a.conv {
        let _ = writeln!(s, "{k:20} objs={n} converted+back {}  X->JS->X vs calcard rt: lines not verbatim {}, properties gone {}", pct(*ok, *n), pct(*lost, *l), pct(*props, *l));
        let mut v: Vec<_> = a.conv_lost_props[k].iter().collect();
        v.sort_by(|x, y| y.1.cmp(x.1));
        let top: Vec<String> = v.iter().take(12).map(|(k, n)| format!("{k}x{n}")).collect();
        let _ = writeln!(s, "{:20} properties gone: {}", "", top.join(" "));
    }
    print!("{s}");
    let mut lost = s.clone();
    let _ = writeln!(lost, "\n-- round-trip: lost input lines by property (count, first example)");
    let mut v: Vec<_> = a.lost_names.iter().collect();
    v.sort_by(|x, y| y.1 .0.cmp(&x.1 .0));
    for (k, (n, ex)) in v.iter().take(60) {
        let _ = writeln!(lost, "{n:6}  {k:28} {ex}");
    }
    for (conv, m) in &a.conv_lost_names {
        let _ = writeln!(lost, "\n-- {conv}: calcard-rt lines lost after conversion and back (count, first example)");
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|x, y| y.1 .0.cmp(&x.1 .0));
        for (k, (n, ex)) in v.iter().take(40) {
            let _ = writeln!(lost, "{n:6}  {k:28} {ex}");
        }
    }
    std::fs::write(format!("results/{name}.lost.txt"), lost).unwrap();
}

fn expand(files: &[String]) {
    for f in files {
        let raw = std::fs::read_to_string(f).unwrap();
        let p = parse_all(&raw);
        for o in &p.objs {
            let Obj::Ical(c) = o else { continue };
            if !raw.contains("RRULE") && !raw.contains("RDATE") {
                continue;
            }
            let t = Instant::now();
            let x = c.expand_dates(chrono_tz::Tz::UTC, 1000);
            let first: Vec<String> = x.events.iter().take(6).map(|e| format!("{}#{}", e.start.format("%Y-%m-%dT%H:%M%z"), e.comp_id)).collect();
            println!(
                "{}: {} instances, {} errors, {:?}; first: {}",
                Path::new(f).file_name().unwrap().to_string_lossy(),
                x.events.len(),
                x.errors.len(),
                t.elapsed(),
                first.join(" ")
            );
            for e in x.errors.iter().take(3) {
                println!("   error: {e:?}");
            }
        }
    }
}

const VTZ: &str = "BEGIN:VTIMEZONE\r\nTZID:Europe/Prague\r\nX-LIC-LOCATION:Europe/Prague\r\nBEGIN:DAYLIGHT\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200\r\nTZNAME:CEST\r\nDTSTART:19700329T020000\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\nEND:DAYLIGHT\r\nBEGIN:STANDARD\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nTZNAME:CET\r\nDTSTART:19701025T030000\r\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n";

fn gen_event(i: usize) -> String {
    let mut e = String::with_capacity(900);
    let (d, h) = (i % 28 + 1, i % 14 + 7);
    let _ = write!(
        e,
        "BEGIN:VEVENT\r\nUID:{i:08}-bench@musubi.test\r\nDTSTAMP:20260101T120000Z\r\nDTSTART;TZID=Europe/Prague:202603{d:02}T{h:02}0000\r\nDTEND;TZID=Europe/Prague:202603{d:02}T{h:02}4500\r\nSUMMARY:Bench event {i} – plánování\r\nDESCRIPTION:Line one of a longer description for event {i} with some Czech t\r\n ext: příliš žluťoučký kůň úpěl ďábelské ódy. Second sentence to make it fo\r\n ld twice.\\nAgenda: 1) status 2) risks 3) next steps\r\nLOCATION:Room {i}\r\nORGANIZER;CN=Org {i}:mailto:org{i}@example.com\r\nATTENDEE;CN=A {i};ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;RSVP=TRUE:mailto:a{i}@example.com\r\nATTENDEE;CN=B {i};ROLE=OPT-PARTICIPANT;PARTSTAT=NEEDS-ACTION:mailto:b{i}@example.com\r\nX-MUSUBI-TRACE;X-SRC=bench:{i}\r\n"
    );
    if i % 10 == 0 {
        e.push_str("RRULE:FREQ=WEEKLY;COUNT=10;BYDAY=MO,WE\r\n");
    }
    e.push_str("END:VEVENT\r\n");
    e
}

fn gen_card(i: usize) -> String {
    format!("BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//musubi//bench//EN\r\nN:Novák{i};Jan;;;\r\nFN:Jan Novák {i}\r\nORG:Firma {i}\r\nitem1.TEL;type=CELL;type=VOICE;type=pref:+420 777 {:06}\r\nitem1.X-ABLabel:mobil\r\nEMAIL;type=INTERNET;type=WORK:jan{i}@example.com\r\nADR;type=HOME:;;Ulice {i};Brno;;60200;Czechia\r\nNOTE:Poznámka {i}\\, s čárkou\r\nUID:{i:08}-card@musubi.test\r\nX-ABUID:{i:08}:ABPerson\r\nEND:VCARD\r\n", i % 1_000_000)
}

fn best<T>(runs: usize, mut f: impl FnMut() -> T) -> (f64, T) {
    let mut best = f64::MAX;
    let mut last = None;
    for _ in 0..runs {
        let t = Instant::now();
        let r = f();
        best = best.min(t.elapsed().as_secs_f64());
        last = Some(r);
    }
    (best, last.unwrap())
}

fn bench(n: usize) {
    let events: Vec<String> = (0..n).map(gen_event).collect();
    let big = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//musubi//bench//EN\r\n{VTZ}{}END:VCALENDAR\r\n", events.concat());
    let singles: Vec<String> = events.iter().map(|e| format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//musubi//bench//EN\r\n{VTZ}{e}END:VCALENDAR\r\n")).collect();
    let single_bytes: usize = singles.iter().map(String::len).sum();
    let cards: String = (0..n).map(gen_card).collect();
    let mb = |b: usize, s: f64| b as f64 / 1e6 / s;
    println!("bench n={n} (best of 3, release build)");

    let (t, cal) = best(3, || ICalendar::parse(&big).unwrap());
    println!("iCal one VCALENDAR  {:6.1} MB  parse {:7.3} s  {:6.1} MB/s  {:5.1} µs/event  components={}", big.len() as f64 / 1e6, t, mb(big.len(), t), t * 1e6 / n as f64, cal.components.len());
    let (t, out) = best(3, || cal.to_string());
    println!("                    serialize {:7.3} s  {:6.1} MB/s  byte-identical={}", t, mb(out.len(), t), out == big);
    let (t, _) = best(3, || {
        let objs: Vec<ICalendar> = singles.iter().map(|s| ICalendar::parse(s).unwrap()).collect();
        objs.len()
    });
    println!("iCal {n} resources  {:6.1} MB  parse {:7.3} s  {:6.1} MB/s  {:5.1} µs/resource (each with VTIMEZONE)", single_bytes as f64 / 1e6, t, mb(single_bytes, t), t * 1e6 / n as f64);
    let parsed: Vec<ICalendar> = singles.iter().map(|s| ICalendar::parse(s).unwrap()).collect();
    let (t, _) = best(3, || parsed.iter().map(|c| c.clone().into_jscalendar::<String, String>()).count());
    println!("                    into_jscalendar {:7.3} s  {:5.1} µs/resource (incl. clone)", t, t * 1e6 / n as f64);
    let (t, x) = best(1, || cal.expand_dates(chrono_tz::Tz::UTC, usize::MAX));
    println!("                    expand_dates(all) {:7.3} s  instances={} errors={}", t, x.events.len(), x.errors.len());

    let (t, k) = best(3, || {
        let mut p = Parser::new(&cards);
        let mut k = 0;
        while let Entry::VCard(_) = p.entry() {
            k += 1;
        }
        k
    });
    println!("vCard 3.0 x{k}       {:6.1} MB  parse {:7.3} s  {:6.1} MB/s  {:5.1} µs/card", cards.len() as f64 / 1e6, t, mb(cards.len(), t), t * 1e6 / n as f64);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("corpus") => corpus(&args[1..]),
        Some("expand") => expand(&args[1..]),
        Some("rt") => args[1..].iter().for_each(|f| print!("{}", write_all(&parse_all(&std::fs::read_to_string(f).unwrap()).objs))),
        Some("js") => {
            for o in parse_all(&std::fs::read_to_string(&args[1]).unwrap()).objs {
                let (json, back) = js_round(&o, args.get(2).map(String::as_str) != Some("--no-raw"));
                println!("{json}\n{}", back.unwrap_or_else(|e| e));
            }
        }
        Some("bench") => bench(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(50_000)),
        _ => eprintln!("usage: calcard-fidelity corpus <dir>... | expand <file>... | bench [n]"),
    }
    let _ = VCardVersion::V4_0;
}
