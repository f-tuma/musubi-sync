//! iCalendar and vCard codecs of musubi-sync over `calcard` (docs/plan/phase2.md,
//! chapters 2 and 3.7).
//!
//! Pure functions without I/O. Phase 2 K0 delivers [`kind_of`]; K2 adds the
//! codecs (VEVENT → [`musubi_model::CalendarEvent`], VTODO →
//! [`musubi_model::CalendarTask`], vCard → [`musubi_model::ContactCard`] and
//! [`musubi_model::ContactGroupCard`]) and the zone chain `tz`.

use musubi_model::EntityKind;
use musubi_model::remote::{MediaType, Raw};

/// The kind of one CalDAV or CardDAV resource, found by scanning its content
/// lines without parsing the object.
///
/// - `text/calendar`: [`EntityKind::Event`] if it has a `VEVENT`, else
///   [`EntityKind::Task`] if it has a `VTODO`, else `None` (`VJOURNAL`,
///   `VFREEBUSY`, no component).
/// - `text/vcard`: [`EntityKind::ContactGroup`] for `KIND:group` (vCard 4) or
///   Apple's `X-ADDRESSBOOKSERVER-KIND:group` (vCard 3), else
///   [`EntityKind::Contact`]; `None` without `BEGIN:VCARD`.
/// - Other media types: `None`.
///
/// Names and values compare without regard to case; folded lines are unfolded.
pub fn kind_of(raw: &Raw) -> Option<EntityKind> {
    match raw.media_type {
        MediaType::Calendar => calendar_kind(&raw.body),
        MediaType::VCard => card_kind(&raw.body),
        _ => None,
    }
}

fn calendar_kind(body: &[u8]) -> Option<EntityKind> {
    let mut todo = false;
    for line in lines(body) {
        let (name, value) = split(&line);
        if name.eq_ignore_ascii_case(b"BEGIN") {
            if value.eq_ignore_ascii_case(b"VEVENT") {
                return Some(EntityKind::Event);
            }
            todo |= value.eq_ignore_ascii_case(b"VTODO");
        }
    }
    todo.then_some(EntityKind::Task)
}

fn card_kind(body: &[u8]) -> Option<EntityKind> {
    let mut card = false;
    for line in lines(body) {
        let (name, value) = split(&line);
        // `item1.KIND`: a property group prefix does not change the property.
        let name = name.rsplit(|&b| b == b'.').next().unwrap_or(name);
        if name.eq_ignore_ascii_case(b"BEGIN") {
            card |= value.eq_ignore_ascii_case(b"VCARD");
        } else if card
            && (name.eq_ignore_ascii_case(b"KIND")
                || name.eq_ignore_ascii_case(b"X-ADDRESSBOOKSERVER-KIND"))
            && value.eq_ignore_ascii_case(b"group")
        {
            return Some(EntityKind::ContactGroup);
        }
    }
    card.then_some(EntityKind::Contact)
}

/// Unfolded content lines (RFC 5545, section 3.1; RFC 6350, section 3.2).
fn lines(body: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
    let body = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body);
    let mut raw = body
        .split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .peekable();
    std::iter::from_fn(move || {
        let mut line = raw.next()?.to_vec();
        while let Some(next) = raw.next_if(|l| l.starts_with(b" ") || l.starts_with(b"\t")) {
            line.extend_from_slice(&next[1..]);
        }
        Some(line)
    })
}

/// Property name (without parameters) and value, trimmed. The value starts
/// at the first `:` outside a quoted parameter value.
fn split(line: &[u8]) -> (&[u8], &[u8]) {
    let mut quoted = false;
    let colon = line.iter().position(|&b| {
        quoted ^= b == b'"';
        b == b':' && !quoted
    });
    let (head, value) = match colon {
        Some(i) => (&line[..i], &line[i + 1..]),
        None => (line, &[][..]),
    };
    let name = head.split(|&b| b == b';').next().unwrap_or(head);
    (name.trim_ascii(), value.trim_ascii())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(media_type: MediaType, body: &str) -> Option<EntityKind> {
        kind_of(&Raw {
            media_type,
            body: body.as_bytes().to_vec(),
        })
    }

    fn cal(body: &str) -> Option<EntityKind> {
        kind(MediaType::Calendar, body)
    }

    fn card(body: &str) -> Option<EntityKind> {
        kind(MediaType::VCard, body)
    }

    const TZ: &str = "BEGIN:VTIMEZONE\r\nTZID:Europe/Prague\r\nBEGIN:STANDARD\r\n\
        DTSTART:19701025T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\n\
        END:STANDARD\r\nEND:VTIMEZONE\r\n";

    #[test]
    fn calendar_objects() {
        let event = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{TZ}BEGIN:VEVENT\r\nUID:e1\r\n\
             DTSTART;TZID=Europe/Prague:20261113T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        assert_eq!(cal(&event), Some(EntityKind::Event));
        // A task with an alarm: the nested VALARM does not change the kind.
        let todo = "BEGIN:VCALENDAR\nBEGIN:VTODO\nUID:t1\nBEGIN:VALARM\nACTION:DISPLAY\n\
                    END:VALARM\nEND:VTODO\nEND:VCALENDAR\n";
        assert_eq!(cal(todo), Some(EntityKind::Task));
        assert_eq!(
            cal("begin:vcalendar\r\nbegin:vtodo\r\nend:vtodo\r\nend:vcalendar\r\n"),
            Some(EntityKind::Task),
            "names compare without case"
        );
        for skipped in ["VJOURNAL", "VFREEBUSY"] {
            let body = format!(
                "BEGIN:VCALENDAR\r\nBEGIN:{skipped}\r\nUID:x\r\nEND:{skipped}\r\nEND:VCALENDAR\r\n"
            );
            assert_eq!(cal(&body), None, "{skipped}");
        }
        assert_eq!(
            cal(&format!("BEGIN:VCALENDAR\r\n{TZ}END:VCALENDAR\r\n")),
            None
        );
        assert_eq!(cal(""), None);
    }

    #[test]
    fn a_folded_begin_line_is_unfolded() {
        assert_eq!(
            cal("BEGIN:VCALENDAR\r\nBEGIN:VEV\r\n ENT\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"),
            Some(EntityKind::Event)
        );
    }

    #[test]
    fn property_values_that_look_like_components_do_not_count() {
        let body = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nSUMMARY:BEGIN:VEVENT\r\n\
                    DESCRIPTION;X-NOTE=\"a:b\":BEGIN:VEVENT\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert_eq!(cal(body), Some(EntityKind::Task));
    }

    #[test]
    fn vcards() {
        let v3 = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Jan Novák\r\nN:Novák;Jan;;;\r\nEND:VCARD\r\n";
        assert_eq!(card(v3), Some(EntityKind::Contact));
        let v4 = "BEGIN:VCARD\nVERSION:4.0\nKIND:individual\nFN:Jan\nEND:VCARD\n";
        assert_eq!(card(v4), Some(EntityKind::Contact));
        let group4 = "BEGIN:VCARD\r\nVERSION:4.0\r\nKIND:group\r\nFN:Family\r\n\
                      MEMBER:urn:uuid:03a0e51f-d1aa-4385-8a53-e29025acd8af\r\nEND:VCARD\r\n";
        assert_eq!(card(group4), Some(EntityKind::ContactGroup));
        let apple = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Family\r\nFN:Family\r\n\
                     X-ADDRESSBOOKSERVER-KIND:group\r\n\
                     X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:1\r\nEND:VCARD\r\n";
        assert_eq!(card(apple), Some(EntityKind::ContactGroup));
        assert_eq!(
            card("BEGIN:VCARD\r\nVERSION:4.0\r\nitem1.kind;VALUE=text:Group\r\nEND:VCARD\r\n"),
            Some(EntityKind::ContactGroup),
            "group prefix, parameters and case"
        );
        assert_eq!(
            card("BEGIN:VCARD\r\nNOTE:KIND:group\r\nEND:VCARD\r\n"),
            Some(EntityKind::Contact),
            "a value is not a property"
        );
        assert_eq!(
            card("\u{feff}BEGIN:VCARD\r\nFN:x\r\nEND:VCARD\r\n"),
            Some(EntityKind::Contact)
        );
        assert_eq!(card("FN:x\r\n"), None);
    }

    #[test]
    fn media_type_decides_the_grammar() {
        let vevent = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(kind(MediaType::Json, vevent), None);
        assert_eq!(card(vevent), None);
        assert_eq!(cal("BEGIN:VCARD\r\nKIND:group\r\nEND:VCARD\r\n"), None);
    }
}
