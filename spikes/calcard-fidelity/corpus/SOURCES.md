# Corpus provenance

| Directory | Files | Origin | Licence notes |
|---|---|---|---|
| `musubi/` | 63 `.ics` | Extracted by `tools/extract_musubi.py` from the read-only Musubi repo (`/home/flamendrin/workspace/musubi`): JSON behaviour scenarios and TS test fixtures, deduplicated by SHA-256. No vCards exist there. Names are `<origin path>_<hash>`. | Musubi's own test data (same owner). |
| `synth/` | 16 `.ics` + 9 `.vcf` | Hand-written for this spike in the *style* of each producer (Google, Outlook/Exchange, iCloud, Nextcloud/Sabre, Thunderbird, Outlook vCard 2.1), plus examples reconstructed from RFC 5545 / 6350 / 2426, plus edge cases (`edge_*`). None was downloaded from or exported out of a real account; all addresses, UIDs and handles are fictitious. | Written for this repo. RFC examples are short excerpts or paraphrases. |
| calcard `resources/ical`, `resources/vcard` | 998 `.ics` + 244 `.vcf` | Read **in place** from the cargo registry (`~/.cargo/registry/src/*/calcard-0.3.14/resources/`), **not vendored**. Their provenance is in calcard's `resources/SOURCES.txt`: biweekly, CalDAVTester, Apple CCS, Cyrus, Evolution, FolkerKinzel.VCards, golang-ical, Home Assistant, ical4j, libical, Nextcloud, Radicale, sabre/vobject, Roundcube, Zimbra and others. | Mixed upstream licences. Several are **copyleft**: Evolution Data Server (LGPL), Nextcloud Server, Roundcube and Zimbra (GPL/AGPL family; per-file licence not checked). They are only read as test input and never redistributed, which is why they are not copied here. |

To regenerate `musubi/`: `python3 -I tools/extract_musubi.py /home/flamendrin/workspace/musubi corpus/musubi`
