# Phase 0: engine pro opakování (RRULE/RDATE/EXDATE)

## Otázka

Má musubi-sync rozbalovat opakování převzatou knihovnou `rrule`, některým forkem (`rrule2`, port na `jiff` z PR #151, rozbalení v `calcard-next`), nebo vlastním modulem? Rozhoduje korektnost. Výkon a paměť jsou až druhotné.

## Shrnutí

- **Verdikt: vlastní modul `musubi-recurrence` za interním rozhraním. Jádro rozbalování převezmeme jako připnutý (vendored) kód. Žádnou knihovnu nepoužijeme přímo jako závislost.**
- Žádný kandidát neprošel celým korpusem. Nejlépe dopadl `calcard-next` (85/89), pak port na jiff (82/89), `rrule2` (81/89) a `rrule` (80/89). Kandidáti selhávají na různých místech. Co chybí, je převážně politika (EXRULE, EXDATE typu DATE, neznámé TZID, DTSTART mimo pravidlo), ne samotná aritmetika.
- Pro vlastní politiku musíme mít obal tak jako tak. Nejhorší selhání jsou tichá: vlastní VTIMEZONE se u všech calcard kandidátů tiše změní na plovoucí čas, EXRULE a EXDATE typu DATE se tiše ignorují a `rrule`/`rrule2` přenáší zónu hostitele do plovoucích časů.
- Výkon problém není. 10 000 sérií v okně 5 let (3,73 mil. výskytů) trvá u všech kandidátů s oknem ~2,2 s včetně formátování na řetězce a peak RSS je ~13,4 MB.
- Zóny: **jiff** se systémovou nebo přibalenou tzdb, ne `chrono-tz`. `chrono-tz` 0.10.4 má zakompilovanou tzdb 2025b a už teď se v 22 zónách liší od tzdb 2026f, u 13 z nich přitom v datech od září a listopadu 2026.

## Metoda

1. **Korpus** (`spikes/recurrence/corpus/build_corpus.py`) má 89 případů s tagy:
   - DST (gap a fold) v Praze, New Yorku a Lord Howe (posun o 30 minut), historické zóny (Apia, která vynechala 30. 12. 2011, a další);
   - BYSETPOS, BYWEEKNO včetně týdne 53 a záporných hodnot, BYYEARDAY, záporný BYMONTHDAY, přestupné dny, WKST;
   - UNTIL v UTC, lokálně (nevalidní) a jako DATE; COUNT;
   - celodenní a plovoucí časy;
   - EXDATE s TZID, celodenní EXDATE, EXDATE typu DATE na časované sérii, RDATE typu PERIOD, samotné RDATE, EXRULE;
   - YEARLY bez BYMONTH; přepisy RECURRENCE-ID včetně THISANDFUTURE a přepisu RDATE výskytu;
   - nekonečné řady s oknem a nemožné pravidlo (30. února);
   - VTIMEZONE s vlastními pravidly, Windows názvy zón a starý Mozilla prefix TZID.

   17 případů pochází přímo z Musubi (read-only):
   - testy;
   - `apps/api/src/sync/adapters/google.ts` (`sanitizeRecurrence`);
   - `packages/calendar/src/time-expansion.test.ts`;
   - `docs/sync/caldav-date-export.md`, `docs/sync/caldav-rdate-writes.md`.
2. **Orákula:**
   - python-dateutil 2.9.0.post0 (+ icalendar 6.3.2 jako parser, tzdata 2026.6);
   - ical.js 2.2.1 s VTIMEZONE vygenerovanými z tzdb.

   Kde se orákula shodla, převzali jsme výsledek (59 případů). Ostatní jsme rozhodli ručně podle textu RFC 5545 (25 případů) nebo podle politiky Musubi (5 případů), každý s odůvodněním v `corpus/expected.json` (pole `why`). U nejednoznačných míst RFC přijímáme obě čtení.
3. **Kandidáti** (všichni MIT OR Apache-2.0; jiff Unlicense OR MIT, bez copyleftu), jeden Rust binár `candidates/`:
   - `rrule` 0.14.0 a `rrule2` 0.15.0 (obojí chrono + chrono-tz);
   - port na jiff (`ccope/rust-rrule` @ `50db3e2`, uzavřený nesloučený PR #151);
   - `calcard` 0.3.14 (`expand_dates`);
   - `calcard-next` 3.0.1 (`expand_dates_between`).

   Přepisy RECURRENCE-ID u rodiny `rrule` aplikuje harness, protože tyto knihovny je neumí. calcard a calcard-next je zpracují samy.
4. **Výkon:** 10 000 syntetických sérií vznikne kombinací 8 typů pravidel × 4 zón (Praha, New York, Lord Howe, Tokio). DTSTART spadají do let 2020–2026, každá pátá série má EXDATE a okno je 2026-01-01 + 5 let. Každý kandidát běží 3×. Peak RSS se měří přes `wait4` a základem je běh `parse-only`.
5. **Zóny:** `tzcheck` porovná všech 597 zón z `chrono-tz` s `jiff` v krocích po 12 h v letech 1970–2038.

Celé měření běží s `TZ=Europe/Prague`, aby se ukázal únik zóny hostitele.

## Reprodukce

```sh
cd spikes/recurrence
./run.sh            # korpus, orákula, kandidáti, tzcheck -> results/report.md
./run.sh --bench    # navíc výkon -> results/bench.json
```

Potřeba je python3, node/npm a cargo. Verze jsou připnuté (`requirements.txt`, `package-lock.json`, `Cargo.lock`). Harness slouží jako regresní sada: nový případ se přidá do `build_corpus.py` a jeho očekávání do `expected.json`.

## Výsledky

### Korpus (89 případů)

| engine | pass | fail | error | poznámka |
|---|---|---|---|---|
| **calcard-next 3.0.1** | **85** | 3 | 1 | okno, přepisy a jiff jsou vestavěné |
| rrule (port na jiff) | 82 | 3 | 4 | uzavřený PR, bez vydání |
| rrule2 0.15.0 | 81 | 4 | 4 | |
| rrule 0.14.0 | 80 | 3 | 6 | |
| calcard 0.3.14 | 72 | 15 | 2 | rozbalovat umí, ale bez okna a s chybami v DST |
| ical.js 2.2.1 (orákulum) | 66 | 22 | 1 | BYWEEKNO, BYSETPOS, YEARLY+BYDAY a přestupné dny špatně |
| dateutil (orákulum) | 89 | 0 | 0 | skóre je zčásti kruhové: očekávání byla nasazena z orákul |

Podrobnosti po případech jsou v `spikes/recurrence/results/report.md`.

### Selhání podle tříd

| třída | případ | kdo selhává | co se stane |
|---|---|---|---|
| DTSTART mimo okno pravidla | `until-before-dtstart` | všichni Rust kandidáti | chyba místo `[DTSTART]` (RFC 3.8.5.3: DTSTART je vždy první výskyt) |
| | `rdate-only` | rrule, rrule2, rrule-jiff | DTSTART chybí |
| EXDATE typu DATE na časované sérii | `exdate-date-on-timed` | všichni Rust kandidáti | **tiše ignorováno** (Google takto exportuje celodenní výjimky) |
| EXRULE | `exrule` | calcard, calcard-next | **tiše ignorováno** |
| vlastní VTIMEZONE (ne-IANA TZID) | `vtimezone-custom-rules` | calcard, calcard-next | **tiše plovoucí čas** |
| | | rrule rodina | chyba (VTIMEZONE vůbec nečte) |
| Windows název zóny | `vtimezone-windows-name` | rrule rodina | chyba; calcard-next projde |
| explicitní DTSTART v gapu | `dst-prague-explicit-gap-dtstart` | rrule | chyba parseru |
| | | rrule2, rrule-jiff | další dny posunuté na 03:30 |
| | | calcard | `InvalidDtStart` |
| DST gap a fold generovaných výskytů | 7 případů | calcard | výskyty se tiše změní na plovoucí čas (`.single().unwrap_or(naive)`) |
| únik zóny hostitele | `floating-daily-host-gap` | rrule, rrule2 | plovoucí čas se počítá v `Local`, takže 29. 3. vyjde 03:30 místo 02:30 |
| RDATE PERIOD | `rdate-period` | rrule rodina | chyba |
| RDATE fold | `rdate-gap-and-fold` | rrule, calcard | chyba / sloučení dvou různých okamžiků |
| ostatní | `tz-utc-z`, `until-utc-inclusive`, `override-of-rdate` | calcard | UTC jako plovoucí čas, poslední výskyt UNTIL vypadne, oba výskyty u přepisu RDATE zůstanou |

BY-pravidla (BYSETPOS, BYWEEKNO, BYYEARDAY, záporný BYMONTHDAY), WKST, přestupné dny, COUNT, celodenní a nekonečné řady s oknem zvládají všichni Rust kandidáti bez chyby. Aritmetika RRULE je v těchto crates zralá, problémy jsou na okrajích.

### Rozhodnutí podle RFC (výběr)

- **Neexistující lokální čas (gap)** se dá podle RFC 5545 vyložit dvěma způsoby a oba přijímáme:
  - 3.3.10: výskyt „MUST be ignored and MUST NOT be counted“, takže se vynechá a COUNT se doplní;
  - 3.8.5.3 → 3.3.5: posun offsetem před gapem (02:30 → 03:30 letního času).

  ical.js používá offset *po* gapu, což neodpovídá ani jednomu. U hodinových řad a u Apie se varianta s posunem srazí s dalším skutečným výskytem a deduplikuje se.
- **Fold** (dvojznačný čas): první výskyt (3.3.5). ical.js bere druhý.
- **YEARLY s BYMONTHDAY bez BYMONTH** se podle tabulky v 3.3.10 rozbalí do každého měsíce. Google tím myslí „ročně“, ale to patří do Google adaptéru (Musubi to dnes řeší v `sanitizeRecurrence`), ne do enginu.
- **DTSTART nesynchronizovaný s pravidlem** dává podle RFC nedefinovanou množinu, proto přijímáme výsledek s DTSTART i bez něj. Pravidlo 30. února vrátí `[DTSTART]` nebo `[]`, hlavně ale musí skončit.
- **Politika** (nevalidní vstupy): UNTIL bez Z u TZID, UNTIL typu DATE u DATE-TIME, EXDATE typu DATE na časované sérii, EXRULE a neznámé TZID. Přijímáme hlasitou chybu nebo rozumné čtení, **nikdy tiché ignorování**.

### Výkon (10 000 sérií, okno 5 let, 3 běhy)

| kandidát | čas na běh | peak RSS | výskytů v okně |
|---|---|---|---|
| parse-only (základ) | 3 ms | 13,2–13,4 MB | 0 |
| rrule 0.14 | 2,16–2,17 s | 13,4 MB | 3 729 034 |
| rrule2 | 2,18–2,22 s | 13,4 MB | 3 729 034 |
| rrule-jiff | 2,17–2,20 s | 13,4 MB | 3 729 034 |
| calcard-next | 2,29–2,31 s | 13,4 MB | 3 729 034 |
| calcard 0.3.14 (bez okna, rozbaluje od DTSTART, limit 5000) | 20,7–42,4 s | 13,4 MB | 3 729 034 |

- Všichni kandidáti vrátili stejný počet výskytů s nulou chyb.
- Časy zahrnují formátování každého výskytu na řetězec, jsou tedy horní mezí. Samotný engine jsme neměřili zvlášť.
- RSS je nízké proto, že benchmark výsledky nedrží. Materializace 3,7 mil. okamžiků jako `i64` by stála ~30 MB **(neověřeno, odhad)**.
- `calcard` bez okenního API je pro velké okno nepoužitelný, protože vždy iteruje od DTSTART.

### Zóny: jiff vs chrono-tz

- `chrono-tz` 0.10.4 má zakompilovanou IANA **2025b**. `jiff` 0.2.42 čte systémovou tzdb (`/usr/share/zoneinfo`, **2026f**).
- Porovnali jsme 597 zón. V jiff nechybí žádná, ale **22 se liší**:
  - od 2026-09-20: Africa/Casablanca, El_Aaiun;
  - od 2026-11-01: America/Vancouver, Edmonton, Winnipeg, Havana, Inuvik, Yellowknife, Rainy_River, Canada/*, Cuba;
  - od 2022-03-27: Europe/Chisinau, Tiraspol;
  - historicky: Bogota 1992, Tijuana/Ensenada/Santa_Isabel/Mexico/BajaNorte 1970, Tehran/Iran 1979.

  Politické příčiny jednotlivých změn **(neověřeno)**.
- Závěr: zakompilovaná tzdb zastarává. Kalendář s výskyty na roky dopředu potřebuje tzdb aktualizovatelnou bez nového vydání. `jiff` umí systémovou databázi, přibalenou (`tzdb-bundle-always`) i načtení z adresáře za běhu.
- Ověřili jsme, že jiff `TimeZone::posix("<+0130>-1:30<+0230>,M3.2.0/2,M11.1.0/3")` dá pro případ vlastní VTIMEZONE správné okamžiky (`2026-03-06T07:30Z, 2026-03-13T06:30Z, 2026-03-20T06:30Z`).

### Strategie pro vlastní VTIMEZONE (ne-IANA TZID)

Postup řešení TZID (první úspěch vyhrává):

1. TZID je platná IANA zóna (případně alias).
2. `X-LIC-LOCATION` uvnitř VTIMEZONE.
3. Mapa Windows → IANA podle CLDR `windowsZones.xml`. To je případ Outlooku a Exchange.
4. Shoda koncovky cesty (`/mozilla.org/20050126_1/Europe/Prague` → `Europe/Prague`), jen pokud koncovka je IANA zóna.
5. Převod samotné VTIMEZONE:
   - jedna aktuální dvojice STANDARD/DAYLIGHT s RRULE → POSIX TZ řetězec → `jiff::tz::TimeZone::posix`;
   - složitější historie → TZif (např. přes `tzif-codec`) **(neověřeno)**.
6. Jinak **hlasitá chyba** u série. Nikdy se nevrací tichý UTC ani plovoucí čas.

Původní VTIMEZONE se vždy uchová beze změny pro zpětný zápis.

## Verdikt a doporučení

**Vlastní modul `musubi-recurrence` za interním rozhraním. Jádro převezmeme a budeme ho vlastnit.**

1. **Rozhraní:** `expand(series, window, limit) -> Result<Vec<Instance>, RecurrenceError>`. Vstupem je náš kanonický model (DTSTART, RRULE, RDATE, EXDATE, přepisy, vyřešená zóna), ne surový ICS. Okno je povinné.
2. **Jádro:** převezmeme (vendor) rozbalovací kód z `calcard-next` (nejvyšší skóre; okno, přepisy i jiff už má) nebo iterátor z portu `rrule` na jiff. Obě varianty jsou Apache/MIT. Volba mezi nimi je malá: calcard-next je provázaný s parserem calcard, který stejně použijeme pro ICS. Port rrule je samostatný a čistší na vyjmutí. **Doporučení: port rrule na jiff jako výchozí jádro, calcard-next jako referenci v diferenciálním testu.** Ani jedno nedáváme jako přímou závislost:
   - PR #151 je uzavřený a nesloučený;
   - upstream `rrule` naposledy změnil kód v dubnu 2025;
   - `rrule2` má 0 hvězd;
   - `calcard-next` je fork jednoho autora.
3. **Politika v našem obalu** (to, co v korpusu selhalo):
   - DTSTART je vždy první výskyt;
   - EXDATE typu DATE na časované sérii vyloučí celý den (nebo vrátí chybu; rozhodnout v návrhu modelu);
   - EXRULE, RDATE PERIOD a podhodinové frekvence se odmítnou nebo převedou výslovně;
   - neznámé TZID vrátí hlasitou chybu;
   - plovoucí čas se nikdy nepočítá v zóně hostitele;
   - gap a fold podle RFC 3.3.5 a 3.3.10 (zvolit jedno čtení a zapsat ho);
   - tvrdý strop výskytů (např. 50 000) a povinné okno.
4. **Zóny:** jiff + aktualizovatelná tzdb (systémová na serveru, přibalená jako fallback) a výše popsaná strategie pro VTIMEZONE. `chrono-tz` nepoužívat.
5. **Regrese:** `spikes/recurrence` přesunout do `musubi-recurrence/tests/differential` a pouštět v CI proti dateutil a calcard-next. Při každé aktualizaci tzdb pouštět `tzcheck`.

Vedlejší zjištění k bodu „Ověřit“ v návrhu architektury: **upstream `calcard` 0.3.14 rozbalovat umí** (`expand_dates`), ale bez okna a s chybami v DST (výskyty v gap/fold se změní na plovoucí čas). Pro produkční rozbalování se nehodí, pro parsování ICS ano.

## Otevřená rizika

- **Kruhovost očekávání:** 59 případů má jako pravdu shodu dvou orákul. Pokud se obě mýlí stejně, korpus chybu nezachytí. Ruční kontrola proběhla jen u případů rozhodnutých podle RFC.
- **Vlastnictví jádra:** převzatý kód budeme udržovat sami. Odhad práce na obal, politiku a převzetí **(neověřeno)**.
- **Volba čtení gapu:** RFC si zde odporuje. Pro shodu s ostatními klienty (Google, Outlook, iCloud) je potřeba ověřit, co skutečně dělají **(neověřeno, bez přístupu k reálným účtům)**.
- **VTIMEZONE → TZif** pro zóny s historií pravidel je neověřený. Ověřený je jen převod na POSIX.
- **tzdb na klientech:** mobilní nebo WASM klient s jinou verzí tzdb může dávat jiné okamžiky než server. Je potřeba určit, kdo rozbaluje autoritativně.
- **Výkon** byl měřen bez materializace výsledků a se zahrnutým formátováním. Paměť pro držení výskytů v cache neměřena.
- Korpus nepokrývá BYHOUR/BYMINUTE kombinace pod den nad rámec dvou případů a nemá žádný reálný export z Outlooku.
