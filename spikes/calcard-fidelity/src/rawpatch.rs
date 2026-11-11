//! Prototype of a thin raw-patching layer on top of calcard.
//!
//! Idea: calcard parses, Musubi edits the typed entry, but only the bytes of the edited logical
//! line are replaced in the original payload. Everything else stays byte-identical.
//! Alignment between raw lines and calcard entries is by order inside each component and is
//! verified by property name; any mismatch refuses the patch (caller falls back / reports).

use calcard::icalendar::{ICalendar, ICalendarEntry};
use calcard::vcard::{VCard, VCardEntry, VCardProperty, VCardVersion};

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

/// Value-only rewrite: keep the original name/params spelling verbatim and take only the value
/// part from calcard's serialisation. Valid when only the value changed (params untouched).
pub fn value_only_line(orig: &RawLine, serialized_crlf: &str) -> Option<String> {
    let new = logical_lines(serialized_crlf).into_iter().next()?.text;
    let head = &orig.text[..value_colon(&orig.text)?];
    Some(fold(&format!("{head}{}", &new[value_colon(&new)?..])))
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
    fn value_only_keeps_head_and_folds_on_octets() {
        let orig = &logical_lines("item1.TEL;type=CELL;type=VOICE:+420 1\r\n")[0];
        assert_eq!(value_only_line(orig, "item1.TEL;TYPE=CELL,VOICE:+420 2\r\n").unwrap(), "item1.TEL;type=CELL;type=VOICE:+420 2\r\n");
        let long = format!("X;P=\"a:b\":{}\r\n", "ž".repeat(60));
        let f = value_only_line(&logical_lines(&long)[0], &long).unwrap();
        assert!(f.split("\r\n").all(|l| l.len() <= 75));
        assert_eq!(logical_lines(&f)[0].text, logical_lines(&long)[0].text);
    }
}
