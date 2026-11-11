# Phase 0: engine pro opakování (RRULE/RDATE/EXDATE)

## Otázka

Má musubi-sync rozbalovat opakování převzatou knihovnou `rrule`, některým forkem (`rrule2`, port na `jiff` z PR #151, rozbalení v `calcard-next`), nebo vlastním modulem? Rozhoduje korektnost. Výkon a paměť jsou až druhotné.

## Shrnutí

- **Verdikt: vlastní modul `musubi-recurrence` za interním rozhraním. Jako jádro převezmeme (vendor) iterátor z vydaného `rrule` 0.14 a obalíme ho vlastní časovou vrstvou nad jiff („hranou“). Jádro počítá jen v civilním čase označeném jako UTC a zóny nikdy nevidí.** Pokud bude tvrdým požadavkem odstranit chrono ze stromu závislostí, za stejnou hranu dáme port na jiff (c). Naměřené výsledky má stejné, ale musíme si sami zrevidovat jeho jediný nevydaný commit. Podrobnosti jsou v kapitole Verdikt.
- Všechna tři zvažovaná jádra jsou jeden algoritmus. `calcard-next/datecalc` je iterátor z `rust-rrule` přepsaný na jiff (SPDX hlavička „2021 Fredrik Meringdal, Ralph Bisschops“) a port z PR #151 je totéž. Liší se tedy hlavně okraji (zóny, DTSTART, UNTIL), provázaností a původem, ne aritmetikou.
- Když odečteme případy politiky a zón, mají v korpusu 0 chyb jádra tito kandidáti: `calcard-next`, `rrule 0.14 + naše hrana` a `port na jiff + naše hrana`. Samotný port na jiff má 1 chybu (`dst-prague-explicit-gap-dtstart`), `rrule2` 2, `rrule` 3 a `calcard` 13.
- Pro vlastní politiku musíme mít obal tak jako tak. Nejhorší selhání jsou tichá:
  - vlastní VTIMEZONE se u calcard kandidátů tiše změní na plovoucí čas;
  - EXDATE typu DATE se tiše ignoruje;
  - `rrule`/`rrule2` bez hrany přenáší zónu hostitele do plovoucích časů.
- Výkon problém není:
  - 10 000 sérií, výskyty v okně 2026–2031 (3,73 mil.): 2,2–3,1 s na běh včetně formátování na řetězce, peak RSS 13,2 MB;
  - rodina `rrule` nemá okenní API, rozbaluje od DTSTART a vyrobí 6,05 mil. výskytů;
  - zhruba polovinu času zabere formátování v harnessu.
- Zóny: **jiff** se systémovou nebo přibalenou tzdb, ne `chrono-tz`. `chrono-tz` 0.10.4 má zakompilovanou tzdb 2025b a od tzdb 2026f se liší ve 22 zónách. U 13 z nich jde o změny platné od září nebo listopadu 2026.

## Metoda

1. **Korpus** (`spikes/recurrence/corpus/build_corpus.py`) má 89 případů s tagy:
   - DST (gap a fold) v Praze, New Yorku a Lord Howe (posun o 30 minut), historické zóny (Apia, která vynechala 30. 12. 2011, a další);
   - BYSETPOS, BYWEEKNO, BYYEARDAY, záporný BYMONTHDAY, přestupné dny, WKST:
     - u BYWEEKNO i týden 53 a záporné hodnoty;
     - případ `byweekno-53` při COUNT=3 pokrývá roky 1998, 2004 a 2009; další roky s týdnem 53 jsou 2015, 2020 a 2026;
   - UNTIL v UTC, lokálně (nevalidní) a jako DATE; COUNT;
   - celodenní a plovoucí časy;
   - EXDATE s TZID, celodenní EXDATE, EXDATE typu DATE na časované sérii, RDATE typu PERIOD, samotné RDATE, EXRULE;
   - YEARLY bez BYMONTH;
   - přepisy RECURRENCE-ID včetně THISANDFUTURE a přepisu RDATE výskytu;
   - nekonečné řady s oknem a nemožné pravidlo (30. února);
   - VTIMEZONE s vlastními pravidly, Windows názvy zón a starý Mozilla prefix TZID.

   17 případů pochází přímo z Musubi (read-only):
   - testy;
   - `apps/api/src/sync/adapters/google.ts` (`sanitizeRecurrence`);
   - `packages/calendar/src/time-expansion.test.ts`;
   - `docs/sync/caldav-date-export.md`, `docs/sync/caldav-rdate-writes.md`.
2. **Orákula:**
   - **dateutil** (`oracles/dateutil_oracle.py`): python-dateutil 2.9.0.post0 (`rruleset`). ICS čte vlastní parser na regulárních výrazech (unfold, komponenty, vlastnosti). TZID řeší přes systémový `zoneinfo` (tzdb 2026f) a vložené VTIMEZONE přes `dateutil.tz.tzical`. DTSTART přidává přes `rset.rdate(dtstart)`. `requirements.txt` připíná jen `python-dateutil`. Knihovnu icalendar ani balík tzdata orákulum nepoužívá.
   - **ical.js** 2.2.1 s VTIMEZONE vygenerovanými z tzdb skriptem `oracles/gen_vtimezones.py`. Výstup `vtimezones.json` (155 KB) vytváří `run.sh` při každém běhu, proto není v gitu.

   Kde se orákula shodla, převzali jsme výsledek (59 případů). Ostatní jsme rozhodli ručně podle textu RFC 5545 (24 případů) nebo podle politiky Musubi (6 případů), každý s odůvodněním v `corpus/expected.json` (pole `why`). U nejednoznačných míst RFC přijímáme obě čtení.
3. **Kandidáti**, jeden Rust binár `candidates/`:
   - `rrule` 0.14.0 a `rrule2` 0.15.0 (obojí chrono + chrono-tz);
   - port na jiff (`ccope/rust-rrule` @ `50db3e2`, uzavřený nesloučený PR #151);
   - `calcard` 0.3.14 (`expand_dates`);
   - `calcard-next` 3.0.1 (`expand_dates_between`);
   - **`rrule+edge`, `rrule-jiff+edge`**: jádra `rrule` 0.14 a portu za naší hranou (`edge_expand` v `candidates/src/main.rs`, ~120 řádků). Hrana funguje takto:
     - DTSTART předá jádru jako civilní čas se Z, takže jádro počítá bez DST;
     - UNTIL převede do téhož civilního času;
     - každý výsledek převede na okamžik přes jiff `TimeZone::to_zoned`, tedy podle RFC 3.3.5 (fold → první výskyt, gap → offset před gapem);
     - RDATE a EXDATE uplatní až nad okamžiky.

     Stejný postup používá `calcard-next` v `icalendar/dates.rs`. TZID hrana v prototypu hledá jen v IANA databázi. Řešení TZID patří do vrstvy zón (viz Strategie).

   Přepisy RECURRENCE-ID u rodiny `rrule` aplikuje harness, protože tyto knihovny je neumí. calcard a calcard-next je zpracují samy.
4. **Rozdělení selhání** (`tools/core_failures.py`):
   - do vrstvy politiky a zón patří 9 případů: `until-before-dtstart`, `dtstart-not-in-rule`, `rdate-only`, `rdate-period`, `exdate-date-on-timed`, `exrule`, `vtimezone-custom-rules`, `vtimezone-windows-name`, `vtimezone-mozilla-prefix`;
   - zbytek jsou chyby jádra.
5. **Velikost a provázanost jádra** (`tools/core_size.py` → `results/core_size.json`):
   - počítá řádky kódu bez prázdných řádků, komentářů a `#[cfg(test)]` modulů;
   - eviduje použité externí crates a cesty do zbytku crate mimo jádro.
6. **Výkon:**
   - 10 000 syntetických sérií vznikne kombinací 8 typů pravidel × 4 zón (Praha, New York, Lord Howe, Tokio);
   - DTSTART spadají do let 2020–2026, každá pátá série má EXDATE;
   - okno je 2026-01-01 + 5 let;
   - každý kandidát běží 3×, peak RSS se měří přes `wait4` a základem je běh `parse-only`;
   - harness navíc zvlášť změří, kolik trvá naformátovat stejný počet výskytů (`fmt_ms`). Rozdíl vůči celkovému času je `engine_ms_est`.
7. **Zóny:** `tzcheck` porovná všech 597 zón z `chrono-tz` s `jiff` v krocích po 12 h v letech 1970–2038.

Celé měření běží s `TZ=Europe/Prague`, aby se ukázal únik zóny hostitele.

## Reprodukce

```sh
cd spikes/recurrence
./run.sh            # korpus, orákula, kandidáti, tzcheck, core_failures, core_size -> results/
./run.sh --bench    # navíc výkon -> results/bench.json
```

Potřeba je python3, node/npm a cargo. Verze jsou připnuté (`requirements.txt`, `package-lock.json`, `Cargo.lock`). Harness slouží jako regresní sada: nový případ se přidá do `build_corpus.py` a jeho očekávání do `expected.json`.

## Výsledky

### Korpus (89 případů)

| engine | pass | fail | error | chyby jádra* | poznámka |
|---|---|---|---|---|---|
| **calcard-next 3.0.1** | **84** | 4 | 1 | **0** | okno, přepisy a jiff jsou vestavěné |
| **rrule 0.14 + naše hrana** | 83 | 2 | 4 | **0** | prototyp hrany v harnessu |
| **port na jiff + naše hrana** | 83 | 2 | 4 | **0** | |
| rrule (port na jiff) | 81 | 4 | 4 | 1 | uzavřený PR, bez vydání |
| rrule2 0.15.0 | 80 | 5 | 4 | 2 | |
| rrule 0.14.0 | 79 | 4 | 6 | 3 | |
| calcard 0.3.14 | 71 | 16 | 2 | 13 | rozbalovat umí, ale bez okna a s chybami v DST |
| ical.js 2.2.1 (orákulum) | 65 | 23 | 1 | 19 | BYWEEKNO, BYSETPOS, YEARLY+BYDAY a přestupné dny špatně |
| dateutil (orákulum) | 88 | 1 | 0 | 0 | skóre je zčásti kruhové: očekávání byla nasazena z orákul |

\* Selhání mimo 9 případů politiky a zón (`tools/core_failures.py`):

- port na jiff: `dst-prague-explicit-gap-dtstart`;
- rrule2: to samé a `floating-daily-host-gap`;
- rrule: navíc `rdate-gap-and-fold`.

Všechna zbylá selhání obou variant s hranou patří do politiky a zón:

- `until-before-dtstart`: jádro vrátí chybu validace;
- `dtstart-not-in-rule`: DTSTART chybí;
- `exdate-date-on-timed`: EXDATE typu DATE se ignoruje;
- `rdate-only`: jádro odmítne vstup bez RRULE;
- `vtimezone-windows-name`, `vtimezone-custom-rules`: TZID není v IANA databázi.

Podrobnosti po případech jsou v `spikes/recurrence/results/report.md`.

### Selhání podle tříd

| třída | případ | kdo selhává | co se stane |
|---|---|---|---|
| DTSTART mimo okno pravidla | `until-before-dtstart` | všichni Rust kandidáti | chyba místo `[DTSTART]` (RFC 3.8.5.3: DTSTART je vždy první výskyt) |
| | `rdate-only` | rrule, rrule2, rrule-jiff | DTSTART chybí; s hranou chyba „Missing date generation property“ |
| DTSTART neodpovídá pravidlu | `dtstart-not-in-rule` | všichni | DTSTART chybí; dateutil (obal `rdate(dtstart)`) ho přidá navíc nad COUNT |
| EXDATE typu DATE na časované sérii | `exdate-date-on-timed` | všichni Rust kandidáti | **tiše ignorováno** (Google takto exportuje celodenní výjimky) |
| EXRULE | `exrule` | calcard, calcard-next | **tiše ignorováno** |
| vlastní VTIMEZONE (ne-IANA TZID) | `vtimezone-custom-rules` | calcard, calcard-next | **tiše plovoucí čas** |
| | | rrule rodina, obě hrany | chyba (VTIMEZONE vůbec nečte) |
| Windows název zóny | `vtimezone-windows-name` | rrule rodina, obě hrany | chyba; calcard-next projde |
| explicitní DTSTART v gapu | `dst-prague-explicit-gap-dtstart` | rrule | chyba parseru |
| | | rrule2, rrule-jiff | další dny posunuté o hodinu (viz níže) |
| | | calcard | `InvalidDtStart` |
| DST gap a fold generovaných výskytů | 7 případů | calcard | výskyty se tiše změní na plovoucí čas (`.single().unwrap_or(naive)`) |
| únik zóny hostitele | `floating-daily-host-gap` | rrule, rrule2 | plovoucí čas se počítá v `Local`, takže 29. 3. vyjde 03:30 místo 02:30 |
| RDATE PERIOD | `rdate-period` | rrule rodina bez hrany | chyba |
| RDATE fold | `rdate-gap-and-fold` | rrule, calcard | chyba / sloučení dvou různých okamžiků |
| ostatní | `tz-utc-z`, `until-utc-inclusive`, `override-of-rdate` | calcard | UTC jako plovoucí čas, poslední výskyt UNTIL vypadne, oba výskyty u přepisu RDATE zůstanou |

Chyba v portu na jiff a v `rrule2` (`dst-prague-explicit-gap-dtstart`):

- `finalize_parsed_rrule` (`core/rrule.rs`) doplní BYHOUR, BYMINUTE a BYSECOND z DTSTART už převedeného na zónu (03:30), ne z civilního času 02:30;
- všechny další výskyty pak padnou na 03:30, např. 2026-03-30T01:30Z místo 00:30Z;
- projeví se jen u DTSTART se zónou, takže hrana, která jádru dává civilní čas, chybu obchází;
- `rrule` 0.14 takový DTSTART vůbec neparsuje.

BY-pravidla (BYSETPOS, BYWEEKNO, BYYEARDAY, záporný BYMONTHDAY), WKST, přestupné dny, COUNT, celodenní a nekonečné řady s oknem zvládají všichni Rust kandidáti bez chyby. Aritmetika RRULE je v těchto crates zralá, problémy jsou na okrajích.

### Rozhodnutí podle RFC (výběr)

- **Neexistující lokální čas (gap)** se dá podle RFC 5545 vyložit dvěma způsoby a oba přijímáme:
  - 3.3.10: výskyt „MUST be ignored and MUST NOT be counted“, takže se vynechá a COUNT se doplní;
  - 3.8.5.3 → 3.3.5: posun offsetem před gapem (02:30 → 03:30 letního času).

  ical.js používá offset *po* gapu, což neodpovídá ani jednomu. U hodinových řad a u Apie se varianta s posunem srazí s dalším skutečným výskytem a deduplikuje se.
- **Fold** (dvojznačný čas): první výskyt (3.3.5). ical.js bere druhý.
- **YEARLY s BYMONTHDAY bez BYMONTH** se podle tabulky v 3.3.10 rozbalí do každého měsíce. Google tím myslí „ročně“, ale to patří do Google adaptéru (Musubi to dnes řeší v `sanitizeRecurrence`), ne do enginu.
- **DTSTART neodpovídající pravidlu** (politika Musubi): RFC 3.8.5.3 nazývá takovou množinu nedefinovanou. Opíráme se proto o 3.3.10, který u COUNT říká „The DTSTART property value always counts as the first occurrence“. Očekáváme `[10. 3., 16. 3., 23. 3.]`, tedy DTSTART a dva výskyty pravidla. Neprojde nikdo:
  - Rust kandidáti a ical.js DTSTART vynechají;
  - dateutil ho přidá navíc a vrátí čtyři výskyty.
  - Stejná politika platí i pro `friday-13th` (COUNT=5, DTSTART mimo pravidlo a v EXDATE). DTSTART zabere jeden z pěti výskytů a EXDATE ho pak odstraní, takže zbudou čtyři. V K2 jsme proto očekávání změnili z pěti výskytů (shoda orákul) na čtyři (`decided_by: policy`).

  Pravidlo 30. února vrátí `[DTSTART]` nebo `[]`, hlavně ale musí skončit.
- **Politika** (nevalidní vstupy): UNTIL bez Z u TZID, UNTIL typu DATE u DATE-TIME, EXDATE typu DATE na časované sérii, EXRULE a neznámé TZID. Přijímáme hlasitou chybu nebo rozumné čtení, **nikdy tiché ignorování**.

### Velikost, provázanost a původ jader

| | (a) calcard-next | (b) rrule 0.14 + naše hrana | (c) port na jiff (+ naše hrana) |
|---|---|---|---|
| chyby jádra v korpusu | 0 | 0 (bez hrany 3) | 0 s hranou, 1 bez ní |
| řádky kódu jádra | 2 886 (`datecalc` + glue `icalendar/dates.rs`), 16 souborů | 4 926 + parser 801, 25 souborů; bez obalu chrono-tz (`core/timezone.rs` + `timezone_impl.rs`, ~1 940 ř.) ~3 000 | 3 003 + parser 756, 23 souborů |
| externí crates | jiff, serde, ahash | chrono, chrono-tz, log, serde_with, thiserror; s hranou odpadá chrono-tz | jiff, log, serde_with, thiserror |
| provázanost mimo jádro | 14 cest do modelu calcard (`ICalendar`, `ICalendarRecurrenceRule`, `ICalendarValue`, `Tz`, `TzResolver`…) | jen vlastní parser RRULE | jen vlastní parser RRULE |
| původ | fork jednoho autora; crate vytvořen 2026-11-09, 5 verzí za 2 dny (3.0.1 z 2026-11-10), 58 stažení, 1 hvězda | vydaná verze 0.14.0 z 2025-04-20, 43 verzí, 1,74 mil. stažení; v repu je AFL fuzz harness (`rrule-afl-fuzz`); poslední commit 2025-04-20 | jeden nerevidovaný a nevydaný commit (`ccope/rust-rrule@50db3e2`); PR #151 zavřen 2026-09-26 nesloučený, podle autora byl do upstreamu poslán omylem |
| vlastní VTIMEZONE | enum `Tz` = `Floating \| Fixed \| Tz(IANA)` bez vlastní varianty; v korpusu tiše plovoucí čas | jádro zóny nevidí. Zóna je věc naší hrany a reprezentaci volíme sami | jádro bere `jiff::tz::TimeZone`, takže vlastní zóna musí jít vyjádřit v jiff (POSIX, TZif) |
| licence | MIT OR Apache-2.0, crate obsahuje `LICENSES/` a SPDX hlavičky | MIT OR Apache-2.0; **tarball z crates.io neobsahuje licenční soubory**, `LICENSE-MIT`/`LICENSE-APACHE` a copyright („2021 Fredrik Meringdal, Ralph Bisschops“) je nutné vzít z repa | MIT OR Apache-2.0, LICENSE soubory v git checkoutu |

Ostatní crates (`rrule2` 0.15: vytvořen 2026-09-27, 180 stažení, 0 hvězd, tarball bez licenčních souborů; `calcard` 0.3.14: 90 798 stažení) do užšího výběru nepostupují. jiff je Unlicense OR MIT. Copyleft se nikde nevyskytuje.

K námitce z PR #151 (WhyNotHugo: „Jiff _cannot_ represent custom VTIMEZONE entries … TZif can't represent all the time zones which can be represented in a VTIMEZONE“):

- autor portu odpověděl, že vlastní zónu lze převést VTIMEZONE → TZif (`tzif-codec`) → `jiff::tz::TimeZone::tzif` nebo seznam přechodů;
- my jsme ověřili jen převod přes POSIX řetězec, cesta přes TZif je **(neověřeno)**;
- námitka míří na jádro, které zónu dostává v API, tedy na (c) a částečně (a). V (b) jádro počítá jen civilní čas. Kdyby jiff některou VTIMEZONE vyjádřit neuměl, napíšeme vlastní hledání offsetu z pozorování VTIMEZONE jen do hrany a jádra se to nedotkne.

### Výkon (10 000 sérií, okno 2026–2031, 3 běhy)

| kandidát | čas na běh | z toho formátování | engine (odhad) | vyrobeno výskytů | peak RSS |
|---|---|---|---|---|---|
| parse-only (základ) | 4 ms | 1 ms | 3 ms | 0 | 13,2 MB |
| rrule 0.14 | 2,17–2,19 s | 1,11 s | 1,06–1,08 s | 6 053 034 | 13,2 MB |
| rrule2 | 2,18–2,19 s | 1,12–1,13 s | 1,06–1,07 s | 6 053 034 | 13,2 MB |
| rrule-jiff | 2,18–2,19 s | 1,12–1,13 s | 1,06–1,07 s | 6 053 034 | 13,2 MB |
| rrule 0.14 + hrana | 2,47–3,08 s | 1,10–1,16 s | 1,35–1,97 s | 6 053 034 | 13,2 MB |
| port na jiff + hrana | 2,29–2,48 s | 1,12–1,13 s | 1,16–1,36 s | 6 053 034 | 13,2 MB |
| calcard-next | 2,33–2,35 s | 0,71 s | 1,63–1,64 s | 3 729 034 | 13,2 MB |
| calcard 0.3.14 (bez okna, limit 5000) | 27,0–28,5 s | 1,12 s | 25,9–27,4 s | 6 054 701 | 13,2 MB |

- V okně je u všech 3 729 034 výskytů a nikdo nevrátil chybu.
- Rodina `rrule` a `calcard` nemají okenní API. Rozbalují od DTSTART (2020–2026) až do konce okna, takže vyrobí 6,05 mil. výskytů a harness pak všechno formátuje na řetězce a ořezává podle okna. Okno tedy u nich neznamená „5 let práce“.
- Formátování zabere u rodiny `rrule` zhruba polovinu času. „Engine“ je celkový čas minus změřené formátování a stále zahrnuje parsování ICS, řazení a deduplikaci a filtr okna nad řetězci.
- Rozptyl `rrule + hrana` (2,47–3,08 s) je šum mezi běhy, nikoli rozdíl v algoritmu. Hrana přidává převod přes jiff a RDATE/EXDATE nad okamžiky.
- RSS je nízké proto, že benchmark výsledky nedrží. Materializace 3,7 mil. okamžiků jako `i64` by stála ~30 MB **(neověřeno, odhad)**.
- `calcard` bez okenního API je pro velké okno nepoužitelný.
- U (b) a (c) stojí za doplnění přeskok na začátek okna pro dlouhé série. Dnes je to ~1 s na 10 000 sérií, problém to tedy není.

### Zóny: jiff vs chrono-tz

Verze tzdb:

- `chrono-tz` 0.10.4 má zakompilovanou IANA **2025b**.
- `jiff` 0.2.42 čte systémovou tzdb (`/usr/share/zoneinfo`):
  - `tzdata.zi` uvádí `# version 2026f-dirty`, tedy distribuční build (pacman `tzdata 2026f-1.1`);
  - co přesně „dirty“ znamená, jsme nezkoumali **(neověřeno)**.

V běhu korpusu pracovaly:

- s 2026f kandidáti nad jiff (`calcard-next`, `rrule-jiff`, obě hrany) a obě orákula;
- s 2025b `rrule`, `rrule2` a `calcard`.

Z 22 lišících se zón je v korpusu jen Africa/Casablanca (`tz-casablanca-ramadan`). Tento případ končí 2026-03-25, tedy před změnou 2026-09-20, takže **žádný případ korpusu nespadá do období, kde se databáze liší**.

Porovnali jsme 597 zón. V jiff nechybí žádná, ale **22 se liší**. Podle NEWS jde ve všech případech o změny dat v tzdb 2025c–2026f, ne o volby buildu (backzone):

- od 2026-09-20: Africa/Casablanca, El_Aaiun (2026c, Maroko trvale +00);
- od 2026-11-01: America/Vancouver, Edmonton, Winnipeg, Inuvik, Yellowknife, Rainy_River, Canada/* (2026b–2026e: Britská Kolumbie trvale −07, Severozápadní teritoria trvale −06, úprava Canada/Central), America/Havana, Cuba (2026f: Kuba 1. 11. 2026 neposouvá čas zpět);
- od 2022-03-27: Europe/Chisinau, Tiraspol (2026a: časy přechodu Moldavska podle EU);
- historicky:
  - Tijuana, Ensenada, Santa_Isabel, Mexico/BajaNorte: 1953 a 1961–1975 (2025c, Baja California);
  - Bogota: 2. 5. 1992 a Tehran/Iran: 26. 5. 1979 (2026d, přechod v 00:00 místo 24:00).

Na zastarání ukazuje 13 změn platných od roku 2026. Historické rozdíly jsou opravy dat a na výskyty v budoucnu vliv nemají.

Závěr: zakompilovaná tzdb zastarává. Kalendář s výskyty na roky dopředu potřebuje tzdb aktualizovatelnou bez nového vydání. `jiff` umí systémovou databázi, přibalenou (`tzdb-bundle-always`) i načtení z adresáře za běhu.

Ověřili jsme, že jiff `TimeZone::posix("<+0130>-1:30<+0230>,M3.2.0/2,M11.1.0/3")` dá pro případ vlastní VTIMEZONE správné okamžiky (`2026-03-06T07:30Z, 2026-03-13T06:30Z, 2026-03-20T06:30Z`).

### Strategie pro vlastní VTIMEZONE (ne-IANA TZID)

Vše se odehrává v hraně, jádro se nemění. Postup řešení TZID (první úspěch vyhrává):

1. TZID je platná IANA zóna (případně alias).
2. `X-LIC-LOCATION` uvnitř VTIMEZONE.
3. Mapa Windows → IANA podle CLDR `windowsZones.xml`. To je případ Outlooku a Exchange.
4. Shoda koncovky cesty (`/mozilla.org/20050126_1/Europe/Prague` → `Europe/Prague`), jen pokud koncovka je IANA zóna.
5. Převod samotné VTIMEZONE:
   - jedna aktuální dvojice STANDARD/DAYLIGHT s RRULE → POSIX TZ řetězec → `jiff::tz::TimeZone::posix` (ověřeno);
   - složitější historie → TZif (např. přes `tzif-codec`) **(neověřeno)**, nebo vlastní tabulka přechodů v hraně.
6. Jinak **hlasitá chyba** u série. Nikdy se nevrací tichý UTC ani plovoucí čas.

Původní VTIMEZONE se vždy uchová beze změny pro zpětný zápis.

## Verdikt a doporučení

**Vlastní modul `musubi-recurrence` za interním rozhraním. Jádrem bude převzatý (vendored) iterátor z vydaného `rrule` 0.14 za naší hranou nad jiff.**

1. **Rozhraní:** `expand(series, window, limit) -> Result<Vec<Instance>, RecurrenceError>`. Vstupem je náš kanonický model (DTSTART, RRULE, RDATE, EXDATE, přepisy, vyřešená zóna), ne surový ICS. Okno je povinné.
2. **Proč (b):**
   - **Korektnost:** v korpusu má 0 chyb jádra, stejně jako (a) a (c) za hranou.
   - **Původ:** jako jediné ze tří je vydané a široce používané, má fuzz harness a ostatní dvě jádra z něj vycházejí.
   - **Provázanost:** závisí jen na vlastním parseru RRULE. Po odstranění obalu chrono-tz zbude ~3 000 řádků, tedy stejně jako u (c).
   - **Zóny:** jádro je nevidí. Tím odpadá námitka z PR #151 a vlastní VTIMEZONE je čistě věc hrany.
   - Cena: chrono zůstane závislostí pro civilní aritmetiku v UTC (MIT/Apache).
3. **Kritérium pro jinou volbu:**
   - Bude-li tvrdým požadavkem jedna knihovna pro datum a čas (bez chrono), vezmeme **(c) za stejnou hranou**. Naměřené výsledky má shodné s (b) a velikost a provázanost také. Navíc je potřeba vlastní revize commitu `50db3e2` proti 0.14 a oprava `finalize_parsed_rrule`.
   - **(a) calcard-next** jako jádro nedoporučujeme. Je provázané se 14 typy modelu calcard a jde o dvoudenní fork jednoho autora. Zůstane jako **referenční engine v diferenciálním testu**.
4. **Povinná práce na (b)** (známé chyby a mezery, všechny naměřené v korpusu):
   1. Invariant „jádro dostává jen civilní čas v UTC“. Odstranit `Tz::Local` (únik zóny hostitele, `floating-daily-host-gap`), `chrono-tz` a `core/timezone.rs` + `timezone_impl.rs`. Invariant zároveň obchází chybu BYHOUR/BYMINUTE z DTSTART se zónou a chybu parseru u DTSTART v gapu. Hlídá ho regresní test `dst-prague-explicit-gap-dtstart`.
   2. `until-before-dtstart`: jádro vrací chybu validace, obal musí vrátit `[DTSTART]`.
   3. `rdate-only`: jádro odmítne vstup bez RRULE („Missing date generation property“). Obal ho bez RRULE nevolá.
   4. `dtstart-not-in-rule`: obal přidá DTSTART jen tehdy, když ho jádro samo nevygenerovalo, a pak množinu ořízne na COUNT výskytů celkem (politika podle RFC 3.3.10). Volat jádro s COUNT−1 nestačí: u synchronizovaného DTSTART by se ztratil poslední výskyt.
   5. `exdate-date-on-timed`: zvolit politiku (vyloučit celý den, nebo chyba). Dnes se EXDATE tiše ignoruje.
   6. EXRULE: jádro ji umí, ale je v RFC 5545 zastaralá. Rozhodnout, zda ji přijímat, nebo odmítat.
   7. Převod UNTIL: hrana převádí UNTIL v UTC na civilní čas jen přibližně a výsledky filtruje podle okamžiku. Ponechat filtr podle okamžiku a doplnit případ UNTIL v hodině foldu, který korpus zatím nemá. *Doplněno v K2:* hrana převádí UNTIL přesně (UTC, místní čas i DATE) a hodinu foldu hlídá test `until_in_fold_hour` v `musubi-recurrence`.
   8. Řešení TZID a vlastní VTIMEZONE v hraně podle strategie výše (`vtimezone-windows-name`, `vtimezone-custom-rules`).
   9. Přepisy RECURRENCE-ID a THISANDFUTURE přesunout z harnessu do modulu.
   10. Licence: do vendored adresáře zkopírovat `LICENSE-MIT`, `LICENSE-APACHE` a copyright z repa `fmeringdal/rust-rrule` (v tarballu chybí) a zachovat hlavičky souborů.
   11. Převzít `rrule-afl-fuzz` a pouštět ho v CI. *Ověřeno v K2 (11. 11. 2026):* upstream harness (commit `1c3420e`) s `cargo-afl` 0.18.2 funguje, 90 s, 1,2 mil. spuštění, 0 pádů. Lokálně s LLVM 23 bylo nutné `cargo afl config --update`, protože přibalené AFL++ tak nové LLVM nezná. Převzatý harness (`crates/musubi-recurrence/fuzz`) volá veřejné `expand` a běží v nočním CI.
   12. Volitelně: přeskok na začátek okna pro dlouhé série (viz Výkon). **Odloženo v K2:** `TooExpensive` znamená „v tomto okně nerozbalitelné" a K5 ho musí zviditelnit (plán fáze 1, K5 a kapitola 9).
5. **Politika v našem obalu**, která platí pro jakékoli jádro:
   - plovoucí čas se nikdy nepočítá v zóně hostitele;
   - gap a fold podle RFC 3.3.5: fold → první výskyt, gap → offset před gapem; tak to dnes dělá hrana;
   - RDATE PERIOD se rozbalí na začátek;
   - tvrdý strop výskytů (např. 50 000) a povinné okno.
6. **Zóny:** jiff + aktualizovatelná tzdb (systémová na serveru, přibalená jako fallback) a výše popsaná strategie pro VTIMEZONE. `chrono-tz` nepoužívat.
7. **Regrese:** `spikes/recurrence` přesunout do `musubi-recurrence/tests/differential` a pouštět v CI proti dateutil a calcard-next. Při každé aktualizaci tzdb pouštět `tzcheck`.

Vedlejší zjištění k bodu „Ověřit“ v návrhu architektury: **upstream `calcard` 0.3.14 rozbalovat umí** (`expand_dates`), ale bez okna a s chybami v DST (výskyty v gap/fold se změní na plovoucí čas). Pro produkční rozbalování se nehodí, pro parsování ICS ano.

## Otevřená rizika

- **Kruhovost očekávání:** 59 případů má jako pravdu shodu dvou orákul. Pokud se obě mýlí stejně, korpus chybu nezachytí. Ruční kontrola proběhla jen u případů rozhodnutých podle RFC.
- **Vlastnictví jádra:** upstream `rrule` se od dubna 2025 nemění, převzatý kód budeme udržovat sami. Odhad práce na obal, politiku a převzetí **(neověřeno)**.
- **Hrana je prototyp** (~120 řádků v harnessu). TZID v ní hledáme jen v IANA databázi a UNTIL převádíme zjednodušeně.
- **Volba čtení gapu:** RFC si zde odporuje. Pro shodu s ostatními klienty (Google, Outlook, iCloud) je potřeba ověřit, co skutečně dělají **(neověřeno, bez přístupu k reálným účtům)**.
- **VTIMEZONE → TZif** pro zóny s historií pravidel je neověřený. Ověřený je jen převod na POSIX.
- **tzdb na klientech:** mobilní nebo WASM klient s jinou verzí tzdb může dávat jiné okamžiky než server. Je potřeba určit, kdo rozbaluje autoritativně.
- **Výkon:** engine jsme od formátování oddělili jen odečtem a bez materializace výsledků. Paměť pro držení výskytů v cache jsme neměřili.
- **Pokrytí korpusu:**
  - BYHOUR/BYMINUTE kombinace pod den nad rámec dvou případů nejsou pokryté;
  - chybí reálný export z Outlooku;
  - žádný případ nespadá do období, kde se liší tzdb 2025b a 2026f.
