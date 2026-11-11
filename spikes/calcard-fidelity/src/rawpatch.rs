//! Prototype of a thin raw-patching layer on top of calcard.
//!
//! Idea: calcard parses, Musubi edits the typed entry, but only the bytes of the edited logical
//! line are replaced in the original payload. Everything else stays byte-identical.
//! Alignment between raw lines and calcard entries is by order inside each component and is
//! verified by property name; any mismatch refuses the patch (caller falls back / reports).
//! Every splice is re-parsed and must yield exactly the expected AST, otherwise it is refused.

use calcard::icalendar::{ICalendar, ICalendarEntry};
use calcard::vcard::{VCard, VCardEntry, VCardProperty, VCardVersion};
use calcard::{Entry, Parser};

/// A top-level object as calcard parses it.
#[derive(Clone, PartialEq, Debug)]
pub enum Obj {
    Ical(ICalendar),
    Card(VCard),
}

/// All top-level objects calcard finds in a payload (invalid lines and errors skipped).
pub fn parse_objects(text: &str) -> Vec<Obj> {
    let mut p = Parser::new(text);
    let mut out = Vec::new();
    loop {
        match p.entry() {
            Entry::ICalendar(c) => out.push(Obj::Ical(c)),
            Entry::VCard(c) => out.push(Obj::Card(c)),
            Entry::Eof => return out,
            _ => {}
        }
    }
}

/// One logical (unfolded) content line of the raw payload.
#[derive(Debug, Clone)]
pub struct RawLine {
    /// Byte span in the raw text, including the folded continuations and the final EOL.
    pub start: usize,
    pub end: usize,
    /// Unfolded content without EOL.
    pub text: String,
}

impl RawLine {
    /// Property name without vCard group, upper-cased ("item1.TEL;..." -> "TEL").
    pub fn name(&self) -> String {
        let head = self.text.split([':', ';']).next().unwrap_or("");
        head.rsplit('.').next().unwrap_or("").to_ascii_uppercase()
    }
    pub fn begin_end(&self) -> Option<(bool, String)> {
        let up = self.text.to_ascii_uppercase();
        if let Some(c) = up.strip_prefix("BEGIN:") {
            Some((true, c.trim().to_string()))
        } else {
            up.strip_prefix("END:").map(|c| (false, c.trim().to_string()))
        }
    }
}

/// Split raw text into logical lines (RFC 5545 3.1 / RFC 6350 3.2 unfolding: CRLF or LF
/// followed by one space/tab). vCard 2.1 quoted-printable soft breaks are not handled.
pub fn logical_lines(raw: &str) -> Vec<RawLine> {
    let mut out: Vec<RawLine> = Vec::new();
    let mut pos = 0;
    for phys in raw.split_inclusive('\n') {
        let content = phys.trim_end_matches(['\r', '\n']);
        let is_cont = (content.starts_with(' ') || content.starts_with('\t')) && !out.is_empty();
        if is_cont {
            let last = out.last_mut().unwrap();
            last.text.push_str(&content[1..]);
            last.end = pos + phys.len();
        } else if !content.is_empty() {
            out.push(RawLine { start: pos, end: pos + phys.len(), text: content.to_string() });
        }
        pos += phys.len();
    }
    out
}

/// Pre-order traversal of calcard's flat component list (index 0 = VCALENDAR).
fn preorder(ical: &ICalendar) -> Vec<u32> {
    let mut order = Vec::new();
    let mut stack = vec![0u32];
    while let Some(id) = stack.pop() {
        order.push(id);
        if let Some(c) = ical.components.get(id as usize) {
            stack.extend(c.component_ids.iter().rev());
        }
    }
    order
}

/// Map every calcard entry (component id, entry index) to its raw line.
/// Returns Err(reason) when the raw structure and the AST do not line up 1:1.
pub fn align_ical(raw: &[RawLine], ical: &ICalendar) -> Result<Vec<((u32, usize), usize)>, String> {
    let order = preorder(ical);
    let mut next_comp = 0usize;
    let mut stack: Vec<(u32, usize)> = Vec::new(); // (component id, next entry index)
    let mut map = Vec::new();
    for (li, line) in raw.iter().enumerate() {
        match line.begin_end() {
            Some((true, name)) => {
                let id = *order.get(next_comp).ok_or("more BEGIN lines than components")?;
                next_comp += 1;
                let ty = ical.components[id as usize].component_type.as_str().to_ascii_uppercase();
                if ty != name {
                    return Err(format!("component mismatch raw={name} ast={ty}"));
                }
                stack.push((id, 0));
            }
            Some((false, _)) => {
                let (id, n) = stack.pop().ok_or("END without BEGIN")?;
                let have = ical.components[id as usize].entries.len();
                if n != have {
                    return Err(format!("entry count mismatch raw={n} ast={have}"));
                }
            }
            None => {
                let Some((id, n)) = stack.last_mut() else { continue }; // junk outside object
                let entry = ical.components[*id as usize]
                    .entries
                    .get(*n)
                    .ok_or_else(|| format!("raw line without entry: {}", line.name()))?;
                if !entry.name.as_str().eq_ignore_ascii_case(&line.name()) {
                    return Err(format!("name mismatch raw={} ast={}", line.name(), entry.name.as_str()));
                }
                map.push(((*id, *n), li));
                *n += 1;
            }
        }
    }
    if next_comp != order.len() {
        return Err("AST has components not present in raw".into());
    }
    Ok(map)
}

/// Same for one vCard; BEGIN/END/VERSION handling mirrors calcard's writer.
pub fn align_vcard(raw: &[RawLine], card: &VCard) -> Result<Vec<(usize, usize)>, String> {
    let entries: Vec<(usize, &VCardEntry)> = card
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !matches!(e.name, VCardProperty::Begin | VCardProperty::End))
        .collect();
    let lines: Vec<(usize, &RawLine)> =
        raw.iter().enumerate().filter(|(_, l)| l.begin_end().is_none()).collect();
    if entries.len() != lines.len() {
        return Err(format!("entry count mismatch raw={} ast={}", lines.len(), entries.len()));
    }
    let mut map = Vec::new();
    for ((ei, e), (li, l)) in entries.iter().zip(lines.iter()) {
        if !e.name.as_str().eq_ignore_ascii_case(&l.name()) {
            return Err(format!("name mismatch raw={} ast={}", l.name(), e.name.as_str()));
        }
        map.push((*ei, *li));
    }
    Ok(map)
}

/// Replace one logical line's bytes with a freshly serialised entry, keeping the payload's EOL.
pub fn splice(raw: &str, line: &RawLine, serialized_crlf: &str) -> String {
    let crlf = raw[line.start..line.end].ends_with("\r\n") || !raw.contains('\n');
    let mut repl = if crlf { serialized_crlf.to_string() } else { serialized_crlf.replace("\r\n", "\n") };
    if !raw[line.start..line.end].ends_with('\n') {
        // last line without EOL: keep it that way
        repl.truncate(repl.trim_end_matches(['\r', '\n']).len());
    }
    format!("{}{}{}", &raw[..line.start], repl, &raw[line.end..])
}

/// Byte index of the first ':' outside double quotes (separates name;params from the value).
fn value_colon(t: &str) -> Option<usize> {
    let mut q = false;
    for (i, c) in t.char_indices() {
        match c {
            '"' => q = !q,
            ':' if !q => return Some(i),
            _ => {}
        }
    }
    None
}

/// Splice `new_line` over `line` and accept the result only if no byte outside the line's span
/// changed and calcard re-parses the whole payload to exactly `expect`.
pub fn checked_splice(raw: &str, line: &RawLine, new_line: &str, expect: &[Obj]) -> Result<String, String> {
    let out = splice(raw, line, new_line);
    let tail = raw.len() - line.end;
    if out.as_bytes()[..line.start] != raw.as_bytes()[..line.start]
        || out.as_bytes()[out.len() - tail..] != raw.as_bytes()[line.end..]
    {
        return Err("verify_fail: bytes outside span changed".into());
    }
    if parse_objects(&out) != expect {
        return Err("verify_fail: re-parse differs from mutated AST".into());
    }
    Ok(out)
}

/// Value-only rewrite: keep the original name/params spelling verbatim and take only the value
/// part from calcard's serialisation. Valid when only the value changed (params untouched).
/// Refused when the original value bytes are encoded (ENCODING / CHARSET / bare 2.1 QP or BASE64):
/// calcard emits the decoded value, which the kept params would make clients decode again.
pub fn value_only_line(orig: &RawLine, serialized_crlf: &str) -> Result<String, String> {
    let colon = value_colon(&orig.text).ok_or("refused: no ':' in line")?;
    let head = &orig.text[..colon];
    if head.split(';').skip(1).any(|p| {
        let n = p.split('=').next().unwrap_or("").trim().to_ascii_uppercase();
        matches!(n.as_str(), "ENCODING" | "CHARSET" | "QUOTED-PRINTABLE" | "BASE64")
    }) {
        return Err("refused: encoded value (ENCODING/CHARSET param)".into());
    }
    let new = logical_lines(serialized_crlf).into_iter().next().ok_or("refused: empty serialisation")?.text;
    let vc = value_colon(&new).ok_or("refused: no ':' in serialisation")?;
    Ok(fold(&format!("{head}{}", &new[vc..])))
}

/// Value-only rewrite of one line, spliced and verified (see `value_only_line`, `checked_splice`).
pub fn patch_value(raw: &str, line: &RawLine, serialized_crlf: &str, expect: &[Obj]) -> Result<String, String> {
    checked_splice(raw, line, &value_only_line(line, serialized_crlf)?, expect)
}

/// Fold at 75 octets on char boundaries, CRLF + single space (RFC 5545 3.1).
pub fn fold(line: &str) -> String {
    let (mut out, mut len) = (String::new(), 0);
    for c in line.chars() {
        if len + c.len_utf8() > 75 {
            out.push_str("\r\n ");
            len = 1;
        }
        out.push(c);
        len += c.len_utf8();
    }
    out.push_str("\r\n");
    out
}

pub fn write_ical_entry(e: &ICalendarEntry) -> String {
    let mut s = String::new();
    e.write_to(&mut s).unwrap();
    s
}

pub fn write_vcard_entry(e: &VCardEntry, v: VCardVersion) -> String {
    let mut s = String::new();
    e.write_to(&mut s, matches!(v, VCardVersion::V4_0)).unwrap();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_touches_only_target_line() {
        let raw = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nX-A;X-P=\"a,b\":1\r\nSUMMARY:old\r\n  folded\r\nUID:u\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let ical = ICalendar::parse(raw).unwrap();
        let lines = logical_lines(raw);
        let map = align_ical(&lines, &ical).unwrap();
        let ((cid, ei), li) = map.iter().find(|(_, li)| lines[*li].name() == "SUMMARY").unwrap();
        let mut e = ical.components[*cid as usize].entries[*ei].clone();
        e.values = vec![calcard::icalendar::ICalendarValue::Text("new".into())];
        let out = splice(raw, &lines[*li], &write_ical_entry(&e));
        assert_eq!(out, raw.replace("SUMMARY:old\r\n  folded\r\n", "SUMMARY:new\r\n"));
    }

    #[test]
    fn single_line_qp_is_refused_not_corrupted() {
        // Regression: value-only splice used to keep ENCODING=QUOTED-PRINTABLE over a decoded value,
        // so "ext=12 café" was read back by clients as "ext\x12 cafÃ©".
        let raw = "BEGIN:VCARD\r\nVERSION:2.1\r\nN:Doe;Jan\r\nTEL;CELL;ENCODING=QUOTED-PRINTABLE:+420=20123\r\nEND:VCARD\r\n";
        let Obj::Card(card) = parse_objects(raw).remove(0) else { panic!() };
        let lines = logical_lines(raw);
        let (ei, li) = *align_vcard(&lines, &card).unwrap().iter().find(|(_, li)| lines[*li].name() == "TEL").unwrap();
        let mut patched = card.clone();
        patched.entries[ei].values = vec![calcard::vcard::VCardValue::Text("ext=12 café".into())];
        let expect = [Obj::Card(patched.clone())];
        let ser = write_vcard_entry(&patched.entries[ei], VCardVersion::V2_1);
        let err = patch_value(raw, &lines[li], &ser, &expect).unwrap_err();
        assert!(err.contains("encoded value"), "{err}");
        // The bad line the old code produced must also be caught by the re-parse check.
        let bad = "TEL;CELL;ENCODING=QUOTED-PRINTABLE:ext=12 café\r\n";
        assert!(checked_splice(raw, &lines[li], bad, &expect).unwrap_err().starts_with("verify_fail"));
    }

    #[test]
    fn value_only_keeps_head_and_folds_on_octets() {
        let orig = &logical_lines("item1.TEL;type=CELL;type=VOICE:+420 1\r\n")[0];
        assert_eq!(value_only_line(orig, "item1.TEL;TYPE=CELL,VOICE:+420 2\r\n").unwrap(), "item1.TEL;type=CELL;type=VOICE:+420 2\r\n");
        let long = format!("X;P=\"a:b\":{}\r\n", "ž".repeat(60));
        let f = value_only_line(&logical_lines(&long)[0], &long).unwrap();
        assert!(f.split("\r\n").all(|l| l.len() <= 75));
        assert_eq!(logical_lines(&f)[0].text, logical_lines(&long)[0].text);
    }
}
