# Phase 0: calcard jako základ iCalendar/vCard vrstvy

Stav: hotovo · datum: 2026-11-11 · harness: [`spikes/calcard-fidelity/`](../../../spikes/calcard-fidelity/)

## 1. Otázka

Může crate [`calcard`](https://github.com/stalwartlabs/calcard) od Stalwart Labs sloužit v musubi-sync jako základ
pro iCalendar, vCard, JSCalendar a JSContact?

Pravidlo projektu je pevné:

- raw payload se ukládá **verbatim**;
- zápis zpět na server je **patch nad posledním raw stavem**, ne přegenerování objektu.

Proto nás nezajímá jen to, jestli calcard data přečte. Zajímá nás také:

- co se stane s daty, kterým nerozumí;
- jestli jde upravit jednu vlastnost, aniž by se změnilo cokoli jiného.

## 2. Metoda

Harness v Rustu (`calcard = "=0.3.14"`, release build, Rust 1.99) projde korpus a pro každý soubor změří následující.

### Parse

Měříme dvě věci:

- jestli `ICalendar::parse` / `VCard::parse` vrátí objekt;
- počet varování `Entry::InvalidLine`.

### Round-trip parse → `to_string()`

Výsledek se řadí do jedné ze čtyř úrovní:

- **L0**: výstup je bajtově identický;
- **L1**: stejné logické řádky, liší se jen folding a EOL;
- **L2**: stejná multimnožina řádků, jiné pořadí;
- **L3**: změněné řádky.

Dále se počítá, kolik se zachovalo verbatim z těchto položek:

- X-vlastnosti;
- neznámé parametry (X-param);
- vCard skupiny (`item1.`);
- bloky VTIMEZONE.

Kontroluje se také pořadí vlastností, idempotence (`rt(rt(x)) == rt(x)`) a rovnost AST (`parse(rt(x)) == parse(x)`).

### Patch

Mění se SUMMARY a DTSTART (iCal) a TEL (vCard). Srovnáváme tři cesty:

1. **AST cesta**: upravit entry v AST a serializovat celý objekt. Měří se, jestli zbytek souboru zůstal verbatim.
2. **raw splice**: prototyp `src/rawpatch.rs`, 202 řádků kódu bez testů a komentářů (v první verzi PR 160; přibyla vestavěná kontrola a odmítání kódovaných hodnot). Funguje takto:
   - zarovná logické řádky raw textu s entries calcardu;
   - nahradí bajty jediného řádku výstupem `entry.write_to()`;
   - když zarovnání nesedí, patch odmítne.
3. **value-only splice**: jako raw splice, ale ze serializace calcardu vezme jen hodnotu za `:`. Jméno a parametry zůstanou v původním zápisu. Řádky s parametrem `ENCODING`/`CHARSET` (nebo holým `QUOTED-PRINTABLE`/`BASE64` z vCard 2.1) odmítne, protože calcard serializuje dekódovanou hodnotu.

Obě splice cesty po každém zápisu samy ověří (`rawpatch::checked_splice`), že:

- bajty mimo nahrazený řádek se nezměnily;
- calcard výsledek znovu načte přesně na očekávaný AST, který se od původního liší **jen** v cílové vlastnosti.

Při nesouladu patch odmítnou. U AST cesty harness kontroluje totéž zvenku.

### Konverze

Testujeme cesty iCal → JSCalendar → iCal a vCard → JSContact → vCard, každou ve dvou režimech:

- `include_ical_components` / `include_vcard_parameters` = true (výchozí stav; nepodporované části se uloží do JSON jako raw);
- tytéž volby = false.

Měříme dvě veličiny:

- **„vlastnosti pryč“**: rozdíl multimnožin jmen vlastností;
- **„řádky ne-verbatim“**.

### Expanze a výkon

- Expanze: `ICalendar::expand_dates(Tz::UTC, 1000)` na syntetických a Musubi souborech, výsledky zkontrolované ručně.
- Výkon: generátor vytvoří 50 000 událostí nebo karet a měří se nejlepší ze tří běhů na Intel Core Ultra 7 265K.

### Korpus

Podrobný původ je v `spikes/calcard-fidelity/corpus/SOURCES.md`.

| Korpus | Soubory | Původ |
|---|---|---|
| `musubi` | 63 `.ics` | Vytaženo z fixtures a scénářů read-only repa Musubi (`tools/extract_musubi.py`), deduplikováno. |
| `synth` | 16 `.ics` + 9 `.vcf` | Ručně psané vzorky ve stylu Google, Outlook/Exchange, iCloud, Nextcloud/Sabre, Thunderbird a Outlook vCard 2.1, příklady z RFC 5545/6350/2426 a edge cases. **Nejde o exporty z reálných účtů.** |
| `calcard-ical` | 998 `.ics` | Testovací data calcardu, čtená z cargo registry (nevendorovaná). Pocházejí z libical, ical4j, Apple CCS, CalDAVTester, Cyrus, Evolution, sabre/vobject, Nextcloud, Radicale, Zimbra a dalších. |
| `calcard-vcard` | 244 `.vcf` | Totéž pro vCard (FolkerKinzel.VCards, Evolution, sabre, …). |

## 3. Reprodukce

```sh
cd spikes/calcard-fidelity
cargo build --release
cargo test --release                     # 3 testy raw-patch prototypu (vč. regrese single-line QP)
cargo fetch
R=$(ls -d ~/.cargo/registry/src/*/calcard-0.3.14/resources)
./target/release/calcard-fidelity corpus corpus/musubi corpus/synth $R/ical $R/vcard > results/summary.txt
./target/release/calcard-fidelity expand corpus/synth/*.ics corpus/musubi/*.ics > results/expand.txt
./target/release/calcard-fidelity bench 50000 > results/bench.txt
./target/release/calcard-fidelity rt <soubor>        # re-serializace calcardem
./target/release/calcard-fidelity js <soubor>        # X -> JS* JSON -> X
```

Výstupy jsou commitnuté v `results/`. Per-file data jsou v `results/<korpus>.tsv` a ztracené řádky v `results/<korpus>.lost.txt`.

**Determinismus:** parse, round-trip, patch a „vlastnosti pryč“ vycházejí při opakovaném běhu stejně. Metrika „řádky ne-verbatim“ u JS* konverzí kolísá mezi běhy o jednotky řádků (např. synth JSCalendar+raw 12 nebo 13/511, calcard-ical 4 891–4 900/138 348). Příčinou je calcard sám: při cestě JS → X zapisuje parametry X-vlastností z hash mapy v náhodném pořadí, např. `X-CUSTOM-CAL-PROP;X-FOO=bar;X-QUOTED=…`. Řazení našeho výstupu by to neodstranilo, protože jde o skutečnou nedeterministickou změnu řádku, proto to jen uvádíme. Čísla v reportu jsou z běhu commitnutého v `results/`.

## 4. Výsledky

### 4.1 Parse

| Korpus | OK | Selhání | InvalidLine varování |
|---|---|---|---|
| musubi | 63/63 | 0 | 0 |
| synth | 25/25 | 0 | 0 |
| calcard-ical | 987/998 (98,9 %) | 11: 2 fuzz soubory, 9 Lotus Notes s čistým CR jako EOL (i calcardův vlastní `.out` je prázdný) | 21 |
| calcard-vcard | 243/244 | 1 | – |

Selhání se týkají jen poškozených nebo exotických vstupů. Calcard je velmi tolerantní: Outlook 2.1 s quoted-printable (QP), Apple `itemN.` skupiny i Windows TZID projdou bez chyby.

### 4.2 Round-trip (parse → `to_string()`)

| Korpus | L0 bajtově | L1 logicky | L2 jen pořadí | L3 změněno | řádky ne-verbatim |
|---|---|---|---|---|---|
| musubi | 29/63 | 32 | 0 | 2 | 2/792 (0,3 %) |
| synth | 3/25 | 6 | 0 | 16 | 74/670 (11,0 %) |
| calcard-ical | 71/987 (7,2 %) | 384 (38,9 %) | 4 | 528 (53,5 %) | 8 527/138 371 (6,2 %) |
| calcard-vcard | 15/243 | 19 | 0 | 209 (86 %) | 1 588/9 413 (16,9 %) |

L1 u Musubi je převážně artefakt extrakce: 28 souborů nemá koncové CRLF a 1 soubor má jen LF. Dva soubory v L3 se liší jen pořadím částí RRULE uvnitř VTIMEZONE (`BYMONTH;BYDAY` → `BYDAY;BYMONTH`).

Zachování podle kategorií:

| Korpus | X-vlastnosti | X-param | skupiny | VTIMEZONE | pořadí vlastností | idempotentní | AST shodný |
|---|---|---|---|---|---|---|---|
| musubi | 29/29 | 4/4 | – | 2/3 | 62/63 | 63/63 | 63/63 |
| synth | 63/69 | 12/14 | 10/17 | 4/8 | 24/25 | 25/25 | 25/25 |
| calcard-ical | 7 630/7 808 (97,7 %) | 108/124 | – | 523/932 (56,1 %) | 951/987 | 987/987 | 987/987 |
| calcard-vcard | 444/506 | 11/16 | 235/346 | – | 178/243 (73,3 %) | 243/243 | 205/243 |

U `calcard-vcard` se AST neshoduje ve 38 případech. Většinou (35) jde o karty bez VERSION: writer ji doplní a reparse pak vidí jinou verzi.

Calcard **neztrácí informaci na úrovni AST**: zápis je idempotentní a znovu načtený AST je shodný. Při serializaci ale normalizuje, a některé změny jsou **sémantické**. Doložené případy:

**Vlastní hodnoty, které calcard escapuje:**

- `X-APPLE-STRUCTURED-LOCATION`: hodnota `geo:50.081157,14.413504` se změní na `geo:50.081157\,14.413504`. Parametry dostanou uvozovky a `VALUE=URI` se přesune.
- `X-ANDROID-CUSTOM`: středníky se escapují, takže se změní struktura hodnoty.
- `X-MULTI:a,b\,c,d` se změní na `a\,b\,c\,d`, čímž se ztratí hranice mezi hodnotami.

**vCard:**

- Řádek VERSION se vždy přesune hned za BEGIN. To je příčina 26,7 % souborů se změněným pořadím.
- Ve verzi 3.0 přibude k ne-ASCII řádkům `CHARSET=UTF-8`.
- `type=work;type=voice` se přepíše na `TYPE=WORK,VOICE`.
- Hodnoty KIND a TYPE se převedou na velká písmena.
- REV se převede z extended na basic formát.
- vCard 2.1 s QP se dekóduje a zapíše jako řádek ve stylu 3.0.

**Obecné normalizace:**

- Jména vlastností se převedou na velká písmena.
- Hodnoty CN dostanou uvozovky.
- `TZOFFSETFROM:+010000` se změní na `+0100`.
- Řádky se přefoldují na 75 oktetů.

Plná re-serializace přes calcard proto **porušuje pravidlo verbatim**. Navíc u X-vlastností Apple a Android může změnit, co si klient z dat přečte.

### 4.3 Patch (SUMMARY, DTSTART, TEL)

Sledujeme podíl patchů, po kterých se změnila **jen cílová vlastnost** a zbytek souboru zůstal bajtově beze změny.

| Korpus / vlastnost | AST cesta (celý objekt) | raw splice | value-only splice |
|---|---|---|---|
| musubi DTSTART | 42/43 | 43/43 | 43/43 |
| musubi SUMMARY | 48/49 | 49/49 | 49/49 |
| synth DTSTART | 6/13 | 13/13 | 13/13 |
| synth SUMMARY | 7/15 | 15/15 | 15/15 |
| synth TEL | 2/8 | 7/8 | 7/8 |
| calcard-ical DTSTART | 350/760 (46,1 %) | 747/760 (98,3 %) | 747/760 |
| calcard-ical SUMMARY | 365/778 (46,9 %) | 765/778 (98,3 %) | 762/778 (97,9 %) |
| calcard-vcard TEL | 13/144 (9 %) | 126/144 (87,5 %) | 126/144 |

Ve zbytku případů splice patch **odmítl**. Většinou zarovnání raw ↔ AST nesedělo: vCard 2.1 QP s měkkými zalomeními, rozbitý folding ve fixtures, karty bez VERSION. U value-only SUMMARY přibyla 3 odmítnutí kvůli kódování (`SUMMARY;CHARSET=utf-8`, 2× `SUMMARY;ENCODING=QUOTED-PRINTABLE`). Odmítnutí je správné chování; volající pak musí zvolit fallback.

**Oprava proti první verzi PR:** tvrzení „nevznikl tichý chybný zápis“ platilo jen díky kontrole re-parse v harnessu (`main.rs`), ne díky samotnému `rawpatch.rs`. Nezávislé review našlo, že value-only splice jednořádkového QP (`TEL;CELL;ENCODING=QUOTED-PRINTABLE:+420=20123`, nová hodnota `ext=12 café`) ponechal `ENCODING=QUOTED-PRINTABLE` nad dekódovanou hodnotou. Klient by pak četl `ext\x12 cafÃ©`. V korpusu se to neprojevilo, protože tam takový TEL nebyl. Teď `rawpatch.rs` takové řádky odmítá a po každém splice sám ověřuje re-parse proti očekávanému AST. Případ pokrývá regresní test `single_line_qp_is_refused_not_corrupted`.

**Raw splice** zapisuje celý řádek ze serializace calcardu. Mění tedy i zápis jména a parametrů cílového řádku: TEL v 5/8 (synth) a 96/144 (calcard-vcard), například `type=cell;type=voice` → `TYPE=CELL,VOICE`. **Value-only splice** tento problém nemá, protože jméno a parametry ponechá v původním zápisu. Pro produkci je proto správná varianta value-only. Pokud se mění parametr, musí vrstva přepsat jen dotčený parametr.

### 4.4 Konverze na JSCalendar / JSContact

Údaje jsou ve formátu „vlastnosti pryč / řádky ne-verbatim“ po cestě X → JS → X.

| Korpus | JSCalendar +raw | JSCalendar −raw | JSContact +raw | JSContact −raw |
|---|---|---|---|---|
| musubi | 0 / 6 z 793 | 142/793 pryč | – | – |
| synth | 0 pryč | 230/511 pryč | 0 pryč, 74/158 řádků změněno | 27/158 pryč |
| calcard-ical | 0,5 % pryč (1 288/1 288 objektů převedeno) | ~40 % pryč | – | – |
| calcard-vcard | – | – | 0,2 % pryč, 32,9 % řádků změněno | 8 % pryč |

Podrobnosti k jednotlivým buňkám:

- **Ztráty u JSCalendar +raw (calcard-ical)**: LOCATION 211, METHOD 146, EXDATE 122 a další. Zda část z nich jen přechází do jiné JSON reprezentace, jsem nerozlišoval **(neověřeno)**.
- **JSCalendar −raw**: ztratí se DTEND (převede se na `duration`), DESCRIPTION v některých kontextech, X-* a VTIMEZONE.
- **JSContact −raw**: ztratí se X-ABLabel, X-ADDRESSBOOKSERVER-MEMBER a další Apple rozšíření.

Cesta JSContact → vCard 3.0 navíc generuje parametry `PROP-ID` a `JSCOMPS` a rozšířené komponenty ADR podle RFC 9554. Starší klienti s tím počítat nemusí **(neověřeno na reálných serverech)**.

JS* reprezentace je tedy použitelná jako **čtecí/doménový model**, pokud se zapne raw režim. Jako zdroj pro zápis zpět do iCal nebo vCard se nehodí.

### 4.5 Expanze opakování (`expand_dates`)

Tyto případy vyšly správně (ručně zkontrolováno):

| Soubor | Pokrývá | Instance |
|---|---|---|
| RFC 5545 | RRULE + RDATE + EXDATE + override | 4 |
| Outlook | Windows TZID „Central Europe Standard Time“ + EXDATE + override (posun na 10:00) | 9 |
| Thunderbird | měsíční opakování přes přechod na letní čas v Praze (+0100 → +0200) | 1 000 (limit) |
| Radicale (Musubi) | přechod CET/CEST | 4 |

Problémy:

- **Nerozpoznaná vlastní VTIMEZONE se tiše ignoruje.** Calcard převádí TZID na zónu jen podle jména: IANA, `X-LIC-LOCATION`, `X-MICROSOFT-CDO-TZID`, nebo Windows název. Pravidla `TZOFFSETFROM/TO` uvnitř VTIMEZONE **nevyhodnocuje**. Soubor `edge_custom_vtimezone_unmapped.ics` (TZID „Musubi Custom Zone“, +0530) se proto expandoval jako `10:00+0000` místo `10:00+0530`, a to bez chyby i bez varování.
- **Rozhraní je chudé.** `expand_dates` bere jen limit počtu instancí, ne časové okno. Výsledky nejsou seřazené. `MissingDtStart` u Musubi fixture `event_delivery` je korektní, protože ta VEVENT DTSTART skutečně nemá.
- **`calcard-next` 3.0.1** (jiff místo chrono) **jsem netestoval (neověřeno)**. Vyšlo 5 verzí během 9.–10. 11. 2026 a každá má jen ~10–17 stažení. Na přechod je příliš čerstvé.

### 4.6 Výkon (release, 50 000 objektů, nejlepší ze 3 běhů)

| Scénář | Velikost | Parse | Další |
|---|---|---|---|
| 1 VCALENDAR s 50k VEVENT | 36,7 MB | 0,174 s, **210,6 MB/s**, 3,5 µs/událost | serializace 372,5 MB/s, výstup není bajtově identický |
| 50k samostatných resource (každý s VTIMEZONE) | 59,1 MB | 0,530 s, **111,5 MB/s**, 10,6 µs/resource | `into_jscalendar` 9,3 µs/resource, `expand_dates` (všechny) 0,048 s, 95 000 instancí |
| 50k vCard 3.0 | 20,0 MB | 0,117 s, **170,7 MB/s**, 2,3 µs/karta | – |

Výkon pro sync engine bohatě stačí. Úzkým hrdlem bude síť a databáze, ne parser.

### 4.7 Zralost, licence, údržba

- **Licence**: calcard je Apache-2.0 OR MIT. Strom závislostí má 43 crates, všechny permisivní (MIT/Apache, BSD-3 `sha1_smol`, Unicode-3.0 `unicode-ident`, BSD-2/Apache/MIT `zerocopy`). **Copyleft ve stromu není.** Část testovacích fixtures calcardu pochází z LGPL a GPL projektů (Evolution, Nextcloud, Roundcube, Zimbra), celé soubory jen čteme z registry a nevendorujeme je. `results/*.lost.txt` a `*.tsv` ale obsahují krátké doslovné výňatky jednotlivých řádků z těchto fixtures (viz `corpus/SOURCES.md`).
- **Kadence vydávání**: verze 0.3.14 vyšla 2026-09-15. Od 2025-05-10 vyšlo 20 releasů a v srpnu 2026 jich bylo 5 (0.3.9–0.3.13).
- **Stažení**: 90 798 celkem, z toho 47 208 za poslední období. Hlavním uživatelem je Stalwart Mail Server, takže crate je odzkoušený v produkci jednoho velkého CalDAV/CardDAV serveru.
- **API a dokumentace**:
  - Verze je stále 0.x a API není stabilní.
  - README slibuje dokumentaci builderů jako „coming soon“.
  - `Display` pro JSCalendar a JSContact existuje jen pod `cfg(test)`, takže musíme volat `serde_json::to_string(&js.0)`.
  - `ConversionOptions` jsou zanořené v modulech `jscalendar::import` / `jscontact::import`.
  - `Entry::TooManyComponents` vrací parser jen při přetečení čítače ID komponent (`src/icalendar/parser.rs:100`); v praxi nedosažitelné.
  - Na API je potřeba pinnout přesnou verzi a mít vlastní tenký adaptér.

### 4.8 Prior art v Musubi

Musubi (TypeScript) už exaktní zachování raw ICS dělá:

- kód: `prepareCaldavSeriesWrite` v `apps/api/src/sync/adapters/caldav.ts`, `caldav_scope.ts`, `event_resolution.ts`;
- testy: `caldav_series.test.ts` a `caldav_series_zone.test.ts`, které logují „exact resource preservation“.

Princip raw patche je tedy v projektu ověřený. Musubi-sync ho převezme, jen nad jiným parserem.

## 5. Verdikt a doporučení

**Verdikt: adopt s vlastní patch vrstvou.**

Calcard použijeme na:

- parse a typovaný AST;
- expanzi opakování;
- konverzi na JSCalendar/JSContact jako čtecí model, vždy se zapnutými `include_ical_components(true)` a `include_vcard_parameters(true)`.

Zakázané cesty:

- zápis přes `ICalendar::to_string()` nebo `VCard::to_string()` celého objektu;
- jakákoli cesta JS* → iCal/vCard pro zápis.

Doporučené kroky:

1. **Raw patch vrstva** v musubi-sync jako samostatný modul nad calcardem, podle vzoru `src/rawpatch.rs`:
   - zarovnat raw logické řádky s entries;
   - měnit jen cílový řádek;
   - u změny hodnoty dělat value-only splice (původní jméno a parametry, folding na 75 oktetů, zachovat EOL payloadu);
   - **value-only splice odmítnout u každého řádku s `ENCODING` (QP/B/BASE64) nebo `CHARSET`**, včetně holých parametrů vCard 2.1; takový řádek přepsat celý, nebo patch odmítnout;
   - **po každém splice payload znovu parsovat a porovnat AST s očekávaným**, a při nesouladu odmítnout. Tato kontrola musí být součástí vrstvy samotné, ne jen testů nebo volajícího;
   - když zarovnání nesedí, **odmítnout** a nahlásit chybu, nikdy nepřegenerovat celý objekt.

   Prototyp má 202 řádků a pokrývá jen změnu hodnoty existující vlastnosti. Produkční odhad je **~400–800 řádků + testy (neověřeno)**. Navíc bude potřeba:
   - přidávat a odebírat vlastnosti;
   - přidávat a odebírat komponenty (override VEVENT, EXDATE);
   - měnit jednotlivé parametry;
   - zvládnout vCard 2.1 QP (měkká zalomení) a čisté CR EOL;
   - property testy „mění se jen cílový řádek“.

2. **Detekce nerozpoznané VTIMEZONE.** Při ingestu zkontrolovat každé TZID přes calcardův resolver (`build_tz_resolver().resolve()`). Pokud se nerozpozná, označit objekt a nespouštět tichou expanzi v UTC. Vlastní vyhodnocovač pravidel VTIMEZONE lze doplnit později, pokud se takové zóny u providerů objeví.

3. **Adaptér s pinnutou verzí.** Pinnout `calcard = "=0.3.x"` a izolovat ho za vlastním rozhraním. Upgrady a případný přechod na `calcard-next` pak řešit vědomě, s tímto harnessem jako regresním testem.

4. **Expanze.** Obalit `expand_dates`: seřadit výsledky a ořezat je podle časového okna. Limit počtu nastavit dost vysoko pro dlouhé série.

Varianta „reject“ by znamenala napsat vlastní parser. Pro to není důvod: calcard parsuje téměř všechno, neztrácí data na úrovni AST a je rychlý. Jediný zásadní problém je serializace, a ta se stejně musí nahradit raw patchem. Pravidlo projektu ji vylučuje u jakékoli knihovny.

## 6. Otevřená rizika

- **Parse rate 98,9 % je nejspíš optimistický.** Pochází z vlastních fixtures calcardu, na kterých autoři knihovnu ladili. Na cizích datech může být úspěšnost nižší **(neověřeno)**.
- **Korpus „provider-style“ je syntetický.** Reálné payloady od Google, Outlook/Exchange, iCloud a Nextcloud **(neověřeno)** mohou obsahovat konstrukce, které tu chybí. Než bude rozhodnutí finální, je potřeba harness pustit nad anonymizovanými skutečnými payloady.
- **Odhad produkční patch vrstvy** (400–800 řádků) **(neověřeno)**. Nejrizikovější části jsou přidávání a odebírání komponent u override instancí a vCard 2.1 QP.
- **Kódované hodnoty a ověření po zápisu.** Value-only splice nad řádkem s `ENCODING`/`CHARSET` tiše poškodí data (doloženo review, viz 4.3). Produkční vrstva proto musí takové řádky odmítat a po každém splice ověřit re-parse proti očekávanému AST. Bez toho se chybné zápisy nedají spolehlivě vyloučit.
- **Tiché selhání u vlastní VTIMEZONE**: bez kroku 2 vede ke špatným časům bez jakékoli chyby.
- **API 0.x a `calcard-next`**: API se může rozbít a směr vývoje (chrono vs jiff) zatím není jasný. `calcard-next` jsem netestoval **(neověřeno)**.
- **Normalizace v round-tripu u X-vlastností Apple a Android**: když se omylem použije plná serializace, hrozí sémantická změna dat. Tomu musí zabránit review a testy.
- **JSContact → vCard 3.0 s `PROP-ID` a `JSCOMPS`**: chování iCloudu a Google při zápisu takových karet, a stejně tak `CHARSET=UTF-8` ve vCard 3.0, je **(neověřeno)**. Podle doporučení se tato cesta nemá pro zápis používat vůbec.
- **Benchmark běžel na jednom stroji s generovanými daty.** Výkon na reálném mixu payloadů je **(neověřeno)**, ale rezerva je o řády.
