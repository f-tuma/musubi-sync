# musubi-sync: návrh samostatného synchronizačního primitiva

**Datum:** 11. 11. 2026 · **Stav:** návrh k rozhodnutí, revize 7 · **Rozsah:** výzkum a architektura, žádná změna kódu

Revize 7 zapracovává výsledky fáze 0 ([`docs/research/phase0/`](../research/phase0/)) a rozhodnutí uživatele o úložišti ze třetího kola: SQLite, jeden soubor na tenanta, s oddělenou databází outboxu (kapitoly 4.2–4.4 a 11.4). Upravuje cíle rychlosti podle měření (5.1), volbu knihoven (8), rizika (11.3) a tvrzení o poskytovatelích, která fáze 0 ověřila nebo vyvrátila. Plán fáze 1 je v [`docs/plan/phase1.md`](../plan/phase1.md). Revize 6 zapisuje přijetí verdiktu o federaci a přechod Musubi najednou (kapitola 11.4). Revize 5 zapracovává rozhodnutí o ověřování a pozvánkách, která uživatel svěřil koordinátorovi. Jádro **nemá přihlašování koncových uživatelů** (kapitoly 1 a 7) a **koncové šifrování není cílem**. Pozvánky jsou nastavené tak, aby uživatel viděl **jednu pozvánku, úplný seznam účastníků a žádný spam** (kapitoly 12.7, 14.6, 14.7 a 14.9). Revize 4 přidala pravidla synchronizace v jádru ([kapitola 13](#13-pravidla-synchronizace)), plánování mezi tenanty ([kapitola 14](#14-plánování-mezi-tenanty)), posouzení federace ([kapitola 15](#15-federace)) a volitelné „smazat všude" (kapitola 12.7). Model jednoho záznamu na více místech je v [kapitole 12](#12-jeden-záznam-na-více-místech-entita-a-umístění). Stav rozhodnutí je v kapitole 11.4.

Projekt se jmenuje **musubi-sync** (repozitář `f-tuma/musubi-sync`, démon `musubi-syncd`). Je to samostatná služba a knihovna, která stahuje data z mnoha účtů u různých poskytovatelů do jednoho místa, drží je aktuální, umí do nich zapisovat zpět, udržuje jeden záznam shodný na všech jeho umístěních, zrcadlí záznamy mezi kalendáři podle pravidel uživatele, doručuje pozvánky mezi lidmi na stejném serveru a aplikacím nabízí rychlé čtení a proud změn. Musubi je první aplikace nad ním, ne jeho součást.

Tvrzení o API poskytovatelů jsou ověřená z webu a mají odkaz v oddílu [Zdroje](#zdroje). Co ověřené není, je označeno **(neověřeno)**. Tvrzení o Musubi vycházejí z cíleného čtení kódu, ne z profilování.

---

## 0. Shrnutí doporučení

Rozhodnutí uživatele z 11. 11. 2026, která návrh bere jako daná:

- projekt je **open source pod licencí MIT**, jmenuje se **musubi-sync** a Musubi je jeden z jeho spotřebitelů;
- hlavní požadavek: **jeden záznam může žít na více místech zároveň** a jádro drží všechna umístění shodná v obou směrech;
- cílem je **rychlost samotného primitiva** (každá operace hluboko pod 1 s), ne lokální replika v klientovi;
- **kontakty patří do první vlny** spolu s událostmi, úkoly a free/busy;
- ověření Microsoft Graphu je **ve fázi 0**; dokumentační část je hotová, živé testy na účtech (T1–T12) se přesunuly před fázi, která je potřebuje (kapitola 11.1);
- **tenant je jeden člověk.** Sdílené tenanty nejsou; lidé na stejném serveru spolu jednají pozvánkami, které doručuje jádro;
- **pravidla synchronizace žijí v jádru** a běží bez aplikace; maska „Obsazeno" je průmět pravidla;
- **smazání:** výchozí stav je „zaniká jen to umístění", ale **„smazat všude" je plnohodnotné nastavení** na záznamu i na pravidle;
- **data aplikací** (štítky, poznámky) žijí nad jádrem;
- **značka entity** ve skrytém poli položky u poskytovatele je povolená;
- **federace mezi servery:** ano, pokud je proveditelná a bezpečná;
- **ověřování** (rozhodl koordinátor z pověření uživatele): jádro nemá přihlašování koncových uživatelů. Aplikace ověřují své uživatele samy a jádro přijímá klíče API, krátkodobé tokeny a tokeny z vnějšího IdP;
- **pozvánky** (rozhodl koordinátor z pověření uživatele, s cílem co největšího pohodlí): jedna pozvánka, úplný seznam účastníků pro všechny, žádný spam;
- **koncové šifrování** není cílem.

Doporučení:

1. **Jeden záznam, více umístění.** Aplikace pracuje s *entitou*. Entita má jedno nebo víc *umístění* a každé umístění je položka u jednoho poskytovatele s vlastním vzdáleným klíčem, validátorem a surovými daty. Pravdou je kanonický dokument entity. Umístění ho mění jen rozdíly, takže slabší poskytovatel nezničí bohatší data. Topologie je hvězda, ne páry. Podrobně kapitola 12.
2. **Bezpečná pravidla pro schůzky.** Účastníky nese nejvýš jedno hlavní umístění. Ostatní jsou soukromé kopie bez účastníků s vlastním UID, takže se pozvánky nezdvojují.
3. **Pravidla synchronizace v jádru.** Pravidlo říká, která umístění mají existovat: rozsah a filtr, cíl, směr, průmět (celý záznam nebo maska „Obsazeno") a politika mazání. Vyhodnocuje se v transakci každé změny. Kopie vytvořená pravidlem není zdrojem žádného pravidla, takže A → B spolu s B → A nevytvoří smyčku. Podrobně kapitola 13.
4. **Mazání.** Politika `on_remote_delete: detach | propagate` na záznamu, na pravidle a jako výchozí hodnota tenanta. Rozeslání smazání jen na výslovný důkaz, s pojistkou hromadného mazání a s lhůtou na obnovu. Kapitola 12.7.
5. **Plánování mezi tenanty.** Entitu vlastní tenant organizátora. Pozvaný dostane příchozí pozvánku do interní schránky: sdílená pole jen pro čtení, vlastní odpověď a připomenutí, kopie ve vlastních kalendářích ručně nebo pravidlem. Zprávy REQUEST, REPLY a CANCEL se sémantikou iTIP jdou z trvalého outboxu do inboxu příjemce. Místní účastník je zapsaný i u poskytovatele organizátora, takže úplný seznam účastníků vidí všichni. Pozvaný přesto vidí jednu pozvánku: doručení jádrem a poskytovatelem se u něj spojí podle dvojice (organizátor, UID). Pozvánka od neznámého odesílatele čeká mezi žádostmi. Kapitola 14.
6. **Federace: jen v zúženém rozsahu a až nakonec.** Proveditelná a dostatečně bezpečná je pouze pro plánovací zprávy, vypnutá ve výchozím stavu, s vyjmenovanými partnerskými servery, podepsanými požadavky a souhlasem příjemce. Otevřená federace s neznámými servery bezpečná není. Kapitola 15.
7. **Tvar produktu:** open-source jádro jako knihovna v Rustu a nad ní démon `musubi-syncd`, který si kdokoli nasadí sám jako jeden binární soubor. Datový model je multi-tenant; hostování pro mnoho tenantů je jeden z režimů nasazení, ne předpoklad.
8. **API pro aplikace:** HTTP + JSON s OpenAPI nad entitami, k tomu **proud změn** (kurzor `GET /changes?since=` a živý kanál přes SSE). Sémantika je převzatá z JMAP, drátová kompatibilita s JMAP ani CalDAV fasáda v první verzi nejsou.
9. **Data položky ve třech vrstvách:** surová data poskytovatele beze změny, kanonický tvar podle JSCalendar (RFC 8984) a JSContact (RFC 9553) a odvozené indexy. Zápis zpět je vždy **záplata nad posledním surovým stavem**, takže neznámá pole přežijí.
10. **Obecná obálka místo kalendářového schématu:** jádro zná jen účet, kolekci, položku, entitu, umístění, pravidlo, kurzor, změnu, mutaci a zprávu. Události, úkoly, kontakty, později e-maily a zprávy jsou „druhy" (kinds) s vlastním kanonickým schématem a indexy.
11. **Sync engine:** jeden aktor na účet se stavovým automatem, trvalá fronta úloh, **push všude, kde existuje** (Google watch, Graph subscriptions), adaptivní dotazování jinde. Stránka změn, sloučení do entity, vyhodnocení pravidel, indexy, záznam do žurnálu a posun kurzoru se zapisují v jedné transakci.
12. **Zápisy:** mutace míří na entitu a přijme se v milisekundách. Na každé umístění jde vlastní podmíněný zápis (ETag / changeKey) s vlastním stavem doručení. Konflikty se slučují třícestně po polích; u stejného pole vyhrává později zpracovaná změna a poražená hodnota se uloží. Při nejistém výsledku nejdřív pozorování, teprve potom opakování.
13. **Úložiště (rozhodnuto 11. 11. 2026):** primitivum drží autorská data. Pro fázi 1 i self-host je to **SQLite, sada souborů na tenanta**: hlavní databáze a oddělená databáze outboxu. Outbox je zdrojem pravdy pro mutace, hlavní databáze ho dohání podle značky (watermark). Atomicita přes dva soubory se nepředpokládá. Vlastní checkpointer, omezené čtecí transakce a zálohování jsou součást první fáze. PostgreSQL zůstává za úzkým rozhraním úložiště jako pozdější práce pro hostovanou instanci se SLA. Kapitola 4.
14. **Rychlost:** čtení v jednotkách milisekund, změna u poskytovatele s pushem v proudu změn do 500 ms od oznámení, zápis potvrzený poskytovatelem typicky do 1 s. Přijetí mutace má p99 pod 10 ms jen mimo import stejného tenanta. Během importu je cíl p99 pod 20 ms (měřeno na btrfs bez PLP; ext4 a disk s PLP neověřeno). Podrobně kapitola 5.
15. **Adaptér poskytovatele:** oddělený **transport** (HTTP, kurzory, push) a **kodek** (čisté funkce mezi surovými daty a kanonickým tvarem, verzované). Schopnosti jsou strukturovaná data vystavená aplikaci. Rozhraní se zmrazí až po dvou adaptérech.
16. **Rust ano.** Dnešní pomalost způsobila architektura, ne jazyk, a propustnost jednoho účtu omezuje poskytovatel. Jazyk rozhoduje při růstu.
17. **Plán:** fáze 0 je hotová (úložiště, calcard, rekurence, dokumentace poskytovatelů). Živé testy na účtech (T1–T12) se přesunuly před fázi, která je potřebuje; série v Graphu (T1) blokují jen fázi 7. Potom Google pro čtení, CalDAV a CardDAV, zápisy, více umístění, **pravidla synchronizace**, **plánování mezi tenanty**, Microsoft a vydání 1.0. Sdílení kalendářů ke čtení a federace přicházejí po něm. Kapitola 11.1.
18. **Rozsah první vlny:** celá rodina Cal (události, úkoly, kontakty, free/busy) u Googlu, Microsoftu, iCloudu a obecného CalDAV/CardDAV. Výjimka: úkoly z iCloudu nejsou přes CalDAV dostupné.

---

## 1. Tvar produktu

### Doporučení

**Knihovna i služba, ze stejného kódu.** Workspace má tři vrstvy:

| Vrstva | Obsah | Kdo ji použije |
| --- | --- | --- |
| `core` | model, kodeky, rekurence, engine, rozhraní úložiště a adaptéru | všichni |
| `server` (`musubi-syncd`) | HTTP API, příjem webhooků, plánovač, správa tenantů, OAuth k poskytovatelům, ověřování průkazů volajících | provozovatel služby, self-host |
| `sdk` | tenké klientské knihovny generované z OpenAPI, TypeScript jako první | aplikace |

Lokální replika na zařízení mezi vrstvy nepatří; je to volitelná budoucí práce (kapitola 5.8). Vestavěný režim (engine přímo v desktopové aplikaci) z knihovny vypadne zadarmo, ale cílem první verze není.

**Open source, self-host jako výchozí případ.** Projekt žije ve vlastním repozitáři mimo Musubi a Musubi ho používá přes veřejné API jako kterákoli jiná aplikace. Z toho plyne:

- **Nasazení:** jeden statický binární soubor a obraz kontejneru, konfigurace jedním souborem, žádná povinná externí služba.
- **Vlastní registrace u poskytovatelů:** každý provozovatel si registruje vlastní OAuth aplikaci u Googlu a Microsoftu. Projekt žádnou sdílenou aplikaci nedodává, dodává návod krok za krokem.
- **Push je volitelný:** kdo nemá veřejnou adresu, běží na dotazování. Stejná fronta a stejné jádro, jen vyšší zpoždění.
- **Dokumentace je součást produktu:** OpenAPI, rychlý start, návod pro každého poskytovatele, matice schopností generovaná z testů, příručka pro autory adaptérů.
- **Stabilita:** veřejné API a rozhraní adaptéru mají vlastní verze a pravidla kompatibility.
- **Licence: MIT** (rozhodnutí uživatele). Důsledky:
  - MIT nemá výslovnou patentovou licenci jako Apache-2.0. Je to vědomá cena za nejjednodušší a nejznámější licenci.
  - Závislosti pod MIT, BSD, ISC a Apache-2.0 použít lze. U závislostí jen pod Apache-2.0 musí binární distribuce nést jejich licenční texty a soubory NOTICE.
  - Copyleft závislosti zůstávají vyloučené: `libdav` (EUPL-1.2) použít nejde, `calcard` (Apache-2.0 nebo MIT) ano.
  - DAVx⁵ (GPL-3.0) a Nango (Elastic License 2.0) jsou zdroj znalostí, ne kódu (licence ověřeny ve fázi 0).
  - Licence závislostí hlídá v CI `cargo-deny` s výčtem povolených licencí; přehled pro distribuci generuje `cargo-about`.

**Multi-tenant v modelu, ne v předpokladu provozu.** Hierarchie dat je `tenant → účet → kolekce → položka` a vedle ní `tenant → entita → umístění`. **Tenant je jeden člověk** (rozhodnutí uživatele) a je to tři věci najednou:

- **hranice propojování:** do jedné entity lze spojit jen položky z účtů téhož tenanta a pravidla synchronizace pracují jen uvnitř něj;
- **hranice izolace:** vlastní úložiště a šifrovací klíč;
- **jednotka zálohy, exportu a smazání.**

Sdílené tenanty domácností a týmů nejsou. Lidé na stejném serveru spolu jednají **zprávami mezi tenanty**: organizátor vlastní záznam a pozvaný dostane příchozí pozvánku do své schránky (kapitola 14). Rodinná událost v kalendářích obou partnerů je tedy pozvánka, ne sdílená entita. Nikdo nečte cizí úložiště.

Self-host pro jednoho člověka je nasazení s jedním tenantem, hostování pro mnoho tenantů je podporovaný režim téhož binárního souboru. Rozdíl je konfigurace, ne větev kódu.

**Bez vlastního přihlašování uživatelů** (rozhodnuto). musubi-sync nemá hesla, obnovu hesla, dvoufázové ověření, relace ani e-maily. Uživatele ověřuje aplikace; Musubi si nechává better-auth v TypeScriptu. Jádro zná jen tenanta a přijímá tři druhy průkazů:

| Průkaz | Pro koho | Jak vzniká |
| --- | --- | --- |
| klíč API vázaný na tenanta | server aplikace | vydá ho správcovské volání instance |
| krátkodobý token s rozsahem | klient (prohlížeč, mobil) | server aplikace ho získá za svůj klíč voláním `POST /v1/tokens` a předá klientovi |
| přístupový token z vnějšího IdP (OIDC, JWT) | samostatný self-host bez vlastní aplikace | vydá ho IdP provozovatele; jádro ho ověří a přiřadí tenantovi |

Přihlášení k **poskytovatelům** (OAuth Googlu a Microsoftu, hesla pro aplikace) v jádru zůstává: je to jeho hlavní práce. Podrobně kapitola 7.

**API: HTTP + JSON a proud změn.**

```
POST /v1/accounts                připojení účtu (OAuth odkaz, nebo import hotového grantu)
GET  /v1/accounts/{id}           stav synchronizace, schopnosti, poslední chyba
GET  /v1/collections             kalendáře, seznamy úkolů, …
GET  /v1/entities?kind=event&from=…&to=…&expand=occurrences   hlavní čtení, jeden řádek na záznam
GET  /v1/entities/{id}?include=placements
POST /v1/entities/{id}/placements   umístit do další kolekce, nebo připojit existující položku
GET  /v1/items/{id}?include=raw  nízkoúrovňový pohled na jednu kopii u poskytovatele
POST /v1/mutations               dávka zápisů entit, každý s idempotency klíčem
GET  /v1/changes?since=<kurzor>  změny od kurzoru
GET  /v1/stream                  SSE: nové změny nebo jen „kurzor se posunul"
POST /v1/freebusy
POST /v1/tokens                  krátkodobý token pro klienta (kapitola 7)
GET  /v1/rules                   pravidla synchronizace (kapitola 13.9)
GET  /v1/scheduling/…            plánovací adresy, odesílatelé, nastavení (kapitola 14.11)
```

Úplný seznam volání pro umístění je v kapitole 12.11, pro pravidla v 13.9 a pro plánování v 14.11.

Pro serverové aplikace navíc odchozí webhook „tenant X má změny od kurzoru Y". Nese jen kurzor, data si aplikace vyzvedne přes `/changes`.

### Zamítnuté alternativy

- **Jen knihovna.** Nevyřeší příjem webhooků, obnovu subscriptions ani práci na pozadí, když je aplikace vypnutá. Každá aplikace by si démona stejně napsala.
- **Jen hostovaná služba.** Odporuje rozhodnutí o open source a self-hostu.
- **GraphQL.** Hlavní dotazy jsou dva: rozsah v čase a změny od kurzoru. GraphQL k nim nic nepřidá a zkomplikuje cache, limity i dokumentaci.
- **gRPC jako hlavní rozhraní.** V prohlížeči potřebuje proxy a třetí strany ho hůř ladí. Může přibýt později pro komunikaci server–server.
- **JMAP jako drátový protokol v první verzi.** JMAP je nejlepší existující vzor (stavové řetězce, `/changes`, push, více účtů, pošta i kalendáře). Proti němu mluví tři věci: JMAP for Calendars a JSCalendar 2.0 jsou v listopadu 2026 stále jen drafty ve frontě RFC editoru a JMAP Tasks je propadlý draft; JMAP předpokládá jeden autoritativní server a nemá pojmy pro stav doručení zápisu k cizímu poskytovateli ani pro rozdílné schopnosti účtů; plná implementace dávkování se zpětnými odkazy je drahá. Proto: **převzít sémantiku, ne formát**, a JMAP fasádu nechat jako pozdější samostatný crate.
- **CalDAV fasáda v první verzi.** Dala by okamžitou kompatibilitu s Apple Kalendářem, Thunderbirdem a DAVx⁵ jako klienty. Je ale ztrátová vůči našemu modelu (stav doručení, schopnosti) a plánování podle RFC 6638 je velký kus práce. Volitelně později.

---

## 2. Kanonický datový model

### 2.1 Co znamená „kompletní CalDAV sada"

CalDAV (RFC 4791) je obal nad iCalendarem (RFC 5545) a řadou rozšíření. Rozdělení pro první rozsah:

| Oblast | První rozsah | Poznámka |
| --- | --- | --- |
| Kalendáře (kolekce): název, barva, pořadí, výchozí zóna, podporované komponenty, úroveň přístupu | ano | |
| Události (VEVENT): časy, celodenní a plovoucí, RRULE/RDATE/EXDATE, výjimky, zóny, místo, popis, kategorie, stav, viditelnost, konference, přílohy odkazem | ano | |
| Účastníci a RSVP (ORGANIZER, ATTENDEE, PARTSTAT) | čtení hned, zápis ve fázi zápisů | přes implicitní plánování poskytovatele; účastníky na stejném serveru zve jádro (kapitola 14) |
| Připomenutí (VALARM) | ano | schopnosti se velmi liší |
| Úkoly (VTODO) včetně podúkolů a opakování | ano | u Googlu a iCloudu silně omezené, viz kapitola 2.6 |
| Free/busy | ano | z uložených dat plus dotaz na poskytovatele pro cizí kalendáře |
| Kontakty a skupiny kontaktů (CardDAV, People API, Graph contacts) | ano | rozhodnutí uživatele: rodina Cal se uzavírá najednou; model a mapování v kapitole 2.7 |
| VJOURNAL, VAVAILABILITY | jen uložit surově, vystavit pro čtení | Google ani Microsoft ekvivalent nemají |
| Binární přílohy, správa sdílení a ACL u poskytovatelů, delegace | ne | sdílení u poskytovatele jen jako úroveň přístupu ke čtení; sdílení mezi tenanty v kapitole 14.10 |
| Posílání pozvánek e-mailem (iMIP) vlastními silami | ne | patří k doméně e-mailu; vnější účastníky zve poskytovatel |

### 2.2 Tři vrstvy na položku

1. **Surová vrstva.** Odpověď poskytovatele beze změny: text ICS, JSON z Googlu, JSON z Graphu. Komprimovaná, adresovaná hashem obsahu.
2. **Kanonická vrstva.** JSON dokument podle JSCalendar pro události a úkoly, JSContact pro kontakty, později tvar JMAP Email pro poštu. V Rustu vlastní typy, které tvoří profil standardu plus jmenný prostor pro rozšíření.
3. **Odvozená vrstva.** Index výskytů, index účastníků, free/busy, později fulltext. Dá se kdykoli smazat a znovu postavit.

Surová vrstva má tři důvody. Bezeztrátový zápis zpět. Oprava chyby v mapování bez nového stahování: kodek má verzi a položky se přepočítají lokálně. A ladění: u každé podivné události je vidět, co poskytovatel skutečně poslal.

**Proč JSCalendar a ne iCalendar jako kanonický tvar.** JSCalendar je JSON, je jednoznačný, výjimky opakování popisuje jako záplaty nad hlavní událostí (což sedí na model Googlu i Graphu), má čistý model účastníků a existuje definovaný převod z a do iCalendaru, který implementuje crate `calcard`. iCalendar je textový formát s parametry a historickými zvláštnostmi a pro JSON API Googlu a Microsoftu je nepohodlný mezikrok.

**Proč ne úplně vlastní model.** Znovu bychom vymýšleli totéž a přišli o převodníky i o budoucí JMAP fasádu. Vlastní jsou typy a pravidla profilu, ne sémantika.

**Riziko:** JSCalendar 2.0 ještě nevyšel. Kanonické dokumenty proto nesou `schema_version` a migrují se přepočtem ze surové vrstvy.

**Výjimka:** kanonický dokument entity s více umístěními se ze surových dat přepočítat nedá (kapitola 12.2). Každá změna schématu pro něj potřebuje migraci dat s vlastním testem.

### 2.3 Obecná obálka

```
Account     { id, tenant, provider, auth_ref, state, capabilities }
Collection  { id, account, kind, remote_ref, capabilities, cursor, coverage }
Item        { id, account, kind, memberships: [collection],
              remote: { id | href, validator, uid },
              raw_ref, observed, schema_version, mapping_version, tombstone }
Entity      { id, tenant, kind, canonical, rev, seq, state, tombstone }
Placement   { id, entity, item, role, direction, projection, origin,
              delivered, local, fidelity, delivery }
Rule        { id, tenant, source, target, direction, projection, policy, state }
SchedMsg    { id, method, uid, sequence, organizer, recipient, payload }
Change      { seq, entity | placement | collection, op, origin }
Mutation    { id, entity, base_rev, patch, idempotency_key, state }
```

Dvě roviny:

- **Položka** je jedna kopie u jednoho poskytovatele. Autoritou je poskytovatel. `observed` je kanonický tvar dekódovaný ze surových dat.
- **Entita** je záznam, se kterým pracuje aplikace. **Umístění** je vazba entity na položku. Každá položka patří právě do jedné entity. Entita s jedním umístěním vlastní dokument neukládá a čte `observed` své položky; entita s více umístěními nese vlastní kanonickou pravdu. Celý model je v kapitole 12.
- **Pravidlo** říká, která umístění mají existovat (kapitola 13). **Zpráva** nese pozvánku mezi tenanty (kapitola 14).
- **Interní kolekce.** Vestavěný poskytovatel `local` má kolekce a položky, které žijí jen v jádru. První z nich je schránka pozvánek každého tenanta (kapitola 14.5). Pro zbytek modelu je to poskytovatel jako každý jiný.

Jádro o kalendářích nic neví. Tři rozhodnutí, která drží cestu k e-mailu a chatu otevřenou:

- **Členství v kolekcích je množina.** Událost má jednu kolekci, ale zpráva v Gmailu má víc štítků.
- **Kurzor je neprůhledný blob adaptéru.** Sync token, delta odkaz, IMAP `UIDVALIDITY`+`MODSEQ` nebo Gmail `historyId` se do něj vejdou stejně.
- **Velká těla jdou mimo databázi** do úložiště objektů, v databázi je jen odkaz. U kalendářů to není potřeba, u pošty nutnost.

### 2.4 Identita, umístění a korelace

Čtyři různé identity, které se nesmí slít. Musubi k tomuto závěru došlo draze (`docs/research/nylas/01-findings-and-decisions.md`, oddíl 3).

| Identita | Co je | K čemu |
| --- | --- | --- |
| `entity.id` | naše stabilní ID záznamu | odkazy z aplikací, proud změn |
| `item.id` | naše ID jedné kopie v jednom účtu | umístění, ladění |
| vzdálený klíč | účet + kolekce + ID nebo href u poskytovatele + validátor | synchronizace, podmíněné zápisy |
| korelační klíč | shoda napříč účty: u událostí iCalendar UID + RECURRENCE-ID, u pošty Message-ID | zobrazení „jedna schůzka, ne tři" |

Dva vztahy mezi kopiemi:

- **Umístění (vlastníme).** Jeden záznam, který jsme sami umístili na víc míst. Změny propisujeme mezi umístěními my. Kapitola 12.
- **Korelace (pozorujeme).** Tatáž schůzka viděná přes tři účty jsou tři entity se společným korelačním klíčem. Každá má vlastní stav: moje odpověď, moje připomenutí. Nic mezi nimi nepropisujeme, to dělá poskytovatel organizátora. API nabídne pohled `dedup=correlation`, který vybere reprezentanta (kopie organizátora, potom zapisovatelná, potom z primárního účtu) a ostatní uvede.

U korelace platí dál: **propojení, ne sloučení.** Sloučení by při zápisu zpět nevědělo, kterou kopii měnit. Jediná výjimka: tatáž pozvánka doručená jednomu tenantovi schránkou jádra i poskytovatelem se spojí do jedné entity podle přesné identity iTIP (kapitola 14.7).

**Nejistota:** dokumentace Graphu v1.0 říká, že `iCalUId` je identifikátor události napříč kalendáři a že se liší pro každý výskyt opakované série. Korelace série proto musí vycházet z hlavní události. Fáze 0 to z dokumentace potvrdila: pole `uid` (stejné pro celou sérii) je jen v betě. Skutečné chování ověří živý test T1 (krok 5) před fází 7 (kapitola 11.2).

### 2.5 Opakování a rychlé dotazy na rozsah

- **Pravdou je vždy hlavní událost a její výjimky**, nikdy předem rozbalené výskyty.
- **Odvozený index výskytů** `(start_utc, end_utc, entity_id, recurrence_id)` se počítá z kanonického dokumentu entity. Filtr na kolekce jde přes umístění, takže záznam ve třech kalendářích je ve výsledku jednou. Index se počítá při příjmu pro klouzavý horizont (návrh: 2 roky zpět, 5 let dopředu) a líně se prodlužuje.
- Vedle něj **index rozpětí sérií** `(první výskyt, konec nebo nekonečno)`. Dotaz mimo horizont z něj vybere série, které rozsah protínají, a rozbalí je za běhu.
- **Celodenní a plovoucí události** nemají okamžik v UTC. Indexují se s rezervou ±14 hodin a přesně se dofiltrují podle zóny tazatele.
- Index nese **verzi databáze časových zón**. Po její aktualizaci se přepočítají jen série v dotčených zónách.
- Stejný kód rozbalení běží v klientovi (WASM, nativní vazby). Tenký klient dostane výskyty rozbalené serverem.

### 2.6 Mapování po poskytovatelích

**Google**

| Část sady | API | Delta a push | Co chybí nebo je ztrátové |
| --- | --- | --- | --- |
| Kalendáře | Calendar API `calendarList` | sync token, watch kanál | barva je per uživatel |
| Události | Calendar API `events` | sync token na kalendář (nelze kombinovat s časovým filtrem; `410` znamená plný sync), watch kanál | jen relativní připomenutí popup/e-mail; žádné plovoucí časy; role účastníka jen „volitelný"; vlastní X- vlastnosti jen přes `extendedProperties`; speciální typy (focus time, out of office, working location) mají omezení zápisu |
| Úkoly | Tasks API | **žádný sync token, žádný push**; jen `updatedMin` + `showDeleted` s překryvem okna (−1 min) a deduplikací podle etagu; stránka nejvýš 100 | termín jen jako datum, čas se zahodí; opakování, připomenutí a přiřazení API nevystavuje; podúkoly jen přes `parent` |
| Free/busy | `freebusy.query` | — | |
| Kontakty | People API | viz kapitola 2.7 | |

- Limity: Calendar API 600 požadavků za minutu na uživatele a 10 000 za minutu na projekt, klouzavé okno. Tasks API 50 000 dotazů za den jako „zdvořilostní limit". Calendar API má pro projekty založené od 1. 5. 2026 navíc práh 1 000 000 požadavků za den na projekt, nad kterým se bude platit.
- Push: oznámení nemá tělo, jen hlavičky; kanál vyprší a neobnovuje se sám; Google sám píše, že oznámení „nejsou stoprocentně spolehlivá". Po každém oznámení následuje delta dotaz a vedle toho běží pojistné dotazování.
- Idempotentní vytvoření: klient může zadat vlastní ID události. Podmíněný zápis: ETag a `If-Match`.
- Watch kanál: výchozí životnost 604 800 s (7 dní), maximum dokumentace neuvádí. Obnovuje se podle vrácené `expiration`, výchozí plán po 6 dnech. Skutečnou životnost ověří živý test T8 ve fázi 1.
- Stránka `events.list` má nejvýš 2 500 položek (výchozí 250). Stránka může být prázdná a přesto nést `nextPageToken`.
- Dávka batch má nejvýš 1 000 volání a do kvóty se počítá jako N požadavků. Neověřená aplikace s citlivými scope má strop 100 nových uživatelů celkem.

**Microsoft**

| Část sady | API | Delta a push | Co chybí nebo je ztrátové |
| --- | --- | --- | --- |
| Kalendáře | Graph `/me/calendars` | bez delty, porovnání seznamu | barva jen z předvoleb |
| Události | Graph `events`, `calendarView` | ve v1.0 **jen `calendarView/delta`, a jen pro primární kalendář**: pevné časové okno, výskyty už rozbalené, `@removed` chodí i pro události mimo okno, `$select` nejde; neomezená `events/delta` je jen v beta. Subscriptions na `/me/events`; sekundární kalendáře jen přes subscription a dotazování | opakování je vlastní struktura, ne RRULE: žádné RDATE, žádná kombinace pravidel; jedno připomenutí; zóny jako Windows ID; tělo v HTML |
| Úkoly | Graph To Do | delta na seznamy i úkoly, subscriptions | podúkoly jen jako plochý checklist; důležitost ve třech stupních; jedno připomenutí; podmíněný zápis není prokázaný |
| Free/busy | `getSchedule` | — | jen pracovní a školní účty |
| Kontakty | Graph `contacts` | viz kapitola 2.7 | |

- Limity: 10 000 požadavků za 10 minut a 4 souběžné požadavky na schránku; `$batch` nejvýš 20 požadavků, každý se omezuje zvlášť a počítá se do 4 souběžných; při `429` hlavička `Retry-After`. Strop `odata.maxpagesize` není dokumentovaný (T3).
- Subscriptions: pro zprávy, události a kontakty nejvýš 10 080 minut (necelých 7 dní), pro `todoTask` 4 230 minut a jen v globálním cloudu; nejvýš 1 000 aktivních subscriptions na schránku. Latence oznámení pro události je v dokumentaci uvedená jako „neznámá". S bohatými oznámeními je životnost jen 1 440 minut.
- **Důsledek pro návrh:** `calendarView/delta` použít jen jako signál „v okně se něco změnilo" a jako pojistku. Pravdu o sérii načíst z hlavní události a jejích výjimek a uložit **jednu položku na sérii**. Dokumentace v1.0 čtení výjimek (`$expand=exceptionOccurrences`) a zrušených výskytů (`cancelledOccurrences`) u hlavní události popisuje. Fáze 0 potvrdila, že výjimky a zrušené výskyty vrací jen GET hlavní události podle ID s `$select`/`$expand` a že zrušený výskyt je znám jen datem. Chování na skutečných účtech ověří živý test T1 před fází 7. Záložní modely zůstávají (kapitola 11.2).
- Další známé nástrahy: ID se mění při přesunu, pokud se nevyžádají neměnná ID hlavičkou `Prefer: IdType="ImmutableId"`, a to u každého požadavku i u subscription; API pro přesun neexistuje (T2); osobní a pracovní účty se chovají různě; `transactionId` má nedokumentované okno, je to pomocník, ne klíč idempotence, a obnova po nejistém vytvoření je pozorováním (T5); EWS se od 1. 10. 2026 vypíná po tenantech a 1. 4. 2027 končí úplně.

**Apple iCloud**

| Část sady | API | Delta a push | Co chybí nebo je ztrátové |
| --- | --- | --- | --- |
| Kalendáře, události | CalDAV na `caldav.icloud.com`, po přihlášení přesměrování na oddílové hostitele | `sync-collection` tam, kde ji kolekce inzeruje (tak ji používá Musubi), jinak ctag a ETagy; **žádný push pro třetí strany** | chybějící hlášení oprávnění u zdrojů, vlastní chování `calendar-query` (obojí zjištění Musubi) |
| Úkoly | prakticky nedostupné | — | Připomínky od iOS 13 nejsou na CalDAV; staré VTODO kolekce jsou oddělený svět |
| Kontakty | CardDAV | viz kapitola 2.7 | |

- Přihlášení jen heslem pro aplikaci: vyžaduje dvoufázové ověření, nejvýš 25 hesel na účet, při změně hlavního hesla se zruší všechna. OAuth neexistuje. Odpověď `401` znamená výzvu uživateli k novému heslu, ne opakování.
- Limity nejsou dokumentované. Výchozí souběžnost 2 na účet, respektovat `Retry-After` a přizpůsobit se při `503`/`429`.
- `SCHEDULE-AGENT=CLIENT` iCloud podle jediné sekundární zkoušky ignoruje (T12), takže pro plánování se chová jako Graph (kapitola 14.6).
- Poctivý závěr: **iCloud úkoly do první verze nepatří.**

**Obecný CalDAV**

- Sada je podle specifikace úplná, ale každý server umí něco jiného: `sync-collection` (RFC 6578) je volitelná, plánování (RFC 6638) je volitelné, VTODO se inzeruje per kolekce.
- Delta má tři stupně podle schopností: `sync-collection`; ctag kolekce a porovnání ETagů; úplný výpis.
- Push: WebDAV-Push je soukromý draft -00 mimo IETF Datatracker. Umí ho DAVx⁵, Nextcloud přes rozšíření, RustiCal a Xandikos; Radicale a Stalwart ne. Výchozí je dotazování, WebDAV-Push je volitelná schopnost po 1.0.
- Zjišťování služby podle RFC 6764. Přihlášení Basic, Digest nebo OAuth.
- Limity neznámé: 1–2 souběžné požadavky na účet, multiget po desítkách položek.
- Potřebujeme **profily serverů** (Nextcloud/Sabre, Radicale, Baikal, Fastmail, Stalwart, Zimbra, …) jako data, ne jako větve kódu.

### 2.7 Kontakty

Kontakty jsou druhy `contact` a `contact_group` ve stejné obálce jako události. Kanonický tvar je JSContact (RFC 9553); převod z a do vCard umí `calcard`. Kolekce je adresář: CardDAV addressbook, složka kontaktů v Graphu, u Googlu jediná implicitní kolekce „Moje kontakty".

Čím se kontakty liší od událostí:

- **Surová vrstva je ještě důležitější.** vCard nese libovolné X- vlastnosti a Apple seskupuje pole předponou `itemN.` s vlastními popisky. Zápis je záplata nad surovou kartou.
- **Skupiny mají tři podoby.** Google: štítky, kontakt může být ve více skupinách. Graph: složky a kategorie. CardDAV: samostatná karta skupiny se seznamem členů (vCard 4 `KIND:group`, u Applu `X-ADDRESSBOOKSERVER-KIND`). Kanonicky je skupina položka se seznamem členů a kodek ji mapuje.
- **Fotky jsou binární.** Ukládají se mimo databázi podle hashe a v API jsou odkazem.
- **Indexy:** normalizovaný e-mail a telefon (dotaz „kdo je tento účastník") a jméno pro hledání podle předpony.
- **Propojení** jen jako návrh podle shodného e-mailu nebo telefonu. Zda dva kontakty spojit do jedné entity se dvěma umístěními, rozhoduje aplikace (kapitola 12.10).

| Poskytovatel | API | Delta a push | Zápis | Co chybí nebo je ztrátové |
| --- | --- | --- | --- | --- |
| Google | People API: `people.connections` (Moje kontakty), `contactGroups`, `otherContacts` | sync token platí 7 dní, potom plný sync; token se musí vyžádat a parametry dotazu se mezi koly nesmí změnit; `contactGroups` má vlastní sync token; **žádný push** | `updateContact` s výčtem měněných polí; vyžaduje aktuální etag, nesoulad vrací `400 failedPrecondition` a mapuje se na konflikt; zápisy sekvenčně (souběžnost 1); značka entity v `clientData`; fotka samostatnou metodou | `otherContacts` jsou jen pro čtení a mají tři pole (jména, e-maily, telefony); libovolné vlastnosti jen jako páry klíč–hodnota; vlastní zápis se v deltě objeví až po minutách |
| Microsoft | Graph `/me/contacts`, `/me/contactFolders/{id}/contacts` | delta po složkách: `/me/contactFolders/delta`, potom `contactFolders/{id}/contacts/delta`; delta bez složky (`/me/contacts/delta`) nepoužíváme, dokládá ji jen Q&A (T7); subscriptions s životností pod 7 dní, latence oznámení v průměru pod minutu, nejvýš 3 minuty | PATCH s `If-Match` z `@odata.etag`, ne `changeKey` (dokumentace to nepotvrzuje, T6); DELETE je nepodmíněný; fotka přes `/photo/$value` | pevné schéma: telefony podle typu, omezený počet e-mailů (přesný počet neověřen, T6), žádné libovolné vlastnosti kromě rozšíření |
| iCloud | CardDAV na `contacts.icloud.com`, přesměrování na oddílové hostitele | ETagy; podporu `sync-collection` dokládají jen cizí projekty (neověřeno, T11), zjišťuje se z `supported-report-set`; žádný push | PUT s `If-Match`; vdirsyncer hlásí u `If-Match` odpověď `403` (neověřeno, T11) | jen vCard 3.0; skupiny jako samostatné karty s vlastnostmi Applu |
| Obecný CardDAV | RFC 6352 | `sync-collection` je volitelná, jinak ctag a ETagy; `addressbook-multiget` | PUT s `If-Match`, vytvoření s `If-None-Match: *` | vCard 3.0 je povinná, 4.0 volitelná; podoba skupin se liší podle serveru, patří do profilu |

Dva důsledky pro engine:

- **Po vlastním zápisu do Googlu nečekat na deltu.** Novým stavem je odpověď na zápis; delta ho potvrdí až po minutách.
- **Kontakty u Googlu a iCloudu nemají push**, takže se na ně vztahuje adaptivní dotazování z kapitoly 3.2.

Minutové kvóty People API jsou vidět jen v konzoli projektu, veřejně dokumentované nejsou. Dokumentace uvádí jen zvláštní, nenavýšitelnou kvótu na první stránku plného syncu.

---

## 3. Architektura sync enginu

```
 webhooky ─┐
 plánovač ─┼─► fronta úloh ─► aktor účtu ─► adaptér
 zápis API ┘                                   │
                                               ▼
                                    stránka změn (surová)
                                               ▼
                                    kodek ─► pozorovaný stav položky
                                               ▼
                                    sloučení do entity (kapitola 12.4)
                                               ▼
   jedna transakce: položky + entity + umístění podle pravidel (kapitola 13.2)
                    + indexy + žurnál + kurzor + doručení + zprávy
            │                                  │
            ▼                                  ▼
   proud změn (SSE),                 outbox: doručení na umístění,
   GET /entities, /changes           která se od entity liší,
                                     a zprávy jiným tenantům (kapitola 14)
```

Stahuje aktor účtu, ale revize entit zapisuje jediný zapisovatel tenanta. Entita s umístěními ve třech účtech má tak jedno pořadí změn. Stejným zapisovatelem procházejí i zprávy z jiných tenantů a vyhodnocení pravidel, takže pravidlo vidí každou revizi právě jednou.

**Dva soubory na tenanta** (kapitola 4.3). „Jedna transakce" z diagramu je transakce v hlavním souboru tenanta. Přijetí mutace z API je jiná transakce v souboru outboxu. Zapisovatel tenanta ji potom promítne do hlavního souboru a ve stejné transakci posune vodoznak. Atomicita mezi soubory není; spojuje je vodoznak a idempotentní promítnutí.

### 3.1 Stavový automat účtu

`Připojování → Zjišťování → Úvodní sync → Živý (push | dotazování) ⇄ Omezený (backoff) → Vyžaduje přihlášení | Pozastavený | Odebraný`

Každá kolekce má vlastní podstav: kurzor, pokrytí (okno nebo celek), čas posledního úplného srovnání. Stav i poslední chyba jsou součást veřejného API, aby aplikace uměla říct „Outlook je 4 minuty pozadu" nebo „přihlaste se znovu".

### 3.2 Plánování

- **Trvalá fronta úloh** v úložišti: čas spuštění, pronájem (lease), sloučení duplicit. Žádný `setInterval` nad všemi uživateli.
- **Priority:** uživatel se dívá nebo právě zapsal; přišel push; plánované dotazování; dosyp historie; pojistné srovnání.
- **Omezovače** podle klíče (aplikace u poskytovatele, účet, hostitel), respektují `Retry-After`. Souběžnost na schránku podle poskytovatele.
- **Push je spouštěč, ne zdroj dat.** Webhook jen označí kolekci jako špinavou. Obsah oznámení se nikdy nepoužije, vždy následuje delta. Subscriptions obnovuje plánovaná úloha zhruba ve dvou třetinách životnosti. Watch kanál Googlu se obnovuje podle vrácené `expiration`, ne podle předpokládané životnosti.
- **Adaptivní dotazování** tam, kde push není (iCloud, obecný CalDAV, Google Tasks): desítky sekund, když má uživatel otevřeného klienta; minuty v klidu; hodiny u spících účtů. Laciná kontrola (ctag, sync token) před drahým čtením.

**Proč push není volitelný.** 20 000 účtů se třemi kalendáři dotazovaných každých 5 minut je 12 000 požadavků za minutu, tedy víc než výchozí limit Googlu 10 000 za minutu na projekt. Dotazování jako hlavní mechanismus má strop v jednotkách tisíc uživatelů na projekt.

### 3.3 Čtení: proudové, po stránkách, atomické

1. Adaptér vrací **proud stránek** `{ změněné objekty se surovým obsahem a validátorem, smazané, další kurzor, pokrytí }`.
2. Kodek převede surový objekt na kanonický. Je to čistá funkce bez I/O. Výsledek je pozorovaný stav položky.
3. Engine zapíše stránku **v jedné transakci**: surová vrstva a pozorovaný stav, sloučení do entit s více umístěními a jejich doručení (kapitola 12.4), indexy, záznamy do žurnálu, nový kurzor.

Z toho plyne odolnost proti pádu: buď je stránka celá i s kurzorem, nebo není vůbec. Opakování je bezpečné, protože zápis je upsert podle vzdáleného klíče a nezměněný validátor je no-op.

**Rychlý první dojem.** Úvodní sync začne omezeným dotazem na okno kolem dneška (Google: časový filtr bez tokenu; CalDAV: `calendar-query` s rozsahem; Graph: `calendarView`). Uživatel vidí aktuální týdny během sekund. Úplný sync, který vrací kurzor, běží hned potom na pozadí.

**Bezpečnost mazání** (všechno jsou lekce z Musubi):

- Nepřítomnost v neúplném čtení není smazání. Mazat podle absence smí jen čtení s úplným pokrytím.
- Okno Graphu: `@removed` ani vypadnutí z okna nedokazuje smazání.
- Pojistka hromadného mazání: pokud by reset smazal velkou část kolekce, engine změnu pozdrží a ověří druhým čtením.
- Chybějící volitelné oprávnění (například k úkolům) znamená „neúplné zjišťování", ne „seznamy úkolů zmizely".
- Smazání položky, která je jedním z více umístění entity, se na ostatní umístění ve výchozím stavu nepropisuje. S politikou `propagate` se propisuje jen na výslovný důkaz (kapitola 12.7).

### 3.4 Žurnál změn

- Monotónní pořadové číslo `seq` na tenanta, revize `rev` na entitu. Každá revize nese původ: mutaci aplikace, nebo umístění, ze kterého změna přišla.
- Žurnál drží **poslední změnu každé entity a každého umístění**, ne celou historii (stejně jako JMAP a CouchDB). Velikost je omezená počtem položek.
- Náhrobky po smazání se drží omezenou dobu. Klient se starším kurzorem dostane odpověď „nutný nový úvodní sync".
- Čtenáři žurnálu: klienti, odchozí webhooky, případné projekce. Každý má vlastní kurzor.

**Zamítnuto: plný event sourcing a externí log (Kafka, NATS).** Pro většinu dat je zdrojem pravdy poskytovatel a data umíme stáhnout znovu. Autorská data entit s více umístěními chrání zálohy (kapitola 4.4), ne historie událostí. Úplná historie by stála úložiště i infrastrukturu a první verze ji nepotřebuje. Zda aplikace chtějí auditní historii, je otevřené rozhodnutí.

### 3.5 Zápisy: mutace, doručení a konflikty

1. API přijme mutaci **entity** a ověří ji proti schopnostem jejích umístění. Zápis, který neunese hlavní nebo jediné umístění, odmítne **před přijetím** typovanou chybou. Co neunese kopie, vrátí jako seznam přibližných polí (kapitola 12.8). Každému účastníkovi přiřadí kanál pozvánky (kapitola 14.6).
2. **Přijetí** je jedna transakce v souboru outboxu tenanta s plným fsync: neměnná mutace (druh operace, výchozí revize, kanonická záplata, idempotency klíč), **doručení pro každé umístění** (u vytváření i s předem přiděleným vzdáleným ID) a zprávy pro místní účastníky. Kontrola výchozí revize zahrnuje i nepromítnuté mutace téže entity. Tím je mutace přijatá.
3. **Promítnutí** dělá zapisovatel tenanta hned potom, v transakci hlavního souboru: nová revize entity, záznam do žurnálu, umístění, která nově vyžadují pravidla, a vodoznak. Je idempotentní a po pádu se přehraje od vodoznaku. Doručení, která z promítnutí plynou (nová umístění podle pravidel), se zapíšou do outboxu idempotentně podle klíče (mutace, umístění). Klienti změnu vidí se stavem „čeká" po promítnutí. Kdo potřebuje vidět vlastní zápis hned, dostane od API i nepromítnuté mutace.
4. Pracovník outboxu se probudí hned po commitu, ne na časovač. Umístění obsluhuje souběžně, každé z nich sériově. Adaptér pošle **podmíněný zápis** a jako základ vezme poslední surový stav, do kterého promítne jen změněná pole. Podrobnosti k latenci zápisu jsou v kapitole 5.5.
5. Výsledky, pro každé umístění zvlášť:
   - **Přijato:** uloží se vrácený surový stav a validátor. Ozvěna v příští deltě je díky shodnému validátoru no-op.
   - **Konflikt (412):** načíst aktuální stav a sloučit třícestně po polích (základ, vzdálená změna, naše změna). Nepřekrývající se pole se sloučí a zápis se zopakuje. U překrývajících se rozhoduje politika: výchozí je „vyhrává příchozí změna a aplikace dostane záznam o konfliktu s poraženou hodnotou".
   - **Odmítnuto:** trvalá chyba s důvodem. Odmítne-li hlavní nebo jediné umístění, mutace se vrátí. Odmítne-li kopie, entita platí dál (kapitola 12.5).
   - **Nejisté** (timeout po odeslání): žádné slepé opakování. Nejdřív pozorování: čtení podle předem přiděleného ID (Google), deterministické URL (CalDAV) nebo obsahu a značky (Graph; `transactionId` je jen pomocník s nedokumentovaným oknem).
6. Stav doručení (`čeká`, `synchronizováno`, `konflikt`, `selhalo`, `nejisté`) je v API u každého umístění a jako souhrn u entity.

U entity s jedním umístěním je to stejný postup jako v revizi 2: jedna mutace, jeden podmíněný zápis. Změna je do potvrzení optimistická vrstva nad pozorovaným stavem položky. Rozeslání na více umístění, ochranu proti smyčkám a pravidla pro schůzky popisuje kapitola 12.

„Uloženo" a „pozvánky doručeny" jsou dvě různá tvrzení. Engine tvrdí jen to první. Výjimka je místní účastník: u něj engine zná i stav doručení do jeho schránky (kapitola 14.4).

### 3.6 Pozorovatelnost

- `tracing` a OpenTelemetry: jeden trace na úlohu, span na HTTP požadavek, automatická redakce tajemství.
- Strukturovaná chyba místo textu: fáze, HTTP stav, `retry_after`, jistota výsledku, třída (přechodná, autorizace, kvóta, trvalá, protokol). Musubi dnes stav parsuje z textu regulárním výrazem (`apps/api/src/sync/errors.ts`, podle auditu).
- Metriky na účet a koncový bod: zpoždění za poskytovatelem, délka fronty, čerpání kvóty, počet resetů kurzoru.
- Inspektor: CLI a endpoint, které pro položku ukážou surový stav, kanonický stav, verzi mapování a poslední mutace.

---

## 4. Úložiště

### 4.1 Přístupové vzory

| Vzor | Kde | Nárok |
| --- | --- | --- |
| Upsert podle (účet, vzdálený klíč) | příjem změn, dosyp | vysoká propustnost zápisu, dávky |
| Rozsah v čase přes vybrané kolekce | hlavní čtecí cesta | jednotky milisekund |
| Změny od `seq` | proud změn | jednotky milisekund |
| Fronta úloh a outbox | plánovač, doručení na umístění, zprávy mezi tenanty | malé objemy, spolehlivost |
| Entity s více umístěními, vazby, pravidla, mutace, konflikty, plánovací zprávy | propojování, rozeslání, pravidla a pozvánky | malý objem, **autorská data**: plná trvanlivost a zálohy |
| Surová těla | ladění, zápis zpět, později pošta | velká data, málo čtení |
| Fulltext | později pošta | per tenant |

Dvě podstatné vlastnosti zátěže:

- **Mnoho tenantů, každý malý až střední, žádné dotazy napříč tenanty.** Tenant je i hranice propojování, takže žádná transakce nepřekročí jednoho tenanta. Platí to i pro pozvánky mezi tenanty: jsou to dvě oddělené transakce spojené zprávou (kapitola 14.8). Jediná data čtená napříč tenanty je adresář plánovacích adres v řídicí rovině. Uživatel má řádově tisíce až desítky tisíc událostí, u pošty statisíce zpráv.
- **Dvě třídy dat.** Stažená data jdou stáhnout znovu. Autorská data (kapitola 12.12) po ztrátě nikdo nevrátí. Objemem jsou malá, nároky na trvanlivost mají nejvyšší.

### 4.2 Srovnání

| Varianta | Pro | Proti | Verdikt |
| --- | --- | --- | --- |
| **SQLite, soubor na tenanta** | čtení i zápis v procesu bez síťového skoku; self-host bez externí databáze; izolace souborem; export, záloha a smazání tenanta jsou práce se souborem; FTS5; jediný zapisovatel na tenanta řadí všechny změny jedné entity za sebe bez zámků | zálohování, replikace a vysoká dostupnost jsou naše práce a u autorských dat už nejsou volitelné; mnoho souborů; migrace schématu přes N souborů; tenant sedí na jednom uzlu, takže je potřeba směrování | **zvoleno** (11. 11. 2026): fáze 1, self-host i první hostovaná instance bez SLA |
| **PostgreSQL, sdílené** | hotová replikace, obnova k bodu v čase a spravované služby; provozně známé; dotazy napříč tenanty; RLS | síťový skok na každý dotaz i zápis; slučování entit potřebuje zámek na entitu nebo tenanta; bloat při masivních upsertech; hluční sousedé; self-hoster musí provozovat další službu | **pozdější druhý backend** za úzkým rozhraním, pro hostovanou instanci se SLA |
| **FoundationDB** | škáluje, striktní serializovatelnost | těžký provoz, nevhodné pro self-host, indexy i dotazovací vrstvu bychom psali sami | zamítnuto pro první verze |
| **Vestavěné KV (RocksDB, fjall, redb)** | nejrychlejší surový zápis | vlastní indexy, vlastní dotazy, vlastní fulltext; Stalwart touto cestou šel a stála roky | zamítnuto jako primární |
| **Log událostí + projekce** | přehratelnost | další infrastruktura; přínos pokrývá žurnál a surová vrstva | převzat vzor, ne infrastruktura |

### 4.3 Rozhodnutí a jeho podmínky

Revize 2 opírala volbu SQLite o větu, že primitivum je „cache s úmysly". S entitami na více místech to přestalo platit. Poctivé převážení:

| Hledisko | SQLite na tenanta | PostgreSQL |
| --- | --- | --- |
| Zálohy a trvanlivost autorských dat | vlastní práce: průběžný odvoz WAL, snímky, obnova a její testy | hotové: obnova k bodu v čase, synchronní replikace |
| Vysoká dostupnost | není; obnova tenanta na jiném uzlu trvá minuty | hotová |
| Správnost slučování | jediný zapisovatel na tenanta; souběh dvou změn téže entity nemůže nastat | zámky a serializovatelné transakce; řešitelné, ale je to další zdroj chyb |
| Rychlost | volání funkce | síťový skok, na stejném stroji desetiny milisekundy (odhad) |
| Self-host | jeden binární soubor, záloha je kopie souboru | další služba k provozu |

Fáze 0 to změřila ([`docs/research/phase0/storage.md`](../research/phase0/storage.md)). Výkon nerozhoduje: čtení je u SQLite rychlejší (rozsah p99 1,0 ms proti 3,4 ms u PG, pod zápisem 3,7 proti 11,2 ms), přijetí mutace vychází podobně a ani jeden backend nesplní 10 ms během importu téhož tenanta. Rozhoduje, kdo ponese odpovědnost za zálohy a dostupnost.

**Rozhodnutí uživatele (11. 11. 2026, třetí kolo):** fáze 1 i self-host stojí na **SQLite, sadě souborů na tenanta**. Platí to i pro první hostovanou instanci bez smluvního SLA. PostgreSQL zůstává za úzkým rozhraním úložiště jako pozdější práce, až vznikne hostovaná instance se SLA. Port je podle spiku přímočarý (stejné schéma s `tenant_id`).

Rozhraní úložiště zůstává úzké (řádově 20 operací, návrh v [`docs/plan/phase1.md`](../plan/phase1.md)). Logika slučování se píše proti rozhraní, ne proti SQLite.

**Soubory tenanta:**

| Soubor | Obsah | Trvanlivost |
| --- | --- | --- |
| hlavní (`main.db`) | položky, surová vrstva, entity, umístění, indexy, žurnál, kurzory, vodoznak promítnutí `meta.projected` | stránky příjmu `synchronous=NORMAL`; promítnutí mutací a jiná autorská data `FULL` |
| outbox (`outbox.db`) | mutace, doručení, odchozí zprávy | `synchronous=FULL`, vlastní zámek zapisovatele a vlastní WAL |

Řídicí rovina (tenanti, klíče API, účty a šifrované tokeny, fronta úloh, adresář plánovacích adres) je samostatná databáze `control.db`.

**Podmínky, bez kterých SQLite vychází hůř než PostgreSQL** (storage.md, kapitola 6). Jsou to požadavky na fázi 1:

1. **Outbox v samostatném souboru.** Bez něj čeká přijetí během importu téhož tenanta na stránku a reset WAL: p99 205–373 ms místo 7,7–16,1 ms.
2. **Zdrojem pravdy pro mutace je outbox.** Mutace je přijatá, jakmile je commitnutá tam. Promítnutí do hlavního souboru je idempotentní a vodoznak se posouvá ve stejné transakci. Po pádu se přehrají mutace nad vodoznakem.
3. **Žádná atomicita přes soubory.** `ATTACH` s transakcí přes oba soubory ji v režimu WAL nedává. Kontrola výchozí revize při přijetí zahrnuje i nepromítnuté mutace a čte entitu a umístění v jedné čtecí transakci.
4. **Id mutací se nikdy znovu nepoužijí:** `AUTOINCREMENT`, nebo se řádky nad vodoznakem nikdy nemažou. Jinak replay přeskočí mutaci se znovu použitým id.
5. **Vlastní checkpointer:** `PASSIVE` na pozadí; `TRUNCATE` v zapisovači po stránce příjmu nad prahem (výchozí 64 MB) s čekáním na čtenáře nejvýš 10 ms a back-offem 1 s nebo +32 MB; `journal_size_limit` a vysoký `wal_autocheckpoint` jako pojistka. Přijetí mutace reset nikdy nevolá. Bez checkpointeru narostl WAL na 1,8–3,1 GB, s ním nejvýš na 51–184 MB.
6. **Omezená délka čtecích transakcí.** Dlouhá čtení (export, záloha) jdou přes backup API nebo `VACUUM INTO`. Nepřetržitý dlouhý čtenář nechal WAL narůst na 3–7 GB.
7. **Build s `SQLITE_DEFAULT_MEMSTATUS=0`** (4× propustnost souběžného čtení), `busy_timeout` jako první pragma, zápisy `BEGIN IMMEDIATE`, čtecí a zapisovací spojení zvlášť.
8. **Měřit:** největší WAL, délku `TRUNCATE` a počet neúspěšných resetů na tenanta.

Podoba datové roviny dál: režim WAL, jeden zapisovatel na tenanta, LRU cache otevřených souborů (otevření pod 1 ms), líná migrace schématu při otevření (p99 ~11 ms na první požadavek, ~21 s sekvenčně pro 10 000 tenantů). Velká těla později v úložišti objektů podle hashe. Škálování do šířky směrováním tenanta na uzel.

**Otevřené:** ext4/XFS a serverový disk s PLP fáze 0 neměřila (běželo se na btrfs nodatacow a spotřebitelském NVMe), stejně jako průběžný odvoz WAL a data větší než RAM. Group commit nebo sdílený accept log jsou jen návrh pro případ, že přijetí pod souběhem nebude stačit.

**Velikost:** naměřeno ~4,4 KB na událost bez komprese, tedy ~2,2 TB pro 100 000 uživatelů × 5 000 událostí. Se zstd po řádcích v surové vrstvě 2,3 KB, se slovníkem na tenanta 1,9 KB (syntetická data). Zda komprimovat, se rozhodne ve fázi 1 podle reálných payloadů. Tabulky s těly kolem 1 KB nepoužívají `WITHOUT ROWID`. Kalendářová data nejsou objemový problém. Pošta bude.

### 4.4 Trvanlivost a zálohy

- **Dvě úrovně potvrzení.** Transakce, která se dotkne entity s více umístěními, umístění, mutace nebo tokenů, se potvrdí až po fsync. Stránky stažených dat triviálních entit se dál zapisují bez čekání na fsync.
- **Průběžná záloha.** U SQLite odvoz WAL do úložiště objektů nebo na druhý disk s cílem ztratit nejvýš jednotky sekund, k tomu denní snímek. Odvoz WAL musí spolupracovat s naším checkpointerem a fáze 0 ho neměřila **(neověřeno)**; ve fázi 1 jsou jen snímky. Self-hosterovi bez úložiště objektů stačí snímek do adresáře.
- **Pořadí zálohy:** backup API vezme nejdřív hlavní soubor, potom outbox. Outbox tak nikdy není starší než vodoznak v hlavním souboru a rozdíl po obnově dorovná replay. Záloha celého uzlu trvala 2–3 s na 5,5 GB (v klidu).
- **Obnova je příkaz, ne návod:** `musubi-syncd restore`. Test obnovy běží v CI.
- **Export tenanta** v dokumentovaném formátu, aby provozovatel nebyl na projektu závislý.
- **Značky u poskytovatelů** (kapitola 12.10) dovolí vazby obnovit i bez zálohy tam, kde je poskytovatel unese.
- **Kopie vytvořené jednosměrnými pravidly** jsou odvozená data: po ztrátě je obnoví nové vyhodnocení pravidla (kapitola 13.4). Pravidla sama jsou autorská data.
- **Plánovací zprávy** se po obnově dorovnají zprávou REFRESH (kapitola 14.8).

**Obnova ze starší zálohy se z velké části dorovná sama.** Entita a snímky umístění v záloze pocházejí z téže transakce. Po obnově proto engine uvidí všechno, co se mezitím u poskytovatelů změnilo, jako příchozí změny a entitu dožene.

Nenávratně se ztratí jen:

- vazby vytvořené po záloze (položky se objeví jako samostatné záznamy; pomohou značky);
- data entity, která neunese žádné umístění;
- mutace přijaté, ale ještě nedoručené;
- pravidla založená nebo změněná po záloze a odpovědi na pozvánky, které ještě neodešly.

---

## 5. Rychlost: operace pod sekundu a optimalizace sync a IO

Podle rozhodnutí uživatele není cílem lokální replika v klientovi, ale to, aby každá operace primitiva skončila hluboko pod 1 s. Tato kapitola proto popisuje rozpočet latence a konkrétní techniky. Replika na zařízení je volitelná budoucí práce (kapitola 5.8).

### 5.1 Rozpočet latence

| Operace | Cíl | Co ji omezuje |
| --- | --- | --- |
| Dotaz na rozsah, položku nebo změny od kurzoru | p99 pod 10 ms na serveru u běžného tenanta; rozsah je stránkovaný a má variantu jen s id | lokální index, žádný poskytovatel v cestě; hustý rozsah (11 761 výskytů) trvá s payloadem 28 ms, jen s id 6,8 ms |
| Přijetí zápisu (trvale v souboru outboxu) | p99 pod 10 ms mimo import téhož tenanta; **p99 pod 20 ms během importu téhož tenanta** | jeden commit s fsync v souboru outboxu; měřeno 7,7–16,1 ms během importu, 1,7 ms s jedním zapisovatelem, 2,9–12,2 ms při 16 tenantech naráz |
| Viditelnost přijatého zápisu v API a v proudu změn | po promítnutí do hlavního souboru, typicky hned; během importu téhož tenanta až po rozepsané stránce **(neměřeno)** | zapisovatel tenanta |
| Potvrzení zápisu poskytovatelem | medián pod 400 ms, p95 pod 1 s | jeden požadavek na teplém spojení; odezva poskytovatele |
| Změna u poskytovatele → proud změn, s pushem | pod 500 ms od přijetí oznámení | jeden delta požadavek a commit |
| Změna na jednom umístění → zapsaná na ostatních umístěních | medián pod 1 s od přijetí oznámení, má-li zdrojové umístění push | delta, sloučení do entity, jeden podmíněný zápis na každé další umístění, souběžně |
| Totéž bez pushe (iCloud, obecný CalDAV, Google Tasks, People API) | interval dotazování: 15–30 s při aktivním zájmu aplikace, minuty jinak | protokol poskytovatele |
| Inkrementální sync bez změn | jeden požadavek, pod 300 ms | odezva poskytovatele |
| První data po připojení účtu | pod 1 s pro okno kolem dneška | jeden až dva požadavky |
| Úplný úvodní sync | měří se propustnost: tisíce položek za sekundu na účet | velikost stránky a kvóty poskytovatele |

Tři poctivá omezení:

- **Čísla závislá na poskytovateli jsou odhady.** Fáze 0 odezvu poskytovatelů neměřila, protože neměla živé účty. Změří ji živé kontroly na testovacích účtech, pro Google na konci fáze 1. Cíle se podle nich upraví.
- **Cíl přijetí během importu téhož tenanta je uvolněný na p99 pod 20 ms.** Na sdíleném disku bez PLP nesplní 10 ms ani SQLite, ani PostgreSQL (~11–37 ms). S dlouhým čtenářem stoupne chvost SQLite na 54–83 ms, proto jsou čtecí transakce omezené (kapitola 4.3). Na ext4/XFS a disku s PLP se neměřilo **(neověřeno)**.
- **Dvě věci pod 1 s dostat nejde.** Zjištění cizí změny u poskytovatele bez pushe je omezené intervalem dotazování. A doba, za kterou poskytovatel oznámení pošle, je mimo naši kontrolu: Graph pro události latenci neuvádí vůbec.

Vlastní režie jádra nad časem stráveným u poskytovatele má být u inkrementálního syncu pod 20 ms.

### 5.2 Síť

- **Teplá spojení.** Jeden sdílený HTTP klient na hostitele poskytovatele, HTTP/2 s multiplexováním, keep-alive, obnovení TLS relace, cache DNS. Zápis ani delta neplatí za navázání spojení.
- **Největší povolená stránka.** Google events dovoluje až 2 500 položek na stránku (výchozí je 250; stránka může být prázdná a mít `nextPageToken`), People API 1 000, Google Tasks 100. Graph řídí hlavička `Prefer: odata.maxpagesize`; strop není dokumentovaný (T3), takže adaptér velikost stránky nepředpokládá. Musubi dnes používá 100 u Googlu i Graphu.
  Příklad: úvodní sync kalendáře s 10 000 událostmi je při stránce 100 položek 100 po sobě jdoucích požadavků, při stránce 2 500 čtyři. Při odezvě kolem 300 ms na požadavek je to rozdíl mezi půl minutou a několika sekundami (odhad).
- **Jen potřebná pole.** Google `fields`, Graph `$select` tam, kde ho delta dovoluje, u CalDAV jen ETag a data.
- **Komprese** odpovědí (gzip).
- **Co nejméně kol.** Nejdřív laciná kontrola (sync token, ctag), drahé čtení až při změně. U CalDAV `sync-collection` a hned `multiget` po dávkách; zda server vrátí data už v odpovědi na `sync-collection`, patří do profilu serveru.
- **Dávky.** Graph `$batch` (nejvýš 20 požadavků) pro dotažení hlavních událostí sérií a pro hromadné zápisy. Dávkový endpoint Googlu nese nejvýš 1 000 volání, ale do kvóty se počítá jako N požadavků, takže šetří spojení, ne kvótu. People API `batch*Contacts` nejvýš 200 kontaktů.
- **Obnova tokenu předem.** Token se obnovuje před vypršením a jen jednou na účet, takže žádný požadavek nečeká na obnovu ani neopakuje po `401`.
- **Cache zjišťování.** Principal, home set a oddílový hostitel iCloudu se ukládají a zneplatní až při přesměrování nebo `404`. Musubi podle vlastního auditu zjišťování opakuje.

### 5.3 Souběžnost v mezích limitů

- **Mezi účty** plně paralelně. Nad tím společný rozpočet na aplikaci u poskytovatele (Google 10 000 požadavků za minutu na projekt).
- **Uvnitř účtu** kolekce paralelně až do limitu poskytovatele: Graph 4 souběžné požadavky na schránku včetně požadavků uvnitř `$batch`, Google 600 požadavků za minutu na uživatele, People API zápisy po jednom, iCloud 2 spojení, obecný CalDAV 2–4 podle profilu serveru.
- **Adaptivní řízení.** Souběžnost roste, dokud nepřijde `429` nebo `503`, potom klesne a respektuje `Retry-After`.
- **Stránky jedné kolekce jdou po sobě**, protože je řetězí kurzor. Zpracování je ale proudové: další stránka se stahuje, zatímco se předchozí dekóduje a zapisuje.
- **Dva pruhy s vyhrazenou kvótou.** Interaktivní práce (zápisy, okno viditelné uživateli, reakce na push) má vyhrazený podíl limitu. Dosyp historie a pojistná srovnání ji nesmí vyhladovět.
- **Dekódování mimo IO vlákna.** Parsování, rozbalení opakování a komprese běží v poolu pro CPU.

### 5.4 Inkrement a push

- Obslužný kód webhooku jen ověří oznámení, trvale ho zařadí do fronty a odpoví. Graph vyžaduje odpověď do 3 s; cíl jsou milisekundy.
- Série oznámení ke stejné kolekci se slučuje v okně 100–200 ms do jednoho delta požadavku.
- Nezměněný validátor znamená žádné dekódování a žádný zápis. Stejný hash surového obsahu také.
- Ozvěna vlastního zápisu je no-op, protože validátor už známe z odpovědi na zápis.
- Proud změn se klientům rozesílá z paměti hned po commitu. Databáze se kvůli němu neobchází dotazy.

### 5.5 Cesta zápisu k poskytovateli

- **Okamžité odeslání.** Pracovníka outboxu budí commit, ne časovač. Časovač slouží jen k obnově po pádu. Musubi dnes obnovuje po 15 sekundách.
- **Žádné čtení před zápisem.** Kontrolu dělá podmíněný zápis s posledním známým validátorem. Čtení, sloučení a opakování nastane jen při konfliktu `412`, což je vzácná cesta.
- **Jeden požadavek na mutaci.** Novým surovým stavem je odpověď na zápis: Google i Graph vracejí celý objekt. U CalDAV stačí ETag z odpovědi na PUT, pokud ho server vrací; jinak jedno dotažení podle profilu serveru.
- **Paralelně mezi položkami, sériově na položku.** Jednotkou pořadí je vzdálený zdroj (u DAV celá série), ne účet. Umístění téže entity u různých poskytovatelů se zapisují souběžně.
- **Sloučení čekajících změn.** Doručení je stavové: několik ještě neodeslaných změn téže entity (například při tažení myší) se odešle jako jeden zápis na každé umístění.
- **Volba pro aplikaci:** `POST /v1/mutations` vrací hned po trvalém přijetí, nebo s parametrem `wait=primary` či `wait=all` počká na výsledek poskytovatelů do zadaného limitu.

### 5.6 Cesta zápisu do databáze

- **Jedna transakce na stránku** (stovky až tisíce řádků) v hlavním souboru, připravené dotazy, víceřádkové upserty. Mutace jdou do samostatného souboru outboxu (kapitola 4.3), takže přijetí nečeká na stránku příjmu.
- **Jeden zapisovatel na soubor** (aktor tenanta pro hlavní soubor, přijetí mutací pro outbox), takže žádné soupeření o zámky. Čtenáři běží souběžně díky WAL a jejich transakce mají omezenou délku.
- **Všechno drahé před transakcí:** dekódování, index výskytů, hashe a komprese se spočítají předem. Transakce jen vkládá.
- **Dvě úrovně trvanlivosti.** Stažená data se zapisují bez čekání na fsync každé transakce: jdou stáhnout znovu a kurzor je ve stejné transakci, takže po výpadku zůstane stav konzistentní. Mutace, tokeny a všechno, co se týká entit s více umístěními, se zapisuje s plným fsync (kapitola 4.4).
- **Málo indexů.** Každý index zpomaluje zápis. Nutných je pět: vzdálený klíč, čas výskytu, `seq`, vazba položka–entita a u kontaktů e-mail s telefonem.
- **Surová těla** adresovaná hashem: existující hash znamená žádný zápis.

### 5.7 Měření jako součást produktu

- V repozitáři je sada benchmarků: falešné servery poskytovatelů s nastavitelnou odezvou a velikostí stránek, mikrobenchmarky kodeků a rekurence.
- Latenční cíle z kapitoly 5.1 jsou testy v CI s rozpočtem na regresi.
- Každý sync hlásí počet požadavků, přenesené bajty a rozdělení času na poskytovatele a vlastní režii. Bez toho se nedá poznat, co zrychlovat.

### 5.8 Lokální replika v klientovi (volitelná budoucí práce)

V plánu není. API je navržené tak, aby ji šlo přidat bez změny jádra: proud změn s kurzorem a idempotentní mutace s výchozí revizí jsou přesně to, co replika potřebuje. Pokud vznikne, platí dvě už zvážená rozhodnutí:

- **Ne CRDT.** Konečnou autoritou je cizí poskytovatel, konflikt se stejně řeší podmíněným zápisem a slučováním na serveru.
- **Ne hotový sync engine** (Zero, PowerSync, Electric). Jsou postavené kolem replikace z PostgreSQL a vlastního modelu oprávnění. Převzít model (server je autorita, klient přeskládává čekající změny), ne závislost.

---

## 6. Kontrakt adaptéru poskytovatele

### 6.1 Dvě rozhraní místo jednoho

**Transport** řeší síť, kurzory a push. **Kodek** je čistá funkce bez I/O. Musubi obojí míchá a jeho adaptér navíc importuje typy databáze, takže ho nejde použít bez Musubi schématu.

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;

    /// Kolekce, jejich schopnosti a příznaky úplnosti zjišťování.
    async fn discover(&self, cx: &AccountCx) -> Result<Discovery, SyncError>;

    /// Proud stránek změn od kurzoru. `None` znamená úplné čtení.
    fn changes<'a>(&'a self, cx: &'a AccountCx, col: &'a CollectionRef,
                   cursor: Option<Cursor>, hint: FetchHint)
        -> BoxStream<'a, Result<ChangePage, SyncError>>;

    /// Načtení konkrétních objektů (po pushi, pro ověření, pro série).
    async fn fetch(&self, cx: &AccountCx, refs: &[RemoteRef])
        -> Result<Vec<RemoteObject>, SyncError>;

    /// Podmíněný zápis na jedno umístění.
    async fn apply(&self, cx: &AccountCx, d: &Delivery)
        -> Result<ApplyOutcome, SyncError>;   // Applied | Conflict | Rejected | Uncertain

    /// Pozorování po nejistém výsledku. Nesmí zapisovat.
    async fn locate(&self, cx: &AccountCx, d: &Delivery)
        -> Result<Option<RemoteObject>, SyncError>;

    /// Volitelný push: založení, obnova, zrušení, ověření příchozího oznámení.
    fn push(&self) -> Option<&dyn PushSource> { None }
}

pub trait Codec<K: Kind> {
    const MAPPING_VERSION: u32;
    fn decode(raw: &Raw) -> Decoded<K::Canonical>;
    /// Promítne kanonickou záplatu do posledního surového stavu.
    fn encode(base: Option<&Raw>, patch: &K::Patch, caps: &Capabilities)
        -> Result<RawWrite, Unsupported>;
    /// Co z entity tento poskytovatel unese a s jakou věrností.
    fn project(entity: &K::Canonical, role: Role, caps: &Capabilities) -> Projection<K>;
    /// Přenese změnu pozorovanou u poskytovatele do záplaty entity.
    fn lift(entity: &K::Canonical, before: &K::Canonical, after: &K::Canonical,
            caps: &Capabilities) -> K::Patch;
}
```

Kontrakt je záměrně malý: žádné metody jako `writeMicrosoftRsvp` nebo `readCaldavSeries`. RSVP, úprava jednoho výskytu nebo zrušení schůzky jsou **druhy mutací**, které adaptér buď umí, nebo je podle schopností odmítne.

`project` a `lift` jsou jediné, co adaptér ví o více umístěních. Slučování, rozeslání, role umístění a ochranu proti smyčkám dělá engine a pro všechny poskytovatele jsou stejné (kapitola 12).

### 6.2 Schopnosti

Strukturovaná data na účet a kolekci, vystavená v API, aby aplikace uměla funkci skrýt dřív, než ji uživatel zkusí:

- druhy položek a čtení/zápis;
- delta: `SyncToken | WindowedDelta | CtagEtag | None`;
- push: `Webhook | None`;
- podmíněný zápis zvlášť pro úpravu a pro smazání a pro každý druh položky (`conditional_update`, `conditional_delete`: `StrongValidator | Unproven | None`). Fáze 0 ukázala, že se liší: Graph DELETE je nepodmíněný a To Do nemá podmíněný zápis vůbec;
- vytvoření s klientským ID: ano/ne;
- opakování: `Full | Finite | ReadOnly | None`;
- zóny: `Iana | WindowsIds | CustomVtimezone`;
- plánování: `ProviderSide | CoreInbox | None`; potlačení pozvánek při zápisu: `žádné | celý požadavek | po účastnících`; zápis odpovědi účastníka organizátorem: ano/ne;
- věrnost každého sdíleného pole při umístění: `přesně | přibližně | chybí` (kapitola 12.8);
- skryté pole pro značku entity: ano/ne;
- připomenutí, úkoly (termín jako datum nebo čas, podúkoly, opakování) a limity.

Čtení má tři možné výsledky: věrné čtení, věrný zápis pro daný profil a „čitelné, ale nezapisovatelné". Jednotnost znamená předvídatelný výsledek, ne předstírání stejných schopností.

### 6.3 Konformační sada

1. **Zlaté fixtures kodeků:** surový vstup → kanonický tvar → zápis zpět. Property testy „neznámé pole přežije úpravu názvu".
2. **Sada pro transport proti falešnému serveru** daného protokolu: úvodní sync, stránkování, inkrement, vypršený kurzor, pád uprostřed stránky, mazání, pojistka hromadného mazání, konflikt při zápisu, nejisté vytvoření, `429` s `Retry-After`, obnova tokenu a `invalid_grant`.
3. **Sada pro více umístění:** dva falešné servery s různou normalizací a různými schopnostmi. Kontroluje invarianty z kapitoly 12.8, souběžné úpravy, smazání na jednom umístění, pád uprostřed rozeslání, žádnou smyčku a žádnou druhou pozvánku.
4. **Sada pro pravidla:** A → B spolu s B → A, řetěz pravidel, dvě pravidla se stejným cílem, maska bez úniku textu, dosyp a jeho přerušení, změna a odebrání pravidla, smazání ve zdroji i v cíli.
5. **Sada pro plánování mezi tenanty:** dva tenanty a falešný poskytovatel s plánováním. Kontroluje, že pozvaný vidí jednu pozvánku při doručení jednou i dvěma cestami v obou pořadích, úplný seznam účastníků u poskytovatele, jednu cestu odpovědi, zařazení známého a neznámého odesílatele, smazání schůzky, opakované a přeházené zprávy, cizího odesílatele a obnovu ze zálohy.
6. **Sada pro federaci** (až s fází 10): falešný partnerský server a scénáře z kapitoly 15.2.
7. **Živá certifikace** proti testovacím účtům, volitelná a noční. Jejím výstupem je automaticky generovaná matice schopností.

Scénáře jsou jazykově neutrální JSON, takže stávající scénáře Musubi jde převzít jako vstup.

### Zamítnuté alternativy

- **Pluginy za běhu (WASM komponenty, dynamické knihovny).** Zatím zbytečné. Adaptér je crate za traitem; kdo chce vlastní, zkompiluje vlastní binárku.
- **Generované SDK poskytovatelů** (`google-calendar3`, `graph-rs-sdk`). Jsou těžké a zahazují neznámá pole. Potřebujeme pár koncových bodů a surový JSON.
- **Zmrazit rozhraní po prvním adaptéru.** Google a CalDAV jsou natolik odlišné, že teprve dvojice ukáže, co je obecné.

---

## 7. Bezpečnost

- **Šifrování tajemství.** Obálkové šifrování: datový klíč na tenanta, zabalený hlavním klíčem z KMS nebo ze souboru (self-host). Tokeny a hesla šifrovaná po polích AEAD šifrou s vazbou na tenanta a účet, aby šifrový text nešel přenést do jiného řádku. Identifikátor klíče u každé hodnoty kvůli rotaci.
- **Ověřování volajících.** Jádro nemá přihlašování koncových uživatelů: žádná hesla, relace, obnova hesla, 2FA ani e-maily (rozhodnuto). Aplikace své uživatele ověřuje sama a jádru předává jen tenanta. Jádro přijímá:
  - **klíč API vázaný na tenanta** pro servery aplikací. Je náhodný, uložený jen jako hash a má rozsah (čtení, zápis, správa účtů). Dva klíče mohou platit zároveň, aby šel klíč vyměnit bez výpadku;
  - **krátkodobý token** pro klienty. Vydá ho `POST /v1/tokens` držiteli klíče. Nese tenanta, rozsah (účty, kolekce, čtení nebo zápis) a platnost (návrh: 15 minut). Podepisuje ho instance klíčem Ed25519, takže se ověří bez dotazu do databáze a bez operace se soukromým klíčem RSA (RUSTSEC-2023-0071, kapitola 8);
  - **přístupový token z vnějšího IdP** (OIDC, JWT) pro samostatný self-host. Konfigurace vyjmenuje důvěryhodné vydavatele. Jádro načte jejich klíče přes OIDC discovery (JWKS), ověří podpis, `iss`, `aud`, `exp` a `nbf`, přijme jen vyjmenované asymetrické algoritmy a dvojici (vydavatel, `sub`) přiřadí tenantovi. Načítání a cache JWKS jsou náš kód: respektuje `Cache-Control` a při neznámém `kid` obnoví klíče nejvýš jednou s omezením četnosti. Nového tenanta založí samo, jen když to konfigurace povolí;
  - **správcovský klíč instance** z konfigurace. Zakládá tenanty, vydává jejich klíče a potvrzuje plánovací adresy. Data tenantů nečte.
- **OAuth k poskytovatelům** zůstává v jádru, včetně uložení, obnovy a rotace tokenů. Dva režimy: služba vede tok sama s klientem provozovatele, nebo aplikace předá hotový grant. PKCE, minimální a přírůstkové scope podle druhů dat. Odkaz na připojení vytvoří jen ověřený volající. Parametr `state` je jednorázový, krátkodobý a vázaný na tenanta, takže zpětné volání od poskytovatele jiný průkaz nepotřebuje.
- **Obnova tokenů.** Jedna obnova na účet najednou. Microsoft refresh token rotuje, nový se musí trvale uložit dřív, než se použije přístupový. `invalid_grant` znamená stav „vyžaduje přihlášení", ne opakování.
- **Izolace tenantů.** Soubor a klíč na tenanta. Každý průkaz patří právě jednomu tenantovi.
- **Mezi tenanty jdou jen zprávy.** Tenant je jeden člověk a nikdo nečte cizí úložiště. Pozvánka je zpráva, kterou příjemce přijme vlastní transakcí. Odesílatele váže adresář adres a první REQUEST (kapitola 14.3). Zvát smí kdokoli na serveru; kde pozvánka přistane, řídí nastavení příjemce, žádosti od neznámých odesílatelů, blokování a limity (kapitola 14.9).
- **Zápis do připojených účtů.** Rozeslání zapisuje i do účtů, které uživatel připojil hlavně kvůli čtení. Umístění proto vyžaduje zapisovací scope cílového účtu a vzniká jen výslovným voláním API, nebo podle pravidla, které uživatel výslovně založil. Založení pravidla je souhlas s automatickým zápisem do cílového účtu; náhled předem ukáže rozsah a upozorní na sdílený cíl (kapitola 13.10).
- **Odchozí požadavky na cizí servery.** Obecný CalDAV znamená, že uživatel zadá URL: blokovat privátní rozsahy, připnout cíl přes přesměrování, bránit DNS rebindingu. Musubi tuto vrstvu má (`pinned_transport`, `packages/provider-caldav/src/network-origin.ts`). Stejným místem by šly i odchozí požadavky federace.
- **Plánovací adresy.** Adresa je identita. Přijímá se jen ověřená (přihlášení přes OAuth, potvrzení správcem), nikdy tvrzení serveru, který si zvolil uživatel. Adresář ukládá klíčovaný hash (kapitola 14.2).
- **Federace** je ve výchozím stavu vypnutá. Model hrozeb a povinná opatření jsou v kapitole 15. Identita serveru pro federaci jsou samostatné podpisové klíče; s průkazy volajících nemá nic společného.
- **Příchozí webhooky.** Ověřit token kanálu (Google) a `clientState` včetně validačního handshake (Graph). Obsah nepoužívat, jen spustit deltu.
- **Logy.** Tajemství v typech, které nejdou vypsat (`secrecy`, `zeroize`). Redakce na úrovni HTTP tracingu.
- **Mazání.** Odebrání účtu smaže jeho tokeny a položky v jádru. U poskytovatelů nemaže nic a entita, která má umístění i jinde, zůstává. Smazání tenanta nejdřív pošle zrušení schůzek, které organizuje, a odmítnutí těch, kam je pozvaný, odebere jeho adresy z adresáře a potom smaže soubor a klíč.
- **iCloud.** Hesla pro aplikace jsou dlouhodobá tajemství bez možnosti omezit rozsah. Zacházet s nimi jako s hesly.
- **Mimo kód:** ověření OAuth aplikace u Googlu, pro Gmail navíc každoroční bezpečnostní posouzení (restricted scopes), ověření vydavatele u Microsoftu. U open-source projektu to řeší každý provozovatel pro vlastní aplikaci; projekt dodá návod a žádá nejmenší možné scope.

**Zamítnuto jako výchozí:** šifrování celé databáze (SQLCipher) kvůli ceně a provozu.

**Koncové šifrování (E2EE) výslovně není cílem.** Synchronizace běží na serveru, takže jádro potřebuje čitelná data i tokeny poskytovatelů.

---

## 8. Rust: ekosystém a mezery

### Je Rust správná volba?

**Ano.** Platí ale dvě věci zároveň a je poctivé říct obě.

**Co jazyk nezmění:**

- Propustnost jednoho účtu omezuje poskytovatel: Google 600 požadavků za minutu na uživatele, Graph 4 souběžné požadavky na schránku.
- Latenci jednoho zápisu určuje odezva poskytovatele.
- Dnešní pomalost Musubi způsobila architektura (dotazování po pěti minutách, zápis po jedné položce, žádný push), ne TypeScript. Stejná architektura v Node.js by byla proti dnešku také řádově rychlejší.

**Co jazyk změní, až poroste počet účtů:**

1. **Všechna jádra v jednom procesu.** Tokio rozkládá úlohy na všechna jádra a ta sdílejí tytéž omezovače, spojení a cache bez kopírování. Node.js vykonává JavaScript v jednom vlákně. Práci pro CPU je nutné posílat do worker vláken, která paměť nesdílejí a data si kopírují, nebo spustit víc procesů a koordinovat je přes databázi. Musubi dnes běží jako jediný proces a vynucuje to zámkem.
2. **Paměť na aktora účtu.** Aktor je úloha se stavovým automatem v řádu kilobajtů. 100 000 připojených účtů, z nichž většina spí, vychází na stovky megabajtů. V Node.js nese každý účet objekty, uzávěry a časovače na haldě V8, jejíž úklid se s objemem živých dat prodražuje. Pro self-hostera je to rozdíl mezi malým VPS a větším strojem. Čísla jsou odhad; fáze 0 je neměřila a změří je benchmark fáze 1.
3. **Latence bez pauz garbage collectoru.** Cíl p99 pod 10 ms pro čtení nesnese zastavení celého procesu uprostřed dosypu cizího účtu. Velká halda V8 přináší pauzy v desítkách milisekund (odhad). Rust globální pauzy nemá. Go s konkurenčním GC by tento bod splnilo také.
4. **Cena parsování, opakování a slučování.** Práce pro CPU jsou: parsování ICS a vCard, rozbalení opakování na sedmiletý horizont, hash a komprese, a nově průmět a rozdíl pro každé umístění entity. V jednom vlákně tato práce blokuje ostatní tenanty, dokud se ručně nerozdělí na kousky. V Rustu běží souběžně na všech jádrech. Na položku čekám rozdíl o řád: desítky mikrosekund proti stovkám mikrosekund až milisekundám (odhad). Fáze 0 změřila parsování v `calcard` na 111–210 MB/s a rozbalení 10 000 sérií v pětiletém okně (3,7 mil. výskytů) na 2,2–3,1 s včetně formátování. Přímé srovnání s kodeky Musubi neproběhlo.
5. **Jeden statický binární soubor** bez běhového prostředí. U open-source projektu určeného k self-hostu je to polovina instalace.
6. **Ekosystém pošty a kalendářů.** Stalwart (server pošty, kalendářů a kontaktů v Rustu) vydává své knihovny samostatně.
7. **Otevřená cesta do klientů.** Kodeky a rekurenci půjde později přeložit pro mobilní platformy a WASM, pokud replika na zařízení někdy vznikne.

Příklad, kdy body 1 a 4 rozhodují: 500 uživatelů připojí účty ve stejnou hodinu (spuštění produktu, migrace Musubi). Každý dosyp brzdí jeho poskytovatel, ale všech 500 běží zároveň a jejich dekódování se sčítá. V jednom vlákně stojí ve frontě za sebou a s nimi i čtení všech ostatních uživatelů. Na osmi jádrech ne.

Protiargumenty jsou reálné: pomalejší iterace než v TypeScriptu, těžce získané znalosti jsou dnes zakódované v TS testech, menší trh vývojářů. Go by pro server stačilo; proti němu rozhodují body 6 a 7 a přesnější typový model pro slučování. TypeScript ponechat pro tenké SDK.

### Crates

| Oblast | Volba | Zralost a mezery |
| --- | --- | --- |
| Async runtime | `tokio` | zralé |
| HTTP klient | `reqwest` (hyper, rustls) | zralé; opakování a limity vlastní (`governor`) |
| HTTP server, SSE | `axum`, `tower-http` | zralé |
| OAuth k poskytovatelům | `oauth2` 5.0 | zralé; rotace a single-flight vlastní |
| Ověření tokenů z vnějšího IdP, podpis krátkodobých tokenů | `jsonwebtoken` 11 s backendem `rust_crypto` a vlastní malá cache JWKS; `openidconnect` 4.1 (MIT) jen pokud se ve fázi 8 vyplatí jeho hotové discovery | zralé. Obě knihovny táhnou `rsa` 0.9 s advisory **RUSTSEC-2023-0071** (Marvin, časový postranní kanál při operaci se soukromým klíčem RSA). Ověření podpisu se netýká a krátkodobé tokeny podepisujeme Ed25519, takže soukromý klíč RSA nepoužíváme. `cargo-deny` proto potřebuje dočasnou výjimku, dokud knihovny nepřejdou na `rsa` 0.10 (vydáno 8. 11. 2026). Backend `aws_lc_rs` zamítnut kvůli C a CMake. Zdvojené `p256`/`p384` 0.13/0.14 a `ed25519-dalek` 2/3 potřebují výjimku `multiple-versions`. Knihovna pro relace a přihlášení (`axum-login`) není potřeba: jádro uživatele nepřihlašuje |
| JSON | `serde`, `serde_json` (`RawValue` pro surová data) | zralé |
| iCalendar, vCard, JSCalendar, JSContact | `calcard` (Stalwart, Apache-2.0/MIT), pinnutý `=0.3.x` za naším adaptérem | ověřeno ve fázi 0 ([calcard.md](../research/phase0/calcard.md)): parsuje 98,9 % korpusu, AST neztrácí data, 111–210 MB/s. **Zápis přes `to_string()` ani cestou JSCalendar/JSContact → iCal/vCard nikdy:** serializace normalizuje. Pro zápis vlastní vrstva záplat surového textu. Čtecí model JSCalendar/JSContact jen s `include_ical_components(true)` a `include_vcard_parameters(true)`. Vlastní VTIMEZONE tiše nerozpozná, proto vlastní detekce přes `build_tz_resolver().resolve()`. `expand_dates` nemá okno a chybuje kolem DST, pro rozbalení se nepoužije |
| Opakování (RRULE) | převzaté (vendored) jádro `rrule` 0.14 ve vlastním modulu `musubi-recurrence` za hranou nad `jiff` | rozhodnuto ve fázi 0 ([recurrence.md](../research/phase0/recurrence.md)): v korpusu 0 chyb jádra. Jádro dostává jen civilní čas označený jako UTC a zóny nevidí. Upstream se od dubna 2025 nemění, kód udržujeme sami; seznam 12 povinných úprav je v recurrence.md. Záloha: port na jiff `ccope/rust-rrule@50db3e2` za stejnou hranou. `calcard-next` jen jako referenční engine v diferenciálním testu |
| Čas a zóny | `jiff` s aktualizovatelnou tzdb (systémová, přibalená jako záloha) | moderní a správné kolem DST. `chrono-tz` ne: má zakompilovanou tzdb 2025b, která se od 2026f liší ve 22 zónách. Řetěz řešení TZID: IANA → `X-LIC-LOCATION` → mapa Windows z CLDR → přípona za předponou Mozilly → vlastní VTIMEZONE převedená na POSIX → hlasitá chyba. Surovou definici zachovat |
| Windows ID ↔ IANA | vlastní malá tabulka z CLDR | Musubi má vendorovaný CLDR 48 |
| XML | `quick-xml` | zralé |
| WebDAV, CalDAV klient | **vlastní tenký klient** | `libdav` (pimsync) existuje a je aktivní, ale má copyleft licenci EUPL-1.2, která se s jádrem pod licencí MIT neslučuje. Vlastní klient pro CalDAV i CardDAV jsou nižší tisíce řádků |
| Zjišťování služby | `hickory-resolver` | zralé |
| SQLite | `rusqlite` s přibaleným SQLite, build s `SQLITE_DEFAULT_MEMSTATUS=0` (`LIBSQLITE3_FLAGS`) | zralé; správa mnoha databází, checkpointer a promítnutí outboxu vlastní (kapitola 4.3) |
| PostgreSQL (později, hostovaná instance se SLA) | `tokio-postgres` (spike použil pipelining a `unnest`) | zralé |
| Objekty | `object_store` | zralé |
| Kryptografie | RustCrypto (`aes-gcm`, `chacha20poly1305`), `secrecy`, `zeroize` | zralé |
| Pozorovatelnost | `tracing`, `opentelemetry`, `metrics` | zralé |
| Vazby do klientů (jen pro volitelnou repliku) | `uniffi`, `wasm-bindgen` | zralé; SQLite ve WASM nad OPFS je novější |
| Testy | `wiremock`, `insta`, `proptest` | zralé |
| Později pošta | `mail-parser`, `mail-builder`, `imap-codec`, `jmap-client` | dobrý stav |

Licence klíčových závislostí ověřila fáze 0 (providers.md, calcard.md, recurrence.md). Převzaté jádro `rrule` potřebuje `LICENSE-MIT`, `LICENSE-APACHE` a copyright „2021 Fredrik Meringdal, Ralph Bisschops". Licence a advisories hlídá `cargo-deny` v CI od fáze 1, ne až před vydáním.

### Kde budeme psát vlastní

1. **Rekurence**: vlastní modul s převzatým jádrem `rrule`, vlastní hranou pro zóny a politikou (povinné okno, strop výskytů). Diferenciální testy ze spiku proti `python-dateutil` a `calcard-next` v CI, `tzcheck` při každé aktualizaci tzdb, později skutečná rozbalení od poskytovatelů.
2. **CalDAV a CardDAV klient** včetně profilů serverů.
3. **Klienti Google a Graph**: tenké typované obálky nad pár koncovými body.
4. **Převod opakování Graph ↔ RRULE**, tabulka zón a cache JWKS.
5. **Záplatování surového iCalendaru a vCard** tak, aby se nedotčené části nezměnily ani o bajt: změna jen hodnoty, odmítnutí řádků s `ENCODING`/`CHARSET`, po každé změně nové parsování a srovnání AST, při nesouladu odmítnutí. Odhad 400–800 řádků **(neověřeno)**. Ve spiku zůstalo beze změny 98,3 % případů iCal a 87,5 % případů vCard `TEL`; zbytek vrstva odmítla.
6. **Správa SQLite per tenant**: dva soubory, checkpointer, promítnutí outboxu s vodoznakem, snímky a obnova, později průběžný odvoz WAL.
7. **Průmět, `lift` a třícestné slučování entit** (kapitola 12). Hotová knihovna pro to neexistuje.

---

## 9. Poučení z existujících systémů

| Systém | Co dělá dobře | Co dělá špatně nebo jinak | Co si vzít |
| --- | --- | --- | --- |
| **Nylas** | v2: uložená data a proud změn, akční log. v3: jednotné API, auth, webhooky | v3 pro Google a Microsoft data neukládá a dotazy přeposílá: latence a limity poskytovatele jdou přímo na aplikaci, není offline; webhooky nerozliší vytvoření od změny | proud změn a akční log z v2; poučení, že průchozí model nedá okamžité UX |
| **Cronofy** | úzké zaměření na kalendáře, ID řízená aplikací, přiznané synchronizační okno | nejmenší společný jmenovatel, upsert není podmíněný zápis | poctivost o pokrytí, periodická pojistka i s pushem |
| **Merge a obecná unified API** | společný model plus surová data poskytovatele jako únik | plánované dotazování, data hodiny stará | potvrzení tří vrstev (kanonická + surová) |
| **Aurinko** | delta sync jako veřejný kontrakt pro vývojáře | hranice proudu změn a mazání je třeba ověřit | kurzorové API jako první občan |
| **OneCal** | sjednocené kalendářové API | veřejné podklady jsem neověřoval | — |
| **Nango** | oddělení auth, sync a akcí, typované checkpointy | doménu neřeší | checkpoint jako typ, ne řetězec |
| **DAVx⁵** | roky znalostí o zvláštnostech serverů, ETagy, zachování neznámých vlastností, WebDAV-Push | server vždy vyhrává | profily serverů, zachování surových dat |
| **vdirsyncer, pimsync** | uložený základ pro třícestné rozhodnutí, konflikty explicitní; pimsync je přepis v Rustu | synchronizuje jen dvojici úložišť | bez základu nejde poznat, kdo položku smazal; náš model je hvězda s entitou uprostřed místo párů |
| **Zrcadlení kalendářů** (OneCal, Reclaim a podobné; z paměti, neověřeno) | kopie události v druhém kalendáři bez účastníků, označená skrytou vlastností | většinou jen události, často jednosměrně a jen jako „Obsazeno" | potvrzení pravidel „kopie bez účastníků" a „značka u poskytovatele"; pravidla v kapitole 13 dělají totéž nad modelem entity, takže zvládnou i obousměrný směr |
| **Radicale** | jednoduchý referenční CalDAV server | — | testovací server v CI |
| **Stalwart** | celý server v Rustu, JMAP + IMAP + CalDAV, abstrakce úložiště, vlastní knihovny | je to server záznamu, cizí účty neagreguje; WebDAV-Push zatím nemá | knihovny; důkaz, že to v Rustu jde |
| **Fastmail a JMAP** | stavové řetězce, `/changes`, push, dávky, jeden protokol pro poštu i kalendáře | kalendářová část ještě není RFC | sémantika našeho API |
| **Evolution Data Server** | démon s backendy, lokální cache a živými pohledy pro aplikace: přesně „sync primitivum jako služba" | C a GObject, vázané na desktop | potvrzení tvaru démon + cache + pohledy |
| **Thunderbird** | abstrakce poskytovatelů, offline cache; nové protokoly pro Exchange píše v Rustu (z paměti, neověřeno) | historická zátěž | další hlas pro Rust v protokolové vrstvě |
| **Apple CalendarAgent a EventKit** | démon na účet, lokální SQLite, API s oznámeními změn, push přes APNs | uzavřené, push jen pro Apple | důkaz modelu démon + lokální úložiště + oznámení |
| **Zero, Replicache, PowerSync, Linear** | server jako autorita, klient přeskládává čekající změny, lokální replika | vázané na vlastní backend | model klientského protokolu |

Společný závěr: systémy, které působí okamžitě (EventKit, EDS, Linear, Fastmail), mají **vlastní úložiště dat a proud změn**. Pro musubi-sync to znamená úložiště na serveru a proud změn pro aplikace; replika na zařízení je další krok téhož principu, ne podmínka. Systémy, které data jen přeposílají (Nylas v3, unified API), jsou jednodušší na provoz, ale rychlost dědí od poskytovatele. Předchozí práce k plánování mezi servery (iSchedule, iMIP, Matrix, ActivityPub, Nextcloud) je v kapitole 15.4.

Obousměrné udržování jednoho záznamu u několika poskytovatelů, kde pravda může být bohatší než kterékoli umístění, jsem u žádného z těchto systémů jako veřejně popsaný model nenašel (bez cíleného průzkumu). Je to hlavní nová práce projektu a největší riziko.

---

## 10. Musubi: co převzít a proč je pomalé

### 10.1 Proč je současná implementace pomalá

Cílené čtení kódu, bez profilování. Jde o strukturální příčiny, ne o naměřené hodnoty.

1. **Jen dotazování, výchozí interval 5 minut, žádný push.** `apps/api/src/index.ts:591-602` spouští `syncUser` pro všechny uživatele na časovač. Dokumentace (`packages/docs/src/content/docs/architecture/sync.mdx`, oddíl „Near-realtime") výslovně říká, že Musubi neinstaluje žádný webhook. Změna u poskytovatele je vidět v průměru po 2,5 minutě, a když běh přeteče interval, další tik se přeskočí.
2. **Jeden proces na databázi.** Tentýž dokument popisuje advisory lock, který vynucuje jediný API proces, a varuje před obcházením. Synchronizace všech uživatelů sdílí jeden proces Node.js.
3. **Adaptér vrací celou sadu změn najednou.** Kontrakt `fetchChanges` v `apps/api/src/sync/adapter.ts:279-293` říká, že adaptér stránkuje uvnitř a vrátí kompletní seznam i nový kurzor. Úvodní sync velkého kalendáře drží vše v paměti a uživatel nevidí nic, dokud neskončí.
4. **Zápis po jedné položce.** `reconcileExternalChanges` v `apps/api/src/sync/engine.ts:244-282` volá v cyklu `await writer.upsertEvent(...)` pro každou změnu zvlášť. Každé volání znamená samostatné dotazy do PostgreSQL přes síť.
5. **Microsoft: okno, rozbalené výskyty, dotazy navíc.** `apps/api/src/sync/adapters/microsoft.ts:64-75` popisuje pevné okno −180 až +730 dní a vynucený reset u jeho okraje. Řádky 906–915 popisují, že výskyty série chodí jako útržky a hlavní událost se dotahuje zvlášť. Zrcadlí se výskyty, ne série.
6. **Klient po oznámení znovu načítá.** SSE rámec `external_sync` říká jen „změnily se tyto kalendáře" a klient si znovu stáhne události a úkoly. Kurzorem je čas serveru (`apps/api/src/handlers/events.ts:677-697`), což je hrubé a náchylné k souběhům.
7. **Zápis je opatrný a tím pomalý.** Čtení před zápisem, specializované toky podle poskytovatele a obnova outboxu po 15 sekundách (`apps/api/src/sync/background.ts:3`), nejvýš 40 kandidátů se čtyřmi pracovníky. Opatrnost je správná, cena za ni je vysoká, protože není oddělená od interaktivní cesty.
8. **Strukturální dluh.** Typ `CalendarAdapter` obsahuje kolem 60 metod specifických pro poskytovatele (`adapter.ts:198-262`) a importuje typy databáze (`adapter.ts:1-18`). Adresář `apps/api/src/sync` má 313 souborů, schéma 41 tabulek a 93 dotazových modulů. Každá funkce je samostatná cesta pro každého poskytovatele.

Vlastní audit Musubi (`docs/research/nylas/04-musubi-audit.md`, oddíl 9) jmenuje tytéž body: rozhraní závislé na úložišti, sekvenční plánování, opakované DAV zjišťování, chyby jako text.

### 10.2 Co převzít jako znalost

**Invarianty** (tabulka v `docs/research/nylas/01-findings-and-decisions.md`, oddíl 3):

- lokální ID, lokátor u poskytovatele a UID jsou tři různé věci;
- původní začátek výskytu je identita výjimky;
- časový model rozlišuje zónový, plovoucí a celodenní čas;
- nepřítomnost v neúplném čtení není smazání;
- neměnný záměr a pořadí předchůdců v outboxu;
- nejistý výsledek se řeší pozorováním, ne opakováním;
- uložená schůzka není doručená pozvánka;
- minimální záplata místo přepsání celé série.

**Katalog zvláštností** v `docs/sync/*.md`, `packages/docs/src/content/docs/providers/*.mdx` a `docs/audits/`:

- Google: úprava EXDATE se zónou, celodenní EXDATE, roční pravidlo bez měsíce, exkluzivní konec celodenní události, `410` jako reset, ID z operace pro idempotentní vytvoření.
- Microsoft: okno delty, útržky výskytů, Windows zóny a mapování přes CLDR, rotace refresh tokenu, barvy z předvoleb, `transactionId`, DELETE bez skutečného podmíněného zápisu.
- CalDAV a iCloud: identitou je URL zdroje, ne UID; `Schedule-Tag`; chybějící ACL u iCloudu; oddílové hostitele; oříznutí hesla ze schránky mobilu.

**Testovací materiál:**

- 27 JSON souborů scénářů chování (zjišťování CalDAV, DAV odpovědi, pokusy o čtení, výsledky syncu, doručení událostí a úkolů, Google RSVP);
- testy s falešnými servery Google a Graph (`apps/api/src/sync/adapters/provider_http.test.ts`);
- integrační testy proti Radicale;
- testy parity kodeků;
- matice podpory `docs/sync-alpha/supported-matrix.v1.json` a konformační sada v `apps/sync-reference/conformance`.

**Hotový výzkum:** deset kapitol v `docs/research/nylas/` (říjen 2026). Návrh z nich přebírá hlavně:

- webhook znamená jen „kolekce může být zastaralá"; oznámení se trvale uloží před potvrzením a několik se sloučí do jednoho čtení (kapitola 06);
- kurzor smí postoupit až s přijatou změnou; pád před posunem kurzoru znamená opakované čtení, pád po něm ztrátu (06);
- stránkovací token není sync kurzor a ve veřejných typech se nesmí zaměnit (09);
- chyba nese tři nezávislé údaje: třídu příčiny, jistotu vedlejšího účinku a doporučený další krok (09);
- bezpečnost podmíněného zápisu a zachování lokálního záměru jsou dvě různé vlastnosti; DAVx⁵ má první, ne druhou (08);
- free/busy je samostatná projekce intervalů, ne zkrácená událost (09);
- přehled licencí: DAVx⁵ GPL-3.0, Nango Elastic License, Etebase SDK BSD-3-Clause (06, 08).

**Co nepřebírat:** členství a role v kalendářích Musubi, „domácí autoritu" sdílených úkolů, generace soukromí, přepínače funkcí. Jsou to vlastnosti aplikace a patří nad primitivum. Jádro místo nich nabízí pozvánky mezi tenanty a později sdílení ke čtení (kapitola 14); zda jimi Musubi své členství nahradí, je otázka přechodu (kapitola 11.4). Z federace Musubi se přebírají poučení, ne kód ani model (kapitola 15.5).

---

## 11. Plán, rizika a otevřená rozhodnutí

### 11.1 Fáze

| Fáze | Obsah | Kritérium hotovo |
| --- | --- | --- |
| **0. Rozhodovací testy** — **hotovo** | srovnání úložišť ([storage.md](../research/phase0/storage.md)); zkouška `calcard` ([calcard.md](../research/phase0/calcard.md)); diferenciální test rekurence ([recurrence.md](../research/phase0/recurrence.md)); ověření tvrzení o poskytovatelích z dokumentace a plán živých testů T1–T12 ([providers.md](../research/phase0/providers.md)) | úložiště rozhodnuto (SQLite, kapitola 11.4); knihovna rekurence a calcard rozhodnuty (kapitola 8); cíle z kapitoly 5.1 upravené podle měření; model sérií v Graphu má oporu v dokumentaci, živé potvrzení (T1) před fází 7 |
| **1. Důkaz jádra** ([plán](../plan/phase1.md)) | Google Calendar jen čtení: OAuth, zjišťování, proudový úvodní sync, inkrement, watch kanály s pojistným dotazováním, tři vrstvy dat, index výskytů, SSE, základ konformační sady. **Entity a umístění jsou v datech i v API od začátku** (zatím vždy jedno umístění). Úložiště SQLite podle podmínek z kapitoly 4.3, zálohování a obnova. Klíče API a krátkodobé tokeny. CI s `cargo-deny` | první data do 1 s od připojení; změna u Googlu v proudu změn do 500 ms od oznámení (proti falešnému serveru); dotaz na rozsah p99 pod 10 ms u běžného tenanta; `kill -9` v libovolném okamžiku bez ztráty a duplicit; obnova ze zálohy projde v CI; WAL omezený checkpointerem; živá kontrola proti testovacímu účtu Google včetně T8 |
| **2. Druhý adaptér a celá rodina pro čtení** | obecný CalDAV a CardDAV včetně iCloudu: události, VTODO, kontakty; Google Tasks a People API; adaptivní dotazování; předem živé testy T10 a T11 | stejná sada testů prochází pro oba adaptéry; rozhraní adaptéru zmrazeno jako v0 |
| **3. Zápisy** | soubor outboxu a promítnutí s vodoznakem (kapitola 4.3); mutace entity, doručení na jedno umístění, podmíněné zápisy, konflikty, nejisté výsledky pro Google a DAV, pro události, úkoly i kontakty; RSVP a změny organizátora; předem živý test T9 | zápis potvrzený poskytovatelem p95 pod 1 s proti falešnému serveru s realistickou odezvou; scénáře konfliktu a nejistého vytvoření prochází; žádná duplicita při pádu |
| **4. Více umístění** | předem živý test T2 (přesuny); materializované entity, průmět a `lift`, rozeslání, příchozí slučování, záznamy o konfliktech, mazání včetně politiky `on_remote_delete` a obnovy z náhrobku, pravidla pro účastníky, připojení a odpojení, značky; Google a DAV; události, úkoly i kontakty | úprava na jednom umístění je na ostatních do 1 s od oznámení; souběžná úprava různých polí se sloučí; stejného pole skončí záznamem o konfliktu; ztrátové umístění nezničí bohatší data (property testy); žádná smyčka mezi dvěma falešnými servery, které normalizují různě; žádná druhá pozvánka; s `propagate` zmizí záznam všude jen na výslovný důkaz a hromadné smazání se pozdrží; pád kdykoli bez duplicit a bez ztráty vazby; obnova ze starší zálohy se dorovná |
| **5. Pravidla synchronizace** | model pravidla, vyhodnocení v transakci, jednosměrná a obousměrná pravidla, filtr, maska „Obsazeno", dosyp s náhledem, změna, vypnutí a odebrání pravidla, výjimky, pojistka hromadného vytváření, API (kapitola 13) | A → B spolu s B → A se ustálí bez smyčky a bez duplicit; řetěz A → B, B → C nepustí záznam z A do C; po odebrání pravidla s `placements=delete` nezůstane u poskytovatele žádná kopie; maskovaná kopie neobsahuje žádný text zdroje ani po úpravě zdroje; změna u poskytovatele se podle pravidla propíše **bez připojené aplikace**; náhled sedí s výsledkem dosypu; restart uprostřed dosypu bez duplicit |
| **6. Plánování mezi tenanty** | předem živý test T12 (iCloud); vestavěný poskytovatel `local` a schránka pozvánek; plánovací adresy a adresář; zprávy REQUEST, REPLY, CANCEL a REFRESH; outbox → inbox; zápis místních účastníků k poskytovateli s potlačením jeho pošty, kde to jde; spojení pozvánky doručené dvěma cestami; jedna cesta odpovědi; známí odesílatelé, žádosti, blokování, limity; smazání schůzky s výslovným oznámením (kapitola 14) | pozvánka je u místního příjemce do 1 s; pozvaný vidí právě jednu pozvánku, ať přišla jednou nebo dvěma cestami a v jakémkoli pořadí; seznam účastníků u poskytovatele je úplný; na CalDAV serveru s `SCHEDULE-AGENT` nedostane místní účastník žádný e-mail a vnější účastník dostane právě jednu pozvánku; odpověď má jednu cestu a dojde organizátorovi; pozvánka od neznámého odesílatele není v kalendáři, dokud ji příjemce nepřijme; smazání kopie schůzku nezruší a smazání entity organizátorem bez `notify_attendees` se odmítne; opakované a přeházené doručení je bez účinku; cizí tenant nezmění ani nezruší cizí schůzku; po obnově kteréhokoli tenanta ze starší zálohy se obě strany dorovnají; pád mezi zápisem příjemce a potvrzením bez duplicit |
| **7. Microsoft** | předem živé testy T1 a T3–T7 na pracovním i osobním účtu; Graph kalendář, To Do a kontakty; subscriptions; série podle výsledku T1; Outlook jako umístění a jako cíl pravidel | jedna položka na sérii, nebo přiznané omezení podle kapitoly 11.2; matice schopností generovaná z testů |
| **8. Vydání 1.0** | agregované free/busy; ověřování tokenů z vnějšího IdP (OIDC) pro samostatný self-host; dokumentace, návody pro poskytovatele, pravidla stability API; Musubi převedené na musubi-sync | cizí vývojář zprovozní instanci a připojí účet podle dokumentace bez pomoci |
| **9. Sdílení kalendáře ke čtení** | jednosměrné pravidlo s cílem v odebírané interní kolekci jiného tenanta; souhlas a odvolání (kapitola 14.10) | příjemce vidí změny do 1 s; odvolání odebere kolekci i kopie z ní vytvořené; maska platí i přes hranici tenanta |
| **10. Federace** (podmíněná) | plánovací zprávy mezi servery v rozsahu z kapitoly 15.6. **Staví se, jen když ji chtějí aspoň dva provozovatelé mezi sebou** | scénáře s falešným partnerem: podvržený podpis, cizí doména organizátora, přehraná zpráva, cíl v soukromé síti, přesměrování, příliš velké tělo, neznámá verze a nepovolená doména jsou odmítnuty; nepřijatý odesílatel se nedostane do žádného kalendáře; odvolání partnera zastaví provoz okamžitě; **externí bezpečnostní revize bez otevřených závažných nálezů** |
| **11. E-mail** | Gmail API, Graph mail, IMAP, JMAP; úložiště objektů, fulltext | jádro se nemění, přibývají druhy a adaptéry |
| **12. Sdílení kalendáře se zápisem** | vzdálená mutace s autoritou u vlastníka, oprávnění, odvolání, konflikty přes hranici tenanta (kapitola 14.10). Pořadí vůči fázím 10 a 11 je volné; podmínkou je osvědčená fáze 9 | zápis příjemce je u vlastníka i u jeho poskytovatelů do 1 s; odvolání zastaví zápisy okamžitě; souběžná úprava obou stran skončí sloučením nebo záznamem o konfliktu u vlastníka |
| Později, volitelně | vlastní e-mailové pozvánky (iMIP) po fázi 11; free/busy mezi tenanty se souhlasem; protinávrh času; zprávy z chatovacích aplikací; lokální replika v klientovi; CalDAV nebo JMAP fasáda | — |

**Co se z fáze 0 přesunulo.** Fáze 0 neměla živé účty, proto proběhla jen dokumentační část. Přesunuto:

- **živé testy T1–T12** z [providers.md](../research/phase0/providers.md#plány-živých-testů). Každý běží těsně před fází, která na něm stojí (tabulka výše). Spouští je uživatel na vlastních testovacích účtech;
- **model sérií v Graphu** (T1). Blokuje jen fázi 7;
- **odezva poskytovatelů** (kapitola 5.1). Google změří živá kontrola na konci fáze 1, ostatní před jejich fází;
- **srovnání CPU a paměti s kodeky Musubi.** Benchmarky fáze 1 změří paměť aktora a cenu kodeku, srovnání s Musubi je volitelné;
- **úložiště na ext4/XFS a disku s PLP.** Změří se ve fázi 1 na cílovém stroji, pokud bude k dispozici **(neověřeno)**.

Proč v tomto pořadí:

- **Google první:** má sync token i push, takže jeden adaptér prověří celou cestu včetně webhooků a latenčních cílů.
- **DAV druhý:** je protokolově úplně jiný, testuje se hermeticky proti skutečnému serveru v CI, iCloud je jeho profil a CardDAV sdílí s CalDAV klienta, zjišťování i deltu.
- **Více umístění hned po zápisech:** je to hlavní důvod projektu, ale stojí na spolehlivém zápisu na jedno umístění a potřebuje dva různé adaptéry.
- **Pravidla hned po více umístěních:** pravidlo jen rozhoduje, která umístění mají existovat. Bez spolehlivých umístění by automatizovalo chyby; s nimi je to malý přírůstek, který uživatel chce nejvíc.
- **Plánování mezi tenanty po pravidlech:** příchozí pozvánka je užitečná hlavně tehdy, když ji pravidlo samo zrcadlí do kalendáře u poskytovatele. Stojí také na pravidlech pro účastníky z fáze 4.
- **Microsoft po nich:** je to práce na adaptéru, ne na jádru, a může běžet souběžně s fázemi 5 a 6, jakmile je rozhraní adaptéru zmrazené.
- **Sdílení ke čtení až po 1.0:** je to první zpráva, která přes hranici tenanta nese celý kalendář. Má přijít, až bude přeprava zpráv prověřená provozem.
- **Federace poslední a podmíněná:** stejná přeprava, ale s nedůvěryhodnou protistranou (kapitola 15).
- **Sdílení se zápisem až po sdílení ke čtení:** potřebuje vzdálenou mutaci s autoritou u vlastníka. Nejdřív se má osvědčit čtení.
- **Entity v datech od fáze 1:** přidání umístění pak není migrace schématu ani změna API. Stejně tak pole umístění pro pravidla (`direction`, `projection`, `origin`) a kanál účastníka jsou ve schématu od fází 3 a 4, i když je zatím nic nenastavuje.

Vývoj probíhá v repozitáři `f-tuma/musubi-sync`. Doporučuji ho otevřít nejpozději s fází 1 pod označením 0.x. Pozdní otevření hotového kódu přináší méně zpětné vazby na rozhraní adaptéru.

### 11.2 Spike Microsoft Graph: dokumentace ve fázi 0, živé testy před fází 7

Na Graphu stojí dvě věci: model jedné položky na sérii a možnost umístit opakovaný záznam do Outlooku. Dokumentační část proběhla ve fázi 0 ([providers.md](../research/phase0/providers.md), G1–G14). Živá část potřebuje pracovní i osobní účet, proto se přesunula na začátek fáze 7. Jádro na ní nestojí, jen adaptér Graphu.

Co říká dokumentace v1.0 (ověřeno z webu):

- `cancelledOccurrences` u hlavní události obsahuje ID zrušených výskytů. Vyžaduje `$select` a vrací se jen při čtení hlavní události podle ID.
- `exceptionOccurrences` jsou výjimky série. Vyžaduje `$select` a `$expand` a vrací se také jen při čtení hlavní události podle ID. Dokumentace uvádí příklad `GET /me/events/{id}?$select=…&$expand=exceptionOccurrences`.
- `iCalUId` je identifikátor napříč kalendáři a liší se pro každý výskyt série.
- Výpis `/me/events` vrací samostatné události a hlavní události sérií.
- `occurrenceId` se v příkladu vyskytuje, ale v tabulce vlastností v1.0 chybí.

Co fáze 0 z dokumentace doplnila:

- Výjimky a zrušené výskyty vrací jen GET hlavní události podle ID; zrušený výskyt je znám jen datem (`OID.{masterId}.{yyyy-mm-dd}`).
- Pole `uid` (společné pro sérii) je jen v betě. `iCalUId` hlavní události je jediný stabilní klíč ve v1.0.
- `calendarView/delta` ve v1.0 funguje jen pro primární kalendář. Sekundární kalendáře potřebují subscription a dotazování.
- Neměnná ID vyžadují hlavičku `Prefer: IdType="ImmutableId"` u každého požadavku i u subscription. API pro přesun neexistuje.
- Bod 6 je zodpovězen negativně: Graph potlačení pozvánek nemá a obchvat přes `isDraft` a MAPI vlastnosti zamítáme. EWS končí 1. 4. 2027.

Co má ověřit živě test T1 (a souvisící T2–T7), na pracovním i osobním účtu:

1. Vrací `$expand=exceptionOccurrences` všechny výjimky se všemi potřebnými poli? Jak se chová série se stovkami výjimek (stránkování, limity)?
2. Jde z `cancelledOccurrences` a z `originalStart` výjimek spolehlivě odvodit původní začátek výskytu, včetně celodenních událostí a změny zóny?
3. Jak se bez `events/delta` dozvíme, **která** série se změnila: z oznámení subscription, nebo jen z `calendarView/delta` jako signálu? Kolik požadavků stojí jedna změněná série, a jde je sdružit přes `$batch`?
4. Je `iCalUId` hlavní události stálý při přesunu mezi kalendáři a shodný u kopií téže schůzky v jiných schránkách?
5. Vytvoří zápis série s výjimkami u Outlooku to, co jsme poslali (pořadí: hlavní událost, potom úpravy výskytů)?
6. ~~Jde potlačit rozeslání pozvánek?~~ Z dokumentace ne; kopie bez účastníků je jediná bezpečná cesta (T4 krok 3 jen pro záznam).

Rozhodnutí podle výsledku:

| Výsledek | Model |
| --- | --- |
| Body 1–3 fungují na obou typech účtů | **jedna položka na sérii** ve v1.0; `calendarView/delta` jen jako signál a pojistka |
| Výjimky nejdou načíst úplně, ale výskyty ano | **záložní model A:** hlavní událost plus `instances` v omezeném horizontu; výjimky se odvodí porovnáním s rozbaleným pravidlem. Série je pořád jedna položka, výjimky známe jen v horizontu |
| Ani to není spolehlivé | **záložní model B:** okno s rozbalenými výskyty jako dnes v Musubi. Opakované záznamy pak v Outlooku nejdou použít jako obousměrné umístění, jen číst |

Beta `events/delta` se sleduje, ale návrh na ní nestojí.

### 11.3 Hlavní rizika

1. **Správnost více umístění.** Slučování, smyčky a mazání přes několik poskytovatelů je nejjemnější část projektu a nemá hotový vzor. Zmírnění: čisté funkce, property testy invariantů, scénáře se dvěma falešnými servery, pojistka proti kmitání, mazání se ve výchozím stavu nepropisuje.
2. **Zdvojené nebo nechtěné pozvánky.** Chyba je vidět u cizích lidí a nejde vzít zpět. U Googlu, Microsoftu a podle fáze 0 i u iCloudu jde pozvánka místnímu účastníkovi záměrně dvěma cestami; chyba ve spojení by znamenala dvě pozvánky v kalendáři. Zmírnění: účastníci jen na hlavním umístění, brána hlavního umístění, spojení podle dvojice (organizátor, UID), jedna cesta odpovědi, potlačení pošty poskytovatele tam, kde to jde, testy „jedna pozvánka" v konformační sadě.
3. **Ztráta autorských dat.** Vazby, pravidla a pravdu entity nikdo znovu nestáhne. Zmírnění: zálohování ve fázi 1, plný fsync, značky u poskytovatelů, test obnovy.
4. **Rozeslané smazání.** S politikou `propagate` se mylně odvozené smazání rozšíří na všechny poskytovatele. Zmírnění: jen výslovný důkaz, pojistka hromadného mazání s ověřením přímým čtením, úprava vyhrává nad smazáním, náhrobek s lhůtou na obnovu, výchozí hodnota zůstává `detach`.
5. **Automatické zápisy pravidel.** Pravidlo zapisuje bez toho, aby se někdo díval: špatný filtr zaplní cizí kalendář a plné zrcadlení do sdíleného kalendáře zveřejní osobní data. Zmírnění: náhled s počty před založením, upozornění na sdílený cíl, pojistka hromadného vytváření, odebrání pravidla, které po sobě uklidí.
6. **E-mail navíc a odpověď u poskytovatele.** U Googlu, Microsoftu a iCloudu dostane místní účastník vedle pozvánky v jádru i e-mail poskytovatele. Fáze 0 riziko rozšířila: Graph potlačení nemá (ověřeno), iCloud `SCHEDULE-AGENT=CLIENT` podle jediné sekundární zkoušky ignoruje a při smazání neposílá zrušení. Odpověď místního účastníka do položky organizátora jde u Googlu zapsat, ale s vedlejším účinkem resetu; u Graphu je to neověřené a výchozí schopnost je `false`, takže vnější účastníci uvidí „čeká". Zmírnění: kopie bez účastníků u Graphu a iCloudu, schopnost v popisu adaptéru (kapitola 14.6), živé testy T4, T9 a T12.
7. **Bezpečnost federace.** Vlastní protokol mezi servery bez standardu a bez interoperability. Zmírnění: zúžený rozsah, vypnuto ve výchozím stavu, seznam povolených partnerů, externí revize jako podmínka, stavět až na skutečnou poptávku.
8. **Správnost opakování.** Fáze 0 riziko zmenšila (0 chyb jádra v korpusu), ale převzaté jádro udržujeme sami a hrana pro zóny je zatím prototyp. Zmírnění: vlastní modul, 12 povinných úprav z recurrence.md, diferenciální testy a fuzzing v CI, `tzcheck` při aktualizaci tzdb.
9. **Model sérií v Graphu.** Dokumentace model podporuje, živé potvrzení chybí. Zmírnění: živý test T1 na začátku fáze 7 se dvěma záložními modely; jádro na výsledku nestojí.
10. **Bezeztrátový zápis zpět.** Záplatování surového iCalendaru a vCard je jemná práce a serializace `calcard` normalizuje. Korpus spiku je syntetický a reálné payloady poskytovatelů jsme neviděli. Zmírnění: vlastní vrstva záplat s kontrolou AST po každé změně, property testy, odmítnutí nepodporovaných tvarů (`ENCODING`, `CHARSET`) před zápisem, harness spiku nad anonymizovanými reálnými payloady.
11. **Latence mimo naši kontrolu.** Doručení oznámení poskytovatelem a poskytovatelé bez pushe. U více umístění to znamená, že změna z iCloudu dorazí na ostatní až po intervalu dotazování. Zmírnění: měřit a zveřejnit skutečná čísla po poskytovatelích.
12. **Registrace aplikací u poskytovatelů.** U open-source projektu si ji dělá každý provozovatel sám. Ověření aplikace u Googlu pro citlivé scope je pro jednotlivce překážka; bez něj platí strop 100 nových uživatelů celkem a varovná obrazovka (ověřeno ve fázi 0). Zmírnění: podrobný návod, minimální scope.
13. **Dosažitelnost webhooků u self-hostu.** Bez veřejné adresy zbývá dotazování, tedy horší latence. Volitelné relé je možné, ale znamená provozovat službu.
14. **Rychlost vývoje v Rustu a rozpínání rozsahu.** Více umístění, kontakty, pravidla a plánování mezi tenanty rozsah před vydáním 1.0 znatelně zvětšily: přibyly dvě fáze. Zmírnění: úzké milníky s měřitelnými kritérii; sdílení kalendářů, federace, vlastní e-mailové pozvánky a fasády vědomě až po 1.0 nebo mimo.
15. **Pohyblivé standardy.** JSCalendar 2.0 a JMAP Calendars nejsou vydané. Zmírnění: verze schématu a přepočet ze surové vrstvy.
16. **Kvóty.** Bez pushe strop v tisících uživatelů na projekt Google. Rozeslání a pravidla navíc násobí počet zápisů počtem umístění. Google Calendar má pro nové projekty denní práh 1 000 000 požadavků na projekt s ohlášeným účtováním a kvóty People API nejsou veřejné.
17. **Provoz SQLite.** WAL roste bez vlastního checkpointeru na GB a dlouhý čtenář ho nafoukne i s ním. Přijetí během importu téhož tenanta má chvost kolem 10–16 ms. ext4/XFS, disk s PLP, odvoz WAL a data větší než RAM jsou neměřené. Zmírnění: podmínky z kapitoly 4.3 jako požadavky fáze 1, metriky WAL na tenanta, crash testy `kill -9`, PostgreSQL za úzkým rozhraním jako úniková cesta.
18. **Advisory v závislostech.** RUSTSEC-2023-0071 (`rsa` 0.9) táhnou `jsonwebtoken` i `openidconnect`. Soukromý klíč RSA nepoužíváme, ale `cargo-deny` potřebuje dočasnou výjimku. Zmírnění: výjimka s datem revize, přechod na `rsa` 0.10, až ho knihovny přijmou.

### 11.4 Rozhodnutí

**Rozhodnuto uživatelem (11. 11. 2026):**

- Projekt je open source pod licencí MIT, jmenuje se musubi-sync, Musubi je jeden ze spotřebitelů a self-host třetími stranami je výchozí případ.
- Jeden záznam může žít na více místech a jádro tento model vlastní.
- Cílem je rychlost primitiva pod 1 s na operaci; lokální replika v klientovi je volitelná budoucí práce.
- Kontakty (CardDAV, People API, Graph contacts) jsou v první vlně.
- Ověření Graphu je ve fázi 0. Dokumentační část proběhla; živé testy se přesunuly před fázi 7 (kapitola 11.1).
- **Tenant je jeden člověk.** Sdílené tenanty nejsou. Lidé na stejném serveru spolu jednají plánováním mezi tenanty v jádru (kapitola 14).
- **Data aplikací** (štítky, poznámky, pole známá jen aplikaci) žijí nad jádrem, v databázi aplikace s klíčem `entity.id`. Jádro drží jen vazby entita–umístění. Spojení a rozdělení entit oznamuje proud změn, aby si aplikace svá data přepojila.
- **Smazání na jednom místě:** výchozí stav je „zaniká jen to umístění". „Smazat všude" je plnohodnotné nastavení na záznamu i na pravidle (`on_remote_delete: propagate`) s pojistkami z kapitoly 12.7.
- **Konflikt stejného pole:** vyhrává později zpracovaná změna a vzniká záznam o konfliktu.
- **Připomenutí** jsou pole umístění.
- **Pravidla synchronizace žijí v jádru** (kapitola 13). **Maska „Obsazeno"** je průmět pravidla a je v rozsahu.
- **Ztrátové umístění času a opakování** se bez výslovného `allow_lossy` odmítá.
- **Úprava kopie cizí pozvánky** se vrací přepsáním.
- **Značka entity** ve skrytém poli položky u poskytovatele je povolená.
- **Federace:** uživatel ji chce, pokud je proveditelná a bezpečná. Posouzení a verdikt jsou v kapitole 15.

**Rozhodnuto koordinátorem z pověření uživatele** (ověřování a pozvánky; u pozvánek s cílem co největšího pohodlí uživatele):

- **Ověřování.** musubi-sync nemá přihlašování koncových uživatelů: žádná hesla, obnova hesla, 2FA ani e-maily. Aplikace ověřují své uživatele samy; Musubi si nechává better-auth. Jádro přijímá klíče API vázané na tenanta, krátkodobé tokeny s rozsahem pro klienty a u samostatného self-hostu tokeny OIDC/JWT z vnějšího IdP. OAuth k poskytovatelům a správa jejich tokenů zůstávají v jádru. Identita serveru pro federaci je oddělená (kapitoly 1 a 7).
- **Koncové šifrování** není cílem (kapitola 7).
- **Místní účastník je zapsaný i u poskytovatele organizátora**, aby úplný seznam účastníků viděli všichni. Poštu poskytovatele jádro potlačí tam, kde to jde po účastnících. Jinde ji přijme a pozvánku u příjemce spojí podle dvojice (organizátor, UID). Uživatel vidí jednu pozvánku a má jednu cestu odpovědi (kapitoly 14.6 a 14.7).
- **Smazání kopie schůzky** s účastníky odstraní jen tu kopii a schůzku nikdy nezruší. Zrušit ji jde jen výslovným smazáním entity organizátorem, s `notify_attendees: true`. Smazání entity pozvaným znamená odmítnout a odebrat (kapitola 12.7).
- **Kdo smí zvát:** kdokoli na stejném serveru. Pozvánka od známého odesílatele je rovnou v kalendáři jako předběžná, od neznámého čeká mezi žádostmi. Příjemce si to může zpřísnit i uvolnit; limity platí na odesílatele (kapitola 14.9).
- **Sdílení kalendáře se zápisem** je pozdější fáze (12). Sdílení ke čtení zůstává ve fázi 9.
- **Vlastní e-mailové pozvánky (iMIP)** pro vnější účastníky u záznamu bez poskytovatele s plánováním nejsou v první verzi; patří k doméně e-mailu. Do té doby platí chyba `no_scheduling_channel`.

**Rozhodnuto uživatelem (11. 11. 2026, druhé kolo):**

- **Federace:** verdikt z kapitoly 15.6 je přijat. Zúžený rozsah (jen plánovací zprávy, ve výchozím stavu vypnutá, jen vyjmenovaní partneři, podpisy RFC 9421, souhlas příjemce), fáze 10 po vydání 1.0, externí bezpečnostní revize jako podmínka a stavba až na poptávku aspoň dvou provozovatelů.
- **Přechod Musubi** proběhne najednou ve fázi 8 (kapitola 11.1): nová verze Musubi nad musubi-sync, ne postupné nahrazování po poskytovatelích. Otázky vlastních kalendářů Musubi (kapitola 14.5) a členství v kalendářích se rozhodnou v rámci fáze 8.

**Rozhodnuto uživatelem (11. 11. 2026, třetí kolo):**

- **Úložiště:** SQLite, jeden soubor na tenanta (přesněji hlavní soubor a soubor outboxu), pro fázi 1 i self-host. PostgreSQL zůstává za úzkým rozhraním úložiště jako pozdější práce pro hostovanou instanci se SLA. Podmínky ze [storage.md](../research/phase0/storage.md) jsou požadavky (kapitola 4.3): samostatný soubor outboxu jako zdroj pravdy, promítnutí podle vodoznaku, žádná atomicita přes soubory, id mutací se nikdy znovu nepoužijí, vlastní checkpointer `PASSIVE` s omezeným `TRUNCATE`, `journal_size_limit`, `SQLITE_DEFAULT_MEMSTATUS=0`, omezené čtecí transakce. Cíl přijetí během importu téhož tenanta je poctivě uvolněný na p99 pod 20 ms (kapitola 5.1). Měření na ext4 a disku s PLP zůstává otevřené.

**Zbývá rozhodnout:**

1. **Hostovaná instance nebo relé webhooků** provozované projektem: rozhodne se později; koordinátor uživateli tehdy předloží možnosti.

**Platí doporučení, dokud nikdo nenamítne:**

- **Veřejné API:** vlastní REST s proudem změn, ne JMAP od začátku.
- **Historie:** poslední stav, žurnál změn a záznamy o konfliktech; žádná auditní historie verzí.
- **Úkoly z iCloudu** v první verzi nejsou.
- **Série v Graphu:** rozhodne živý test T1 na začátku fáze 7; cíl je jedna položka na sérii ve v1.0 a dokumentace ho podporuje.
- **Komprese surové vrstvy** (zstd po řádcích): rozhodne se ve fázi 1 podle reálných payloadů; bez ní ~4,4 KB na událost, s ní ~2,3 KB.

---

## 12. Jeden záznam na více místech: entita a umístění

Tohle je hlavní důvod, proč primitivum vzniká. Jedna událost, úkol nebo kontakt může být zároveň v kalendáři Google, na iCloudu i v Outlooku. musubi-sync drží jeden záznam a všechna jeho umístění udržuje shodná v obou směrech.

### 12.1 Pojmy

| Pojem | Co je | Kdo je autorita |
| --- | --- | --- |
| **Položka** (item) | jedna kopie u jednoho poskytovatele: vzdálený klíč, validátor, surová data a *pozorovaný stav* (kanonický tvar dekódovaný ze surových dat) | poskytovatel |
| **Entita** (entity) | jeden záznam, se kterým pracuje aplikace; nese kanonickou pravdu | musubi-sync |
| **Umístění** (placement) | vazba entity na položku a stav synchronizace této vazby | musubi-sync |
| **Korelace** | „tyto entity popisují tutéž schůzku": stejné UID viděné přes více účtů | odvozená, nic se nepropisuje |

Dva vztahy, které se nesmí plést:

- **Umístění:** záznam vlastníme a změny mezi umístěními propisujeme my.
- **Korelace:** kopie jen pozorujeme, například pozvánku viděnou pracovním i soukromým účtem. Pravdu drží organizátor a rozesílá ji jeho poskytovatel. Nepropisujeme nic, jen nabízíme pohled „jedna schůzka, ne tři". Tady dál platí pravidlo z revize 2: propojení, ne sloučení. Výjimku tvoří jen pozvánka doručená témuž tenantovi dvěma cestami (kapitola 14.7).

### 12.2 Datový model

Každá položka patří právě do jedné entity. Aplikace čte a zapisuje entity. Položky jsou nízkoúrovňový pohled pro ladění.

- **Triviální entita** má jedno umístění. Vlastní kanonický dokument neukládá a čte pozorovaný stav položky. Týká se naprosté většiny dat, je stejně rychlá jako v revizi 2 a dá se celá stáhnout znovu. ID entity se přidělí už při prvním příjmu položky, takže se pozdějším přidáním umístění nezmění.
- **Materializovaná entita** vzniká s druhým umístěním, nebo vytvořením záznamu rovnou na více míst. Má vlastní kanonický dokument a ten je pravda. Materializovaná už zůstane, i kdyby jí zbylo jedno umístění, protože může nést data, která zbylé umístění neunese. Výjimka: umístění, která jen přijímají (jednosměrná pravidla, kapitola 13.4), entitu nematerializují. Pravdou zůstává zdrojová položka.

```
Entity     { id, tenant, kind, canonical, rev, seq, state, tombstone,
             policy }     // on_remote_delete (kapitola 12.7)
Placement  { id, entity, item,
             role: primary | copy | peer,
             direction: both | out,         // out: umístění jen přijímá (kapitola 13.1)
             projection: full | busy_only,  // kapitola 13.5
             origin: manual | rule,         // a která pravidla umístění vlastní
             delivered,   // průmět entity, který jsme sem naposledy doručili
             local,       // pole platná jen zde (kapitola 12.3)
             fidelity,    // která sdílená pole jsou zde přibližná nebo chybí
             delivery,    // synced | pending | conflict | failed | uncertain | diverged | suspended
             reserved_remote_id }  // pro idempotentní vytvoření
```

Každé umístění má dva snímky ve tvaru, který jeho poskytovatel umí vyjádřit:

- `delivered`: co jsme tam naposledy poslali;
- `item.observed`: co poskytovatel naposledy vrátil.

Rozdíl mezi nimi je **normalizace poskytovatele** (oříznuté sekundy, přepsané HTML, zkrácený název). Je stálý a nikdy se nepovažuje za změnu. Oba snímky dohromady tvoří společný základ pro třícestné slučování.

Topologie je **hvězda**: entita uprostřed, umístění kolem. Umístění se mezi sebou nikdy nesynchronizují napřímo. N umístění znamená N vazeb místo N² párů a jediné místo, kde se rozhoduje konflikt. Umístění mohou být u různých poskytovatelů i ve dvou kolekcích téhož účtu.

### 12.3 Sdílená pole a pole umístění

| Třída | Pole | Chování |
| --- | --- | --- |
| **Sdílená** | události: název, časy a zóna, celodenní, opakování a výjimky, místo, popis, stav, kategorie, odkaz na konferenci jako URL. Úkoly: název, poznámka, termín, začátek, dokončení, priorita, opakování. Kontakty: jména, e-maily, telefony, adresy, organizace, výročí, poznámka, fotka | žijí v entitě, propisují se na všechna umístění |
| **Sdílená s možností přepsat** | obsazenost (volno nebo obsazeno), soukromí | hodnota entity platí všude, umístění ji může přepsat ve svém `local` |
| **Plánovací** | organizátor, účastníci a jejich odpovědi | žijí v entitě, zapisují se **jen na hlavní umístění** (kapitola 12.9); místní účastníci se tam zapisují také, podle možnosti s potlačením pošty poskytovatele (kapitola 14.6) |
| **Pole umístění** | připomenutí; barva; moje odpověď na pozvánku; pořadí v seznamu úkolů; členství kontaktu ve skupinách a štítcích; checklist úkolu v Graphu; nativní objekt konference (Meet, Teams); přílohy uložené u poskytovatele; speciální typy událostí | žijí v `placement.local`, nikam se nepropisují |
| **Neznámá** | vlastnosti, kterým kodek nerozumí (X- vlastnosti, rozšíření) | zůstávají v surové vrstvě položky; zápis je záplata nad ní |

Proč jsou připomenutí pole umístění:

- uživatel s jednou událostí ve třech kalendářích na jednom telefonu by dostal tři upozornění;
- schopnosti se u připomenutí liší nejvíc ze všech polí (Google relativní popup a e-mail, Graph jedno, iCalendar libovolné VALARM).

Nová kopie se proto zakládá výslovně bez připomenutí, pokud aplikace neřekne jinak.

### 12.4 Příchozí změna

Delta přinese nový stav položky, která je umístěním Q materializované entity. Všechno následující proběhne v téže transakci jako zápis stránky:

1. Kodek dekóduje surová data na nový pozorovaný stav. Předchozí pozorovaný stav je základ.
2. Rozdíl `staré → nové` se počítá **ve tvaru, který poskytovatel umí vyjádřit**, a rozdělí se podle tříd z kapitoly 12.3.
3. Pole umístění se jen uloží do `placement.local`. Plánovací pole se přijmou jen z hlavního umístění.
4. Sdílená pole se přenesou do entity pole po poli (funkce `lift`, kapitola 12.8):
   - pole změnil jen poskytovatel → přepíše se v entitě;
   - pole změnila jen entita (průmět entity se liší od `delivered`) → čeká na doručení, nic se neděje;
   - pole změnily obě strany → konflikt (kapitola 12.6).
5. Entita dostane novou revizi s poznamenaným původem Q.
6. Pro každé jiné umístění, jehož `delivered` se od nového průmětu entity liší, se naplánuje doručení.

Ochrana proti ozvěnám a smyčkám:

- **Stejný validátor je no-op.** Ozvěna vlastního zápisu se pozná dřív, než se cokoli dekóduje.
- **Prázdný rozdíl je no-op.** Poskytovatel někdy změní validátor beze změny obsahu. Pozorovaný stav se aktualizuje, entita ne.
- **Normalizace není změna.** Odpověď na zápis se stane novým pozorovaným stavem, i když se od `delivered` liší. Propisuje se jen rozdíl mezi dvěma po sobě jdoucími pozorovanými stavy téhož umístění.
- **Původ se vynechá.** Revize vzniklá z umístění Q se na Q neposílá.
- **Zapisuje se jen skutečný rozdíl.** Doručení porovná průmět entity s `delivered`. Pole, které už cíl podle pozorovaného stavu má, se neposílá.
- **Pojistka proti kmitání.** Když se totéž pole téže entity vrátí mezi dvěma umístěními víckrát za krátkou dobu (návrh: třikrát za 10 minut), propisování toho pole na dotčené umístění se zastaví. Umístění dostane stav `diverged` a aplikace záznam. Pojistka kryje případ, kdy dva poskytovatelé totéž pole opakovaně a různě přepisují.

Proč to konverguje: revize entity zapisuje jediný zapisovatel tenanta, takže jsou úplně uspořádané. Každé umístění se nezávisle blíží poslední revizi a příchozí změna se započítá právě jednou.

### 12.5 Zápis a rozeslání

Mutace míří na entitu, ne na položku.

1. **Jedna transakce:** neměnná mutace (idempotency klíč, výchozí revize, záplata), nová revize entity, záznam do žurnálu a **doručení** pro každé dotčené umístění.
2. **Doručení je stavové, ne záplatové:** „dostaň umístění P na aktuální revizi entity". Pracovník spočítá rozdíl mezi průmětem entity a `delivered`, promítne ho do posledního surového stavu položky a pošle podmíněný zápis. Několik revizí entity se tak samo sloučí do jednoho zápisu.
3. **Pořadí:** umístění se obsluhují souběžně, každé z nich sériově. Pořadí mezi umístěními není potřeba, s jedinou výjimkou u schůzek (brána hlavního umístění, kapitola 12.9).
4. **Výsledek se vyhodnotí pro každé umístění zvlášť:**
   - *Přijato:* uloží se vrácený surový stav a validátor, `delivered` se posune.
   - *Konflikt (412):* načte se aktuální stav a zpracuje se jako příchozí změna podle kapitoly 12.4, potom se doručení přepočítá. Konflikt při zápisu a příchozí změna jsou tentýž postup.
   - *Odmítnuto:* umístění přejde do stavu `failed` s důvodem.
   - *Nejisté:* pozorování, ne opakování (kapitola 3.5).
5. **Stav pro aplikaci** je souhrn: entita je `synced`, jen když jsou `synced` všechna umístění. Jinak nese nejhorší stav a seznam po umístěních.

**Částečné selhání.** Atomický zápis přes několik poskytovatelů neexistuje. Entita se potvrdí první a umístění ji dohánějí.

- Odmítne-li změnu **hlavní nebo jediné** umístění, mutace se vrátí.
- Odmítne-li ji **kopie nebo jedno z rovnocenných** umístění, entita platí dál a umístění zůstane `failed`.

Automatické vracení v druhém případě nedoporučuji. Znamenalo by kompenzační zápisy na umístění, která už uspěla, tedy další místa, kde se může něco pokazit. Aplikace má tři možnosti: opakovat, umístění odpojit, nebo poslat opačnou mutaci.

**Idempotence:**

- *Mutace:* idempotency klíč od aplikace.
- *Vytvoření na umístění:* vzdálené ID se rezervuje už v transakci přijetí: ID události zvolené klientem (Google, base32hex 5–1024 znaků), URL odvozené z ID umístění (CalDAV, CardDAV). `transactionId` u Graphu je jen pomocník pro rychlé opakování: okno deduplikace není dokumentované (T5), takže nejistý výsledek po delší době řeší pozorování podle obsahu a značky, ne nový POST.
- *People API a Google Tasks* klientské ID v dokumentaci nemají (ověřeno ve fázi 0; u Tasks to živě potvrdí T10). Nejistý výsledek se tam řeší pozorováním podle značky nebo obsahu.
- *Změna:* podmíněný zápis a stavové doručení, takže opakování je neškodné.
- *Pád mezi přijetím u poskytovatele a commitem:* po restartu přijde `412`, načte se stav, průmět se shoduje a umístění se jen označí jako `synced`.

Aplikace může čekat: `POST /v1/mutations?wait=primary` nebo `wait=all` s časovým limitem.

### 12.6 Souběžné úpravy

| Situace | Výsledek |
| --- | --- |
| Různá pole změněná na dvou místech | sloučí se obě změny |
| Stejné pole změněné na dvou místech | vyhrává změna, kterou jádro zpracuje později; poražená hodnota se uloží do záznamu o konfliktu a jde vrátit |
| Mutace aplikace a příchozí změna téhož pole | stejné pravidlo: příchozí změna vyhrává (shodné s revizí 2), aplikace může politiku změnit |
| Úprava na jednom místě, smazání na druhém | smazané umístění zaniká, úprava se propíše na zbylá. Platí to i s politikou `propagate`: úprava vyhrává nad smazáním (kapitola 12.7) |

- Výjimky opakování se slučují po výskytech. Klíčem je původní začátek výskytu, takže úprava dvou různých výskytů na dvou místech není konflikt.
- Seznamy (e-maily a telefony kontaktu) se slučují po prvcích podle normalizované hodnoty.

**Proč ne časová razítka poskytovatelů.** Hodiny různých poskytovatelů nejsou srovnatelné a `LAST-MODIFIED` v CalDAV nastavuje klient. Pořadí zpracování je deterministické a dá se uživateli vysvětlit.

**Omezení, které z toho plyne:** u poskytovatele bez pushe se o změně dozvíme se zpožděním, takže může vyhrát starší úprava jen proto, že jsme ji viděli později. Zmírnění: záznam o konfliktu s oběma hodnotami. Razítka poskytovatelů se nepoužívají ani tam, kde je nastavuje server (rozhodnuto).

### 12.7 Mazání

Rozhodnutí uživatele: výchozí stav zůstává „zaniká jen to umístění", ale **„smazat všude" musí být plnohodnotná volba**, aby po záznamu u poskytovatelů nic nezůstávalo.

| Co se stalo | Výsledek |
| --- | --- |
| Aplikace smaže entitu | smaže se na všech umístěních, vždy; u schůzky nejdřív na hlavním, kde poskytovatel rozešle zrušení. Schůzka s účastníky: viz „Smazání schůzky s účastníky" níže |
| Aplikace odebere umístění, `remote=delete` | položka u poskytovatele se smaže, entita žije dál |
| Aplikace odpojí umístění, `remote=keep` | vazba zanikne, položka zůstane a stane se samostatnou triviální entitou |
| Položka zmizí u poskytovatele (kopie nebo rovnocenné umístění) | podle politiky `on_remote_delete`: **`detach`** (výchozí) ukončí jen toto umístění; **`propagate`** smaže entitu a všechna její umístění. Kopie schůzky s účastníky zaniká vždy sama (pojistka 6) |
| Zmizí poslední umístění | entita dostane náhrobek s lhůtou na obnovu |
| U poskytovatele je zrušeno hlavní umístění schůzky | schůzka je zrušená: entita přejde do stavu „zrušeno" a kopie se smažou, bez ohledu na politiku |
| Smazán jeden výskyt série | není to smazání položky, ale úprava opakování; propíše se jako každá změna sdíleného pole |
| Účet vyžaduje nové přihlášení, ztratil přístup ke kolekci, nebo zmizela celá kolekce | umístění přejde do stavu `suspended`; **nic se nemaže a nic se nerozesílá** ani s `propagate` |
| Uživatel účet odebere | jeho umístění se odpojí a u poskytovatele se nemaže nic; entita s umístěním jinde žije dál |

**Kde se politika nastavuje.** Platí první nalezená hodnota:

1. výslovná hodnota na **entitě** (mutace `update` s `policy.on_remote_delete`);
2. hodnota **pravidla**, které vlastní umístění, kde ke smazání došlo (kapitola 13.8);
3. výchozí hodnota **tenanta**: `detach`, uživatel ji může přepnout na `propagate` pro všechno.

S `propagate` tedy platí: smazání v aplikaci nebo u kteréhokoli poskytovatele znamená, že záznam zmizí všude. Výjimkou jsou schůzky s účastníky.

**Pojistky pro `propagate`:**

1. **Jen výslovný důkaz:** náhrobek v deltě, nebo `404`/`410` při přímém čtení položky. Nikdy nepřítomnost ve čtení.
2. **Ztráta přístupu není smazání.** Odhlášený účet, odebrané oprávnění ani zmizelá kolekce rozeslání nespustí.
3. **Přesun není smazání.** Přesun položky do jiného kalendáře téhož účtu může v deltě vypadat jako smazání a vytvoření. Rozeslání proto počká na konec celého kola delty účtu a položky spáruje podle vzdáleného ID nebo UID. Graph nemá API pro přesun a s `ImmutableId` si ID přesun ve schránce drží; párování proto vyžaduje sledovat všechny kalendáře schránky. U Googlu se čeká `cancelled` ve zdroji a stejné `id` v cíli. Obojí potvrdí živý test T2 před fází 4.
4. **Pojistka hromadného mazání.** Víc smazání z jednoho účtu v krátké době (návrh: víc než 20 za 5 minut, nebo desetina kolekce) rozeslání pozdrží. Každá položka se ověří přímým čtením a aplikace dostane záznam. Teprve potom se rozesílá.
5. **Úprava vyhrává nad smazáním.** Mazání na ostatních umístěních je podmíněný zápis. Když ho poskytovatel odmítne, protože se položka mezitím změnila, rozeslání se zastaví, entita žije dál na zbylých umístěních a aplikace dostane záznam o konfliktu.
6. **Schůzky.** Smazání **kopie** schůzky, která má další účastníky, odstraní jen tu kopii. Schůzku nikdy nezruší ani neodmítne, ani s `propagate` (rozhodnuto). Aplikace dostane záznam.
7. **Lhůta na obnovu.** Náhrobek entity drží kanonický dokument a seznam bývalých umístění po omezenou dobu (návrh: 30 dní). `POST /v1/entities/{id}/restore` umístění založí znovu, s novými vzdálenými ID. U schůzky s účastníky by to znamenalo nové pozvánky, proto obnova vrací jen kopii bez účastníků.

**Smazání schůzky s účastníky** (rozhodnuto):

- **Organizátor.** Smazání entity schůzku ruší a účastníci dostanou zrušení. Mutace `delete` proto musí nést `notify_attendees: true`. Bez něj ji jádro odmítne typovanou chybou `attendees_will_be_notified` s počtem účastníků, aby se aplikace mohla uživatele zeptat.
- **Pozvaný.** Smazání entity znamená odmítnout a odebrat, jako v běžných kalendářích. Jádro pošle odmítnutí cestou hlavního umístění a potom odstraní entitu i její kopie.
- **Kopie.** Smazání kopie u poskytovatele odstraní jen tu kopii (pojistka 6).
- Odpověď na mutaci říká, co se stalo: `cancelled`, `declined` nebo `deleted`.

**Proč výchozí stav zůstává `detach`.** Nejnebezpečnější třída chyb v Musubi bylo mylně odvozené smazání. S více umístěními by se taková chyba rozšířila na všechny poskytovatele najednou. Kdo `propagate` zapne, dostává pohodlí za cenu této třídy rizika; pojistky ji zmenšují, ale neruší.

### 12.8 Rozdílné schopnosti poskytovatelů

Pravidlo: **pravdou je kanonický dokument entity. Žádné umístění ho nikdy nenahrazuje celý. Mění ho jen rozdíly.**

Mechanismus tvoří dvě čisté funkce kodeku:

- `project(entita, schopnosti)` vrátí průmět: co poskytovatel unese. U každého pole řekne `přesně`, `přibližně`, nebo `chybí`. Výsledek je v `placement.fidelity` a v API.
- `lift(entita, staré, nové)` přenese změnu pozorovanou u poskytovatele zpět do entity. Pole, které poskytovatel neunese, se v rozdílu nikdy neobjeví, takže ho umístění nemůže zničit.

Příklady:

1. **Google Tasks, termín jen jako datum.** Entita má termín 12. 11. v 15:00. Google drží 12. 11. Další čtení z Googlu vrací pořád 12. 11., rozdíl je prázdný a čas v entitě zůstává. Když uživatel v Google Tasks posune termín na 14. 11., rozdíl je „datum 12 → 14". `lift` změní datum a čas 15:00 zachová. Outlook a CalDAV dostanou 14. 11. v 15:00.
2. **Graph, opakování bez RDATE.** Entita má týdenní pravidlo a dva mimořádné termíny. Do Outlooku se promítne jen pravidlo a umístění je označené `přibližně: recurrence.rdate`. Když uživatel v Outlooku změní den v týdnu, `lift` přepíše pravidlo a mimořádné termíny v entitě zůstanou. Google a CalDAV dostanou nové pravidlo i s nimi.
3. **Popis.** Graph vrací HTML, CalDAV text. Porovnává se text po normalizaci. Rozdíl jen ve formátování není změna.

Tři stupně přísnosti při umisťování:

| Co je ztrátové | Chování |
| --- | --- |
| doplňková pole (popis, kategorie, formátování) | umístit a označit |
| pole, která mění, **kdy se něco děje** (opakování, čas, zóna) | umístění se odmítne typovanou chybou, pokud aplikace výslovně nepošle `allow_lossy`; pravidlo má vlastní `allow_lossy` a bez něj takový záznam přeskočí |
| druh, který poskytovatel nemá vůbec (úkol na iCloudu) | odmítnout |

Důvod druhého řádku: kalendář, který ukazuje jiné termíny než pravda, je horší než chybějící kopie.

Invarianty, které hlídají property testy v konformační sadě:

- čtení beze změny entitu nezmění;
- po `lift` se průmět entity ve vyjádřitelných polích rovná novému stavu u poskytovatele;
- pole označené `chybí` se nezmění žádnou posloupností příchozích změn z toho umístění.

Omezení první verze:

- **Jedna entita je na jednom umístění jedna položka.** Rozklad záznamu na víc položek u slabšího poskytovatele (série bez RDATE plus samostatné události pro mimořádné termíny) není.
- **Hierarchie podúkolů se mezi různými modely nepropisuje.** Google Tasks a CalDAV mají podúkol jako samostatnou položku s odkazem na rodiče, Graph jako vložený checklist. Vazba rodič–potomek se propisuje jen mezi poskytovateli se samostatnými položkami.
- **Opakované záznamy v Outlooku** závisí na živém testu T1 na začátku fáze 7 (kapitola 11.2).

### 12.9 Účastníci a pozvánky

Poskytovatel rozesílá pozvánky sám, jakmile organizátor uloží událost s účastníky:

- Google podle parametru `sendUpdates` (`all`, `externalOnly`, `none`); platí pro celý požadavek, ne pro jednotlivého účastníka;
- Graph při vytvoření události s účastníky; parametr pro potlačení nemá (ověřeno ve fázi 0);
- CalDAV server s implicitním plánováním podle RFC 6638. Parametr `SCHEDULE-AGENT` u účastníka umí pozvánku potlačit. iCloud ho podle jediné sekundární zkoušky ignoruje a při smazání neposílá CANCEL (T12), proto se k němu chováme jako ke Graphu.

Tatáž schůzka uložená s účastníky do tří kalendářů by znamenala tři pozvánky od tří organizátorů a tři oddělená vlákna odpovědí. Pravidla:

1. **Nejvýš jedno hlavní umístění** (`primary`) na entitu. Jen na něm jsou organizátor a účastníci a jen přes něj jde plánování.
2. **Ostatní umístění jsou soukromé kopie** (`copy`). Zapisují se bez organizátora a bez účastníků, takže je poskytovatel bere jako osobní událost a nic nerozesílá.
3. **Kopie má vlastní UID.** Stejné UID v kalendáři, kam může dorazit i skutečná pozvánka na tutéž schůzku, vede ke kolizi nebo k tomu, že server kopii s pozvánkou spáruje. Vazbu držíme my, ne UID.
4. **Brána hlavního umístění.** U entity s účastníky se změna nejdřív doručí na hlavní umístění a kopie se zapíšou až po jeho přijetí. Když hlavní umístění změnu odmítne, mutace se vrátí a kopií se nikdo nedotkl. Nenastane tak stav, kdy kopie ukazují čas, o kterém se účastníci nedozvěděli.
5. **Cizí pozvánka jako hlavní umístění.** Když nejsem organizátor, sdílená pole jsou jen pro čtení: pravdu určuje organizátor a kopie ji následují. Úprava sdíleného pole na kopii se vrátí přepsáním a aplikace dostane záznam. Odpověď na pozvánku jde jen přes hlavní umístění.
6. **Účastníci přidaní na kopii** v cizím klientovi se do entity nepřenesou. Poskytovatel už pozvánky rozeslal a automatické odebrání by rozeslalo zrušení. Umístění dostane stav `diverged` a rozhodne aplikace.
7. **Změna hlavního umístění** v první verzi není. Znamená zrušit schůzku u jednoho poskytovatele a znovu pozvat z druhého.
8. **Free/busy** počítá entitu jednou, i když má tři umístění.
9. **Účastník na stejném serveru** je zapsaný u poskytovatele jako každý jiný a pozvánku mu navíc doručí jádro. Pozvaný vidí jednu pozvánku (kapitoly 14.6 a 14.7). Příchozí pozvánka od jiného tenanta se chová jako cizí pozvánka podle pravidla 5 (kapitola 14).

Osobní událost bez účastníků má jako hlavní první umístění. Když aplikace později účastníky přidá, jdou jen tam. Úkoly a kontakty plánování nemají a všechna jejich umístění jsou rovnocenná (`peer`).

### 12.10 Připojení a odpojení existující položky

Každá položka už je triviální entita, takže připojení existující položky je **spojení dvou entit**.

- **Umístit do další kolekce:** `POST /v1/entities/{id}/placements { collection }`. U poskytovatele vznikne nová položka.
- **Připojit existující položku:** `POST /v1/entities/{id}/placements { item, truth: "entity" | "item" }`. Dvě entity nemají společný základ, takže se neslučuje po polích. Aplikace určí, která strana je pravda, a ta se zapíše na druhou.
- **Náhled:** totéž volání s `dry_run` vrátí rozdíl polí a seznam polí, která budou na novém umístění přibližná. Aplikace to ukáže uživateli před potvrzením.
- **Podmínky:** stejný druh, stejný tenant, zapisovatelná položka, nejvýš jedno umístění s účastníky.
- **Odpojit:** `DELETE /v1/entities/{id}/placements/{pid}?remote=keep`. Položka zůstane a dostane novou triviální entitu.
- **ID:** entita, ke které se připojuje, si ID nechá. Zanikající a vznikající ID oznámí proud změn (`merged_into`, `split_from`), aby si aplikace přepojily vlastní data.
- **Návrhy:** jádro umí nabídnout kandidáty (stejné UID ve dvou vlastních účtech, shodný název a čas, shodný e-mail kontaktu). Samo nepropojuje nikdy, s jedinou výjimkou pozvánky doručené dvěma cestami (kapitola 14.7).

**Značka u poskytovatele.** Do položky, která je umístěním materializované entity, zapíšeme ID entity do skrytého pole:

- Google: `extendedProperties.private`;
- Graph: rozšířená vlastnost nebo otevřené rozšíření (obojí dokumentace u událostí uvádí);
- CalDAV a CardDAV: X- vlastnost;
- People API: `clientData`;
- Google Tasks takové pole nemá (ověřeno ve fázi 0). People API ho má (`clientData`), Google Calendar `extendedProperties.private`.

Značka slouží k obnově vazeb po ztrátě databáze, k rozpoznání vlastní kopie viděné přes jiný účet a k tomu, aby se kopie pravidla nestala zdrojem jiného pravidla (kapitola 13.3). Nese jen neprůhledné ID entity. Uživatel zápis značky povolil. Nevýhoda zůstává: vidí ji jiné aplikace s přístupem k témuž kalendáři.

### 12.11 API

```
GET    /v1/entities?kind=event&from=…&to=…        jeden řádek na záznam
GET    /v1/entities/{id}?include=placements        umístění, jejich stav a věrnost
POST   /v1/mutations                               create { kind, data, placements: [{collection, role}], policy } | update | delete { notify_attendees }
POST   /v1/entities/{id}/placements                { collection } nebo { item, truth }; volitelně dry_run, allow_lossy
PATCH  /v1/entities/{id}/placements/{pid}          pole umístění (připomenutí, barva, odpověď)
DELETE /v1/entities/{id}/placements/{pid}?remote=delete|keep
GET    /v1/entities/{id}/conflicts                 poražené hodnoty; vrátit je jde běžnou mutací
GET    /v1/link-suggestions                        kandidáti na propojení
POST   /v1/entities/{id}/restore                   obnova z náhrobku ve lhůtě (kapitola 12.7)
PATCH  /v1/settings                                výchozí hodnoty tenanta, například on_remote_delete
```

Proud změn nese změnu entity s původem, změnu stavu umístění, `merged_into`, `split_from`, záznam o konfliktu a pozdržené hromadné smazání. Volání pro pravidla jsou v kapitole 13.9, pro plánování v kapitole 14.11.

### 12.12 Důsledek: primitivum drží autorská data

Revize 2 popisovala musubi-sync jako cache s úmysly. To už platí jen pro triviální entity. Nenahraditelné jsou:

- kanonický dokument materializované entity, hlavně pole, která žádné umístění neunese;
- vazby entita–umístění a role umístění;
- pole umístění, která poskytovatel nedrží;
- záznamy o konfliktech a nedoručené mutace;
- pravidla synchronizace a jejich výjimky;
- odchozí plánovací zprávy a stav účastníků, které zve jádro.

Dopad na úložiště, trvanlivost a zálohy je v kapitolách 4.3 a 4.4.

### 12.13 Zamítnuté alternativy

- **Párová synchronizace** (A↔B, B↔C) jako vdirsyncer. N² vazeb, smyčky mezi páry a žádné místo pro pravdu bohatší než nejslabší poskytovatel.
- **Jedno umístění jako pán, ostatní jako zrcadla.** Jednodušší, ale pravdu by omezily schopnosti pána a úpravy na zrcadlech by se ztrácely. Používáme to jen tam, kde to vynucuje plánování (pravidlo 5 v kapitole 12.9).
- **CRDT.** Poskytovatelé jím nemluví. Konflikt se stejně řeší podmíněným zápisem.
- **Stejné UID na všech umístěních** a korelaci nechat na poskytovatelích. Koliduje se skutečnými pozvánkami a u úkolů a kontaktů nepomůže.
- **Sloučit položky do jednoho řádku.** Ztratili bychom validátory a základ pro slučování každého umístění.
- **Záplatové doručení** (každému umístění poslat tutéž záplatu). Po částečném selhání a souběžné změně se umístění rozejdou. Stavové doručení konverguje.
- **Automatické propojování podle shody.** Chybné propojení přepíše cizí data. Jen návrhy.
- **Propisovat smazání vždy.** Je to volba (`propagate`), ne výchozí stav. Viz kapitola 12.7.

### 12.14 Rozhodnutí k tomuto modelu

Devět otázek z revize 3 uživatel rozhodl 11. 11. 2026:

| Otázka | Rozhodnutí | Kde |
| --- | --- | --- |
| 1. Smazání na jednom místě | výchozí „jen to umístění"; „smazat všude" jako nastavení na entitě i na pravidle | kapitola 12.7 |
| 2. Konflikt stejného pole | vyhrává později zpracovaná změna, vzniká záznam o konfliktu; razítka poskytovatelů se nepoužívají | kapitola 12.6 |
| 3. Připomenutí | pole umístění | kapitola 12.3 |
| 4. Pravidla automatického umisťování | v jádru | kapitola 13 |
| 5. Maska umístění | průmět pravidla, v rozsahu | kapitola 13.5 |
| 6. Ztrátové umístění času a opakování | odmítnout bez `allow_lossy` | kapitola 12.8 |
| 7. Úprava kopie cizí pozvánky | vrátit přepsáním | kapitola 12.9 |
| 8. Sdílený tenant domácnosti | zrušen: tenant je jeden člověk, lidé spolu jednají pozvánkami | kapitola 14 |
| 9. Značka u poskytovatele | povolena | kapitola 12.10 |

Co z modelu zůstává otevřené, je v kapitole 11.4.

---

## 13. Pravidla synchronizace

Rozhodnutí uživatele: pravidla typu „zrcadli kalendář A do B" žijí **v jádru**, ne v aplikaci. Běží, i když žádná aplikace neběží.

Pravidlo nepřidává druhý synchronizační mechanismus. Rozhoduje jen o tom, **která umístění mají existovat**. Doručení, slučování, konflikty a ochranu proti smyčkám dál dělá model z kapitoly 12.

### 13.1 Model pravidla

```
Rule {
  id, tenant, name, enabled, rev,
  source:     { collections: [id], filter },
  target:     { collection },
  direction:  one_way | two_way,
  projection: full | busy_only { title },
  on_remote_delete: detach | propagate,    // jen two_way
  on_target_delete: exclude | recreate,    // jen one_way
  allow_lossy: bool,
  state: backfilling | active | paused | suspended
}
```

| Část | Význam |
| --- | --- |
| **Rozsah** (`source`) | kolekce, ze kterých pravidlo bere záznamy, a filtr nad nimi |
| **Cíl** (`target`) | kolekce, kde má mít každý zahrnutý záznam umístění |
| **Směr** | `one_way`: cíl jen přijímá. `two_way`: dvě kolekce jsou si rovny a záznam vzniklý v kterékoli z nich se umístí i do druhé |
| **Průmět** | `full`: celý záznam. `busy_only`: maska „Obsazeno" (kapitola 13.5) |
| **Politika mazání** | co se stane, když kopie nebo zdroj zmizí u poskytovatele (kapitola 13.8) |
| **`allow_lossy`** | souhlas se ztrátovým umístěním času a opakování pro celé pravidlo (kapitola 12.8). Bez něj pravidlo takový záznam přeskočí a vykáže ho |

**Filtr** je malá deklarativní sada podmínek nad kanonickými poli. Podmínky se spojují „a zároveň":

| Podmínka | Hodnoty | Výchozí |
| --- | --- | --- |
| `kinds` | událost, úkol, kontakt | podle druhu cílové kolekce |
| `time` | `from_today`, `all`, pevné okno | `from_today`: záznamy, které končí dnes nebo později, a série s budoucím výskytem |
| `busy` | jen obsazeno, vše | vše; u masky jen obsazeno |
| `participation` | vše, jsem organizátor, neodmítnuté, jen přijaté | neodmítnuté |
| `visibility` | vše, bez soukromých | vše |
| `all_day` | vše, bez celodenních, jen celodenní | vše |
| `categories` | výčet | bez omezení |

Záznam, který z okna `from_today` vypadne jen plynutím času, se nemaže. Jinak by pravidlo každý den mazalo včerejší kopie.

Umístění dostává tři nová pole (kapitola 12.2):

- `direction: both | out`. Umístění `out` jen přijímá. Změna sdíleného pole na něm se do entity nepřenese: vrátí se přepsáním a aplikace dostane záznam. Je to stejný mechanismus jako u kopie cizí pozvánky (kapitola 12.9, pravidlo 5). Pole umístění (připomenutí, barva) si cíl drží vlastní.
- `projection: full | busy_only`.
- `origin: manual | rule` a seznam pravidel, která umístění vlastní.

### 13.2 Vyhodnocení

`evaluate(pravidla, entita) → umístění, která mají podle pravidel existovat` je čistá funkce bez I/O.

- Spouští ji jediný zapisovatel tenanta **v téže transakci** jako každou novou revizi entity a každou revizi pravidla.
- Výsledek porovná s umístěními, která pravidla už vlastní:
  - chybí → vznikne umístění a jeho doručení, s předem přiděleným vzdáleným ID (kapitola 12.5);
  - přebývá (záznam přestal filtru vyhovovat, například jsem pozvánku odmítl) → umístění se odebere a položka u poskytovatele smaže;
  - změnil se průmět → přepočítá se a stavové doručení pošle jen rozdíl.
- Zdrojem pravidla je entita, která má ve zdrojové kolekci umístění s `origin: manual`. Patří sem i příchozí pozvánky (kapitola 14.5).

Žádná aplikace u toho být nemusí: vyhodnocení je součást příjmu změny.

### 13.3 Invarianty a smyčky

1. **V jedné kolekci má entita nejvýš jedno umístění.**
2. **Umístění vytvořené pravidlem není zdrojem žádného pravidla.** Pravidla nejsou tranzitivní.
3. **Vlastní kopie se pozná podle značky** (kapitola 12.10), i když ji jádro vidí přes jiný účet téhož tenanta (sdílený kalendář). Vede se jako alias existujícího umístění, ne jako nový záznam, a zdrojem není.
4. **Pravidlo nevytvoří kopii v účtu, kde tatáž schůzka už je.** Rozhoduje korelační klíč (UID). Příklad: na poradu jsem pozvaný pracovní i soukromou adresou a pravidlo zrcadlí pracovní kalendář do soukromého.
5. **Dvě pravidla se stejným cílem** pro jednu entitu sdílejí jedno umístění. Platí přísnější z obou: `out` před `both`, `busy_only` před `full`. Umístění zanikne, až přestane vyhovovat poslednímu z nich.

Co z toho plyne:

| Sestava | Výsledek |
| --- | --- |
| A → B a zároveň B → A | záznam z A má kopii v B, záznam z B má kopii v A; kopie nejsou zdrojem, takže nic dalšího nevznikne |
| A ⇄ B (jedno obousměrné pravidlo) | totéž, navíc se úpravy na kopiích propisují zpět |
| A → B, B → C | záznam z A je v B, **ne** v C. Kdo ho chce i v C, přidá pravidlo A → C |
| A → C a B → C | C obsahuje kopie z obou; žádná se nevrací |

Proč to konverguje: vstupem vyhodnocení jsou jen ruční umístění entity, výstupem jen umístění pravidel. Výstup se nikdy nestane vstupem, takže opakované vyhodnocení dá stejný výsledek.

Proč ne tranzitivita: výsledek by závisel na pořadí pravidel, odebrání pravidla uprostřed řetězu by nemělo jasný význam a záznam by se dostal dál, než uživatel čekal (osobní událost přes pracovní kalendář do sdíleného týmového).

Pojistky:

- **Hromadné vytváření.** Pravidlo, které by mimo úvodní dosyp vytvořilo víc než N kopií v krátké době (návrh: 200 za 10 minut), přejde do stavu `paused` a aplikace dostane záznam.
- **Kmitání** (kapitola 12.4) a **hromadné mazání** (kapitola 12.7) platí beze změny.

### 13.4 Jednosměrné a obousměrné pravidlo

| | `one_way` (A → B) | `two_way` (A ⇄ B) |
| --- | --- | --- |
| Zdroj | jedna nebo víc kolekcí | právě jedna kolekce na každé straně, stejného druhu |
| Úprava kopie v cíli | vrátí se přepsáním, aplikace dostane záznam | propíše se do entity a na druhou stranu |
| Průmět | `full` nebo `busy_only` | jen `full` |
| Materializace entity | ne: pravdou zůstává zdroj a kopie jsou odvozená data | ano (kapitola 12.2) |
| Událost s účastníky | kopie bez účastníků s vlastním UID | stejně; hlavní umístění zůstává, kde bylo |
| Cizí pozvánka | kopie následuje organizátora | stejně; úprava kopie se vrací (kapitola 12.9) |
| Úkoly a kontakty | rovnocenné umístění se směrem `out` | rovnocenné umístění |

Jednosměrné kopie nejsou autorská data. Po ztrátě databáze je obnoví nové vyhodnocení pravidla a značky u poskytovatelů zabrání duplicitám. Kde poskytovatel značku neunese (Google Tasks, ověřeno ve fázi 0), duplicita po ztrátě databáze hrozí.

### 13.5 Maska „Obsazeno"

Maska je průmět pravidla, ne zvláštní funkce.

| Přenáší se | Nepřenáší se |
| --- | --- |
| začátek, konec, zóna, celodenní příznak | název (nahradí ho pevný text z pravidla, výchozí „Obsazeno") |
| opakování a ty výjimky, které mění čas nebo ruší výskyt | popis, místo, účastníci, konference, kategorie, přílohy |
| stav obsazeno | připomenutí |

- Maska je vždy jednosměrná.
- Události označené jako volno se přeskočí.
- Kde to poskytovatel umí, kopie se zapíše jako soukromá.
- Čas zůstává přesný, takže `allow_lossy` není potřeba.
- Výjimka série, která mění jen název nebo popis, v kopii nic nezmění.
- Značka u poskytovatele nese jen neprůhledné ID entity.

Konformační test: žádné textové pole zdroje se neobjeví v surových datech maskovaného umístění, ani po úpravě zdroje, ani po změně průmětu z `full` na `busy_only`.

### 13.6 Založení pravidla a dosyp

- **Náhled.** `POST /v1/rules?dry_run=true` vrátí počty: kolik kopií vznikne, kolik záznamů se přeskočí kvůli ztrátovosti, kolik jich v cílovém účtu už je (invariant 4) a zda je cílová kolekce sdílená s dalšími lidmi. U obousměrného pravidla počty pro oba směry a počet pravděpodobných duplicit.
- **Pravděpodobné duplicity** (shodný název a čas na obou stranách) se nikdy nepropojují samy. Dokud uživatel nerozhodne, jsou z pravidla vyjmuté. Jinak by obousměrné pravidlo nad dvěma kalendáři se stejným obsahem každý záznam zdvojilo. Možnosti: propojit (kapitola 12.10, s volbou strany pravdy), nebo zkopírovat i tak.
- **Dosyp** běží jako úloha po dávkách v pruhu pro práci na pozadí (kapitola 5.3). Pravidlo je ve stavu `backfilling` a postup hlásí proud změn.
- **Souhlas.** Pravidlo vyžaduje zapisovací scope cílového účtu. Jeho založení je výslovný souhlas s automatickým zápisem do tohoto účtu.

### 13.7 Změna, vypnutí a odebrání

Změna pravidla je nová revize pravidla a po ní nové vyhodnocení všech dotčených entit.

| Akce | Co se stane s umístěními pravidla |
| --- | --- |
| Změna filtru | přibudou chybějící; ta, která už nevyhovují, se u poskytovatele smažou |
| Změna průmětu | přepočet a doručení rozdílu; `full → busy_only` obsah z kopií odstraní |
| Změna cíle | staré kopie se smažou a v novém cíli vzniknou nové |
| `one_way → two_way` | umístění dostanou směr `both`, entity se materializují a opačný směr se dosype s náhledem |
| `two_way → one_way` | kopie v cíli dostanou směr `out`; záznamy vzniklé v cíli si kopii ve zdroji nechají jako ruční umístění, nic se nemaže |
| Vypnutí (`enabled: false`) | umístění přejdou do stavu `suspended`; nic se nemaže ani nepropisuje. Po zapnutí se dorovnají |
| Cílový nebo zdrojový účet vyžaduje přihlášení | pravidlo je `suspended` a po obnově přístupu se dorovná |
| Odebrání účtu nebo kolekce | pravidlo je `suspended` s důvodem; umístění se odpojí podle kapitoly 12.7 a u poskytovatele se nemaže nic |
| **Odebrání pravidla** | podle povinného parametru `placements`, viz níže |

`DELETE /v1/rules/{id}?placements=delete|keep|detach`. Parametr je povinný a `dry_run` vrátí počty.

| Volba | Výsledek | Kdy |
| --- | --- | --- |
| `delete` | kopie, které pravidlo vytvořilo, se u poskytovatele smažou; originály zůstanou | doporučeno pro jednosměrná a maskovaná pravidla: nic nezůstane ležet |
| `keep` | umístění zůstanou jako ruční a obousměrná; záznam je dál jeden | uživatel chce propojení zachovat bez pravidla. U masky nejde |
| `detach` | vazby zaniknou, položky u poskytovatele zůstanou jako samostatné záznamy a značka se odstraní | uživatel chce kopie ponechat jako obyčejné události |

### 13.8 Mazání pod pravidly

| Kde položka zmizela | `one_way` | `two_way` |
| --- | --- | --- |
| Ve zdroji | kopie se smažou vždy: bez zdroje nemají pravdu, kterou by následovaly | podle `on_remote_delete` |
| V cíli | podle `on_target_delete`: `exclude` (výchozí) umístění ukončí a záznam z pravidla vyjme, takže se kopie nevrátí; `recreate` ji při dalším vyhodnocení založí znovu | podle `on_remote_delete` |

`on_remote_delete` u obousměrného pravidla:

- `detach` (výchozí): zaniká jen smazané umístění. Zbylé se změní na ruční a záznam se z pravidla vyjme, aby ho pravidlo nezaložilo zpátky.
- `propagate`: záznam se smaže na obou stranách, s pojistkami z kapitoly 12.7.

Jednosměrné pravidlo `propagate` nemá. Cíl zdroj nemění, tedy ho ani nemaže.

Pro všechna rozeslání smazání platí pravidlo z kapitoly 12.7: jen na výslovný důkaz. Smazání odvozené z úplného čtení se před rozesláním ověří přímým čtením položky.

### 13.9 API

```
GET    /v1/rules
POST   /v1/rules                        založení; ?dry_run=true vrátí jen počty
GET    /v1/rules/{id}                   stav, postup dosypu, počty, přeskočené záznamy s důvodem
PATCH  /v1/rules/{id}                   nová revize pravidla; dry_run
DELETE /v1/rules/{id}?placements=delete|keep|detach
GET    /v1/rules/{id}/placements
POST   /v1/rules/{id}/exclusions        { entity }: záznam z pravidla vyjmout
DELETE /v1/rules/{id}/exclusions/{entity}
```

Proud změn nese stav pravidla, postup dosypu, pozastavení pojistkou a přeskočené záznamy.

### 13.10 Bezpečnost a cena

- **Automatický zápis do cizích kalendářů.** Plné zrcadlení do kalendáře sdíleného s kolegy zveřejní osobní data. Náhled proto hlásí, že cílová kolekce je sdílená, a aplikace má v tom případě nabídnout masku.
- **Kvóty.** Každé pravidlo násobí zápisy. Dosyp běží v pruhu na pozadí a metriky se vedou po pravidlech.
- **Rozsah tokenů.** Token, který smí spravovat pravidla, je samostatné oprávnění. Token jen pro čtení pravidla nezakládá.

### 13.11 Zamítnuté alternativy

- **Pravidla v aplikaci nad proudem změn** (doporučení revize 3). Zamítnuto rozhodnutím uživatele: nefungovala by bez běžící aplikace a každá aplikace by je psala znovu.
- **Skriptovací nebo výrazový jazyk pro filtry.** Nejde staticky ověřit, nejde k němu spočítat náhled a je to plocha pro chyby i útoky. Pevná sada podmínek stačí.
- **Tranzitivní pravidla.** Viz kapitola 13.3.
- **Synchronizace dvojice kolekcí mimo model entity** (jako vdirsyncer). Byly by to dva mechanismy se dvěma sadami chyb.
- **Automatické propojení shodných záznamů při založení pravidla.** Chybné propojení přepíše cizí data. Jen návrhy.
- **Obousměrná maska.** Kopie bez obsahu nemá co vracet.

---

## 14. Plánování mezi tenanty

Rozhodnutí uživatele: **tenant je jeden člověk.** Sdílené tenanty domácností nejsou. Lidé na stejném serveru spolu jednají pozvánkami, které doručuje jádro.

### 14.1 Princip

- Entitu vlastní **tenant organizátora**.
- Tenant pozvaného dostane **příchozí pozvánku**: vlastní entitu, jejíž sdílená pole určuje organizátor. Má u ní vlastní odpověď a vlastní připomenutí a může ji umístit do svých kalendářů u poskytovatelů.
- Mezi tenanty putuje jen **zpráva**: trvalý outbox odesílatele → inbox příjemce. Nikdo nečte cizí úložiště.
- Sémantika je iTIP (RFC 5546) tak, jak ji používá plánování v CalDAV (RFC 6638): metody REQUEST, REPLY a CANCEL, pořadí podle SEQUENCE.

Příklad: Anna pozve Borise, oba jsou na stejném serveru. Annina událost žije v jejím kalendáři Google. Boris ji má během desítek milisekund (odhad) ve své schránce pozvánek a jeho pravidlo ji zrcadlí do jeho iCloudu. Když Anna schůzku posune, posune se u Borise všude. Když Boris přijme, Anna vidí jeho odpověď.

### 14.2 Adresy

Tenant má **plánovací adresy**. Jsou to e-mailové adresy ve tvaru `mailto:`, stejně jako v iCalendaru.

| Zdroj adresy | Ověření |
| --- | --- |
| e-mail připojeného účtu Google nebo Microsoft | ověřený údaj z přihlášení přes OAuth |
| adresy účtu iCloud | vlastnost `calendar-user-address-set` ze serveru Applu **(neověřeno, T12 krok 1)**. iCloud podle sekundárního zdroje přepisuje vlastní adresy v `ORGANIZER` a `ATTENDEE` na cestu principalu s parametrem `EMAIL=`; normalizace adres ji převede na e-mail a považuje za vlastní adresu |
| místní identita `jmeno@domena-serveru` | přiděluje instance |
| e-mail z tokenu důvěryhodného IdP s `email_verified` | ověřil ho IdP provozovatele; zapíná konfigurace |
| cokoli jiného, včetně adres z obecného CalDAV serveru | **neplatí samo.** CalDAV server si zvolil uživatel a může o sobě tvrdit libovolnou adresu. Adresu musí potvrdit provozovatel nebo aplikace správcovským voláním |

- **Adresář** `adresa → tenant` je v řídicí rovině. Ukládá klíčovaný hash adresy (HMAC klíčem instance), ne adresu samotnou.
- Adresa patří nejvýš jednomu tenantovi. Platí první ověřený nárok; druhý se odmítne a ohlásí provozovateli.
- Tenant si nastaví, zda je **dohledatelný**. Nedohledatelný se navenek chová jako vnější adresa.

### 14.3 Zprávy

```
SchedMsg { id, method: REQUEST | REPLY | CANCEL | REFRESH,
           uid, sequence, dtstamp,
           organizer, recipient, recurrence_id?,
           payload }        // u REQUEST celý sdílený stav schůzky včetně série a výjimek
```

| Pravidlo | Proč |
| --- | --- |
| Příjem je idempotentní podle `id` | doručení je „aspoň jednou" |
| Pořadí určuje `sequence` a `dtstamp`, ne pořadí příchodu | zprávy se mohou předběhnout |
| Zpráva s nižší `sequence`, než příjemce zná, se zahodí | starý stav nepřepíše nový |
| Podstatná změna (čas, opakování, místo) zvýší `sequence` a vrátí odpovědi na „čeká" | účastník souhlasil s něčím jiným |
| REQUEST je stavový: nese celý aktuální stav | několik změn se sloučí do jedné zprávy; ztracená zpráva se nahradí další |
| První REQUEST sváže dvojici (adresa organizátora, UID) s tenantem odesílatele | další zprávy k témuž UID od jiného tenanta se odmítnou: cizí tenant nemůže schůzku změnit ani zrušit |
| REPLY se přijme jen od tenanta, jehož adresa je v seznamu účastníků | nikdo neodpovídá za jiného |
| Odpověď na jeden výskyt nese `recurrence_id` (původní začátek) | shodné s kapitolou 12.6 |

REFRESH je vnitřní zpráva pro dorovnání po obnově ze zálohy (kapitola 14.8).

**Mimo první verzi:** protinávrh času (COUNTER, DECLINECOUNTER), pozvání jen na jeden výskyt série, ADD, dotaz na free/busy (kapitola 14.9) a delegace.

### 14.4 Strana organizátora

- Účastník v entitě nese `{ address, name, role, status, channel, delivery }`.
  - `channel: core | both | provider | none` říká, kdo mu pozvánku doručuje (kapitola 14.6).
  - `delivery: pending | delivered | undeliverable` platí pro doručení jádrem.
- Zprávy se zapisují do outboxu **ve stejné transakci** jako mutace entity. Buď existuje změna i zpráva, nebo nic.
- Entita s místním účastníkem je vždy materializovaná: nese stav doručení, který žádné umístění nemá.
- Odpověď účastníka (REPLY) změní jeho `status` v entitě. Ostatní místní účastníci ji dostanou v dalším stavovém REQUEST, bez zvýšení `sequence`. Jádro ji zapíše i do položky u poskytovatele, kde to poskytovatel dovolí (kapitola 14.6).
- Odebrání účastníka pošle CANCEL jen jemu. Smazání entity pošle CANCEL všem. Smazání vyžaduje výslovné `notify_attendees: true` (kapitola 12.7).

Tady engine poprvé ví víc než „uloženo": u místního účastníka zná stav doručení do schránky.

### 14.5 Strana pozvaného

**Interní kolekce.** Jádro dostává vestavěného poskytovatele `local`. Jeho kolekce a položky žijí jen v jádru. Každý tenant má interní kolekci **Pozvánky**. Surovou vrstvou položky je poslední přijatý REQUEST.

Příchozí pozvánka je entita, jejíž **hlavní umístění je položka ve schránce pozvánek**. Od té chvíle platí beze změny pravidlo „cizí pozvánka jako hlavní umístění" (kapitola 12.9, pravidlo 5):

| Co | Chování |
| --- | --- |
| Sdílená pole (název, čas, místo, popis) | jen pro čtení; určuje je organizátor |
| Odpověď | pole hlavního umístění. Mění se stejným voláním jako odpověď na pozvánku od poskytovatele; jádro z ní udělá REPLY |
| Připomenutí | pole každého umístění zvlášť (kapitola 12.3) |
| Umístění do vlastních kalendářů | ručně, nebo pravidlem se schránkou pozvánek jako zdrojem (kapitola 13). Kopie jsou bez účastníků a s vlastním UID |
| Úprava sdíleného pole na kopii | vrátí se přepsáním (kapitola 12.9) |
| CANCEL | entita přejde do stavu „zrušeno" a kopie se smažou (kapitola 12.7) |
| Odmítnutí | výchozí filtr pravidel „neodmítnuté" kopie odstraní |
| Smazání pozvánky pozvaným | odmítnout a odebrat: organizátor dostane odmítnutí, entita i kopie zmizí (kapitola 12.7) |

Nic nového tedy na straně příjemce nevzniká: je to existující model s jedním dalším „poskytovatelem".

### 14.6 Soužití s pozvánkami poskytovatelů

Rozhodnuto, s cílem co největšího pohodlí: **pozvaný vidí jednu pozvánku, úplný seznam účastníků vidí všichni a nikdo nedostává zbytečnou poštu.**

Místní účastník se proto zapisuje **také do seznamu účastníků u poskytovatele** organizátora. Vidí ho tak vnější účastníci i organizátor ve svém klientovi. Jádro mu k tomu doručí pozvánku do schránky: je to rychlejší a funguje to i tehdy, když pozvaný nemá u jádra připojený kalendář té adresy.

„Místní" znamená, že adresa patří dohledatelnému tenantovi na tomto serveru.

| Účastník | Hlavní umístění organizátora | Zápis u poskytovatele | Kdo doručuje | Co pozvaný vidí |
| --- | --- | --- | --- | --- |
| místní | CalDAV server s plánováním (RFC 6638), který `SCHEDULE-AGENT` dodržuje (iCloud ne, viz níže) | ano, s `SCHEDULE-AGENT=CLIENT` | jen jádro | jednu pozvánku, žádný e-mail |
| místní | Google nebo Microsoft | ano | jádro i poskytovatel | jednu pozvánku: obě doručení se u něj spojí (kapitola 14.7). E-mail od poskytovatele přijde |
| místní | kolekce bez plánování (interní kolekce, CalDAV bez RFC 6638) | ano; server bez plánování nic nerozesílá | jen jádro | jednu pozvánku |
| vnější | poskytovatel s plánováním | ano | poskytovatel e-mailem | pozvánku od poskytovatele |
| vnější | kolekce bez plánování | — | nikdo | nic: mutace se odmítne typovanou chybou `no_scheduling_channel` s radou umístit záznam do kalendáře s plánováním |

- Platí to i pro účastníka, kterého organizátor přidal přímo v klientovi poskytovatele. Jakmile ho jádro v položce uvidí, doručí mu pozvánku do schránky také.
- Účastník v entitě nese `channel: core | both | provider | none` podle sloupce „Kdo doručuje". Odpověď API na mutaci ho uvádí.
- **Vlastní e-mailové pozvánky (iMIP) v první verzi nejsou** (rozhodnuto). Patří k doméně e-mailu.

**Potlačení pošty poskytovatele:**

- CalDAV: parametr `SCHEDULE-AGENT=CLIENT` u účastníka říká serveru, aby ho nezval (RFC 6638, ověřeno). Odpověď tohoto účastníka pak do položky zapisuje jádro. iCloud ho podle jediné sekundární zkoušky ignoruje (T12), proto má profil `suppress_per_attendee: false` a místní účastník se k němu zapisuje jako u Graphu.
- Google: `sendUpdates` platí pro celý požadavek, ne pro jednotlivého účastníka (ověřeno). Zda se událost účastníkovi s účtem Google objeví v kalendáři i bez e-mailu, řídí jeho nastavení „Add invitations to my calendar" (ověřeno ve fázi 0).
- Graph: potlačení nemá (ověřeno ve fázi 0). Obchvat přes `isDraft` a MAPI vlastnosti zamítáme. EWS končí 1. 4. 2027.
- Ke zkoušce v živém testu T9 (před fází 3): přidat místního účastníka u Googlu samostatným požadavkem se `sendUpdates=none`. Ušetřilo by to první e-mail; pozdější změny schůzky by mu Google oznámil stejně. Návrh na tom nestojí.

Návrh je bezpečný i tam, kde potlačení selže. Když server parametr ignoruje, pozvánka přijde dvěma cestami a příjemce si ji spojí. Selhání znamená e-mail navíc, ne druhou pozvánku v kalendáři.

**Jedna cesta odpovědi.** Odpověď jde vždy přes hlavní umístění pozvánky u pozvaného:

| Hlavní umístění u pozvaného | Cesta odpovědi | Jak se dostane k ostatním |
| --- | --- | --- |
| položka ve schránce jádra | REPLY jádrem | jádro organizátora zapíše stav účastníka do položky u poskytovatele, takže ho vidí i vnější účastníci |
| položka u poskytovatele (po spojení, kapitola 14.7) | přes poskytovatele | poskytovatel ji doručí organizátorovi a jádro organizátora ji přečte z jeho položky. REPLY jádrem se neposílá |

Stav účastníka v entitě organizátora je jedno pole. Obě cesty do něj ústí a pro téhož účastníka nikdy neběží zároveň.

**Zápis odpovědi organizátorem:** u Googlu je `attendees[].responseStatus` zapisovatelné (ověřeno ve fázi 0), ale zápis může účastníkům s některými nastaveními vrátit odpověď na `needsAction` a nad 200 pozvaných se nepropaguje (T9 krok 4). U Graphu to dokumentace neříká, výchozí schopnost je `false` (T4). Kde to nejde, vidí odpověď místního účastníka místní lidé a aplikace, ale vnější účastníci u něj vidí „čeká". Schopnost je v popisu adaptéru (kapitola 6.2).

**Cena, kterou je poctivé říct:**

- U Googlu a Microsoftu dostane místní účastník vedle pozvánky v jádru i e-mail poskytovatele. Pozvánku v kalendáři má jednu.
- Místní účastník, který nemá u jádra připojený účet té adresy, může odpovědět dvakrát různě: v aplikaci a odkazem v e-mailu. Platí později zpracovaná odpověď.

### 14.7 Když táž schůzka přijde dvěma cestami

U Googlu a Microsoftu je to běžný stav, ne výjimka: pozvánka dorazí do schránky jádra a přes poskytovatele i do kalendáře pozvaného, pokud ho má u jádra připojený.

Pravidlo na straně příjemce: **v tenantu existuje nejvýš jedna pozvánka pro dvojici (adresa organizátora, UID).** REQUEST nese UID hlavního umístění organizátora, tedy totéž UID, pod kterým schůzku rozesílá jeho poskytovatel. U Graphu se dvojice opírá o `iCalUId`; dokumentace jeho shodu mezi schránkami netvrdí a ověří ji živý test T1 krok 5 před fází 7 (kapitola 11.2, bod 4).

Když má tenant položku ve schránce i položku u poskytovatele se stejnou dvojicí:

1. Spojí se do jedné entity. Je to jediná výjimka z pravidla „jádro samo nepropojuje" a opírá se o přesnou identitu iTIP, ne o podobnost.
2. Hlavním umístěním se stane **položka u poskytovatele**. Vidí ji ostatní klienti uživatele a odpověď přes ni dojde organizátorovi i jeho poskytovateli. Je-li účet připojený jen ke čtení, zůstává hlavní položka ve schránce; položka u poskytovatele se k entitě jen připojí a jádro do ní nezapisuje.
3. Položka ve schránce dostane náhrobek `superseded`. Pole umístění se přenesou. Pokud uživatel už odpověděl ve schránce a u poskytovatele je stav „čeká", odpověď se tam zapíše.
4. Kopie, které pravidla mezitím vytvořila v účtu, kde je teď skutečná pozvánka, se smažou (invariant 4 v kapitole 13.3).
5. Proud změn ohlásí `merged_into`.
6. Na pořadí nezáleží. Když dřív přijde položka od poskytovatele, REQUEST se k ní jen přiřadí a položka ve schránce vůbec nevznikne.

Aplikace tedy ukazuje jednu pozvánku od začátku do konce. Na straně organizátora se nemění nic: účastník je v seznamu u poskytovatele od první chvíle.

Korelace mezi **různými** příjemci se nemění: Borisova a Cyrilova pozvánka na tutéž schůzku jsou dvě entity ve dvou tenantech a nic se mezi nimi nepropisuje.

### 14.8 Úložiště

SQLite na tenanta znamená, že transakce přes dva tenanty neexistuje. Doručení ji nepotřebuje:

1. **Odesílatel:** zpráva je v souboru outboxu jeho tenanta, zapsaná v transakci přijetí mutace s plným fsync. Patří k autorským datům.
2. **Dispečer** (úloha v řídicí rovině) najde v adresáři tenanta příjemce a předá zprávu jeho zapisovateli.
3. **Příjemce:** jeho jediný zapisovatel ve vlastní transakci vloží zprávu do inboxu, idempotentně podle `id`, a zpracuje ji do entity.
4. **Potvrzení:** odesílatel označí zprávu jako doručenou.

Jsou to dvě transakce, každá v jednom tenantu, s doručením „aspoň jednou" a idempotentním příjmem. Pád mezi kroky 3 a 4 znamená opakované doručení, které příjemce zahodí.

| Otázka | Odpověď |
| --- | --- |
| Tenant příjemce je na jiném uzlu | stejná zpráva přes vnitřní RPC mezi uzly. Stejný protokol jako federace (kapitola 15), jiná důvěra: uzly jedné instance si věří |
| Velikost | outbox drží poslední zprávu na dvojici (UID, příjemce), inbox poslední REQUEST na dvojici (organizátor, UID). Obojí je omezené počtem schůzek |
| Adresář | řídicí rovina; jediná data, která se čtou napříč tenanty. Věta „žádné dotazy napříč tenanty" z kapitoly 4.1 platí dál pro datovou rovinu |
| Příjemce byl smazán | trvalé odmítnutí; účastník dostane `delivery: undeliverable` |
| Smazání tenanta | nejdřív CANCEL pro schůzky, které organizuje, a odmítnutí těch, kam je pozvaný; potom se odeberou jeho adresy a smažou jeho soubory |
| PostgreSQL | outbox a inbox jsou tabulky; i tak dvě oddělené transakce, aby obě varianty úložiště měly stejné chování |

**Obnova ze zálohy.** Tenant obnovený ze starší zálohy může mít nižší `sequence`, než ostatní už viděli. Proto:

- obnovený **organizátor** se před dalším odesláním zeptá příjemců zprávou REFRESH na nejvyšší `sequence`, kterou znají, zvýší svou nad ni a pošle aktuální stav;
- obnovený **pozvaný** pošle REFRESH organizátorům svých pozvánek a ti mu znovu pošlou aktuální REQUEST;
- pozvánky přijaté po záloze se vrátí samy, protože outbox organizátora drží poslední zprávu.

### 14.9 Zneužití a soukromí

Rozhodnuto: **pozvat smí kdokoli na stejném serveru. Kde pozvánka přistane, určuje příjemce.**

| Odesílatel | Kde pozvánka přistane |
| --- | --- |
| známý | rovnou v kalendáři jako předběžná, dokud příjemce neodpoví |
| neznámý | mezi **žádostmi**: není v kalendáři ani ve výchozích dotazech, není zdrojem pravidel a nedostane se do žádného kalendáře u poskytovatele, dokud ji příjemce nepřijme |
| blokovaný | zahodí se; odesílatel se o tom nedozví |

**Známý odesílatel** je ten, jehož adresa:

- je v kontaktech příjemce (jádro je synchronizuje);
- patří někomu, koho příjemce sám dřív pozval, nebo od koho už pozvánku přijal. Až přibude e-mail (fáze 11), také tomu, s kým si psal;
- je na seznamu, který si příjemce vede sám, například lidé z domácnosti. Aplikace k němu může nabídnout skupinu „domácnost"; jádro drží jen seznam adres.

**Nastavení příjemce** `incoming_invites`:

| Hodnota | Chování |
| --- | --- |
| `all` | všechno rovnou do kalendáře (volnější) |
| `known` | výchozí: známí rovnou, neznámí jako žádost |
| `requests` | všechno jako žádost (přísnější) |

Provozovatel může změnit výchozí hodnotu instance, ne volbu uživatele.

- **Přijetí žádosti** ji přesune do kalendáře. Odesílatele může příjemce zároveň zařadit mezi známé. Zahodit žádost jde i bez odpovědi organizátorovi.
- **Limity na odesílatele:** nové pozvánky za hodinu a pozvánky jednomu příjemci (návrh: 100 a 20). Další mutace se odmítne typovanou chybou.
- **Co doručil poskytovatel, jádro neschovává.** Pozvánku, kterou do kalendáře pozvaného vložil jeho poskytovatel, vidí uživatel tak jako tak; tam platí filtr poskytovatele. Žádosti chrání to, co doručuje jádro.
- **Existence adresy.** Odpověď API na přidání účastníka prozradí, zda adresa patří místnímu tenantovi. Proto dohledatelnost: nedohledatelný tenant vypadá jako vnější adresa.
- **Free/busy mezi tenanty** v první verzi není. Odhaluje rozvrh člověka každému, kdo se zeptá. Později jen s výslovným souhlasem pro konkrétního tazatele.
- **Obsah zprávy** se validuje stejně přísně jako u federace (kapitola 15.3): velikost, počet účastníků a výjimek, žádné stahování vzdálených zdrojů.

### 14.10 Sdílení kalendářů mezi lidmi

Otázka zadání: má sdílení celého kalendáře (čtení nebo zápis) stát na stejném mechanismu?

**Ano na stejné přepravě a stejném modelu u příjemce, ale jako samostatná rodina zpráv.** Pozvánka je jeden záznam s účastníky. Sdílení je proud všech záznamů kolekce bez účastníků. Míchat je do jedné metody by pokazilo obojí.

| Stupeň | Návrh | Doporučení |
| --- | --- | --- |
| **Pozvánky** | tato kapitola | první verze |
| **Sdílení ke čtení** | jednosměrné pravidlo (kapitola 13), jehož cílem je *odebíraná interní kolekce* v tenantu příjemce. Přes hranici jdou stavové zprávy „záznam změněn" a „záznam smazán" (obdoba iTIP PUBLISH). Použije filtr i masku. Vyžaduje souhlas příjemce a jde kdykoli odvolat. Příjemce si odebíraný kalendář může pravidlem zrcadlit do vlastního kalendáře u poskytovatele | fáze 9, po vydání 1.0 |
| **Sdílení se zápisem** | vyžaduje vzdálenou mutaci s autoritou u vlastníka, oprávnění po polích, odvolání a řešení konfliktů přes hranici tenanta | pozdější fáze (12), až se osvědčí sdílení ke čtení (rozhodnuto) |

Do té doby pro sdílení se zápisem stačí dvě existující cesty:

- **sdílení u poskytovatele** (sdílený kalendář Google nebo iCloud). Jádro ho už synchronizuje jako kolekci s úrovní přístupu;
- **sdílení v aplikaci.** Aplikace drží tokeny obou tenantů a zapisuje za oba, jako to dnes dělá Musubi členstvím v kalendářích.

### 14.11 API

```
GET    /v1/scheduling/addresses            plánovací adresy tenanta a způsob ověření
POST   /v1/scheduling/addresses            přidat adresu (správcovské potvrzení)
DELETE /v1/scheduling/addresses/{address}
GET    /v1/scheduling/settings             dohledatelnost, incoming_invites
PATCH  /v1/scheduling/settings
GET    /v1/scheduling/senders              známí, blokovaní a čekající odesílatelé
PUT    /v1/scheduling/senders/{address}    { state: known | blocked }
```

Nová volání pro samotné pozvánky nejsou potřeba:

- pozvat = běžná mutace entity se seznamem účastníků; odpověď uvádí kanál a stav doručení každého z nich;
- příchozí pozvánky = `GET /v1/entities?collection=<schránka pozvánek>`, žádosti s `include=requests`;
- přijmout žádost = odpovědět na ni, nebo zařadit odesílatele mezi známé;
- zrušit schůzku = mutace `delete` s `notify_attendees: true` (kapitola 12.7);
- odpovědět = `PATCH /v1/entities/{id}/placements/{pid}` s polem odpovědi, stejně jako u pozvánky od poskytovatele.

Proud změn nese novou pozvánku, změnu, zrušení, odpověď účastníka a změnu stavu doručení.

### 14.12 Zamítnuté alternativy

- **Sdílený tenant domácnosti** (revize 3). Zamítnuto rozhodnutím uživatele: měkká izolace uvnitř tenanta je slabší než dva tenanty se zprávami.
- **Jedna entita sdílená dvěma tenanty.** Znamenala by transakce a oprávnění přes hranici úložiště a klíče.
- **Nechat všechno na poskytovatelích.** Funguje jen tam, kde obě strany mají poskytovatele s plánováním, a je pomalé a závislé na e-mailu. Záznam bez poskytovatele by pozvat nešel vůbec.
- **Místního účastníka k poskytovateli nezapisovat** (doporučení revize 4). Zamítnuto: vnější účastníci ani organizátor ve svém klientovi by neviděli úplný seznam účastníků. Dvojí doručení řeší spojení u příjemce a jedna cesta odpovědi (kapitoly 14.6 a 14.7).
- **Politika instance „jen známí odesílatelé"** (revize 4). Nahrazeno nastavením příjemce: zvát smí každý, o zařazení rozhoduje příjemce.
- **Vlastní e-mailové pozvánky (iMIP) pro vnější účastníky.** Patří k doméně e-mailu: doručitelnost, SPF a DKIM, zpracování odpovědí. Vnější účastníky zve poskytovatel. V první verzi nejsou (rozhodnuto).

---

## 15. Federace

Zadání: uživatel federaci chce, **pokud je proveditelná a bezpečná**. Byla by to stejná schránka zpráv jako v kapitole 14, jen mezi servery. Tato kapitola říká, co to stojí, a končí verdiktem.

### 15.1 Co by federace byla

- Tytéž zprávy REQUEST, REPLY a CANCEL jako v kapitole 14.3. Outbox odesílajícího serveru → HTTPS → inbox přijímajícího serveru.
- Přijatá zpráva projde stejným zpracováním jako místní: zóna žádostí, schránka pozvánek, pravidla, spojení pozvánky doručené dvěma cestami.
- Adresa účastníka je e-mailová adresa. Server, který za doménu adresy mluví, se dohledá přes `/.well-known/` na té doméně.
- Nic dalšího: žádné čtení cizích kalendářů, žádné API přes hranici serveru, žádné účty hostů.

Rozdíl proti kapitole 14 je jediný, ale zásadní: **odesílateli nelze věřit.** Místní zprávu podepisuje vlastní proces. Federovanou zprávu posílá cizí software pod cizí správou.

### 15.2 Model hrozeb

| Hrozba | Co se může stát | Čím se brání |
| --- | --- | --- |
| **Podvržení odesílatele** | zpráva tvrdí, že je od `anna@firma.cz`, ale poslal ji někdo jiný; zrušení nebo přesun cizí schůzky | podpis požadavku klíčem serveru; doména adresy organizátora se musí shodovat s doménou podepisujícího serveru (nebo ji na něj výslovně delegovat); vazba (organizátor, UID) → odesílatel z prvního REQUEST |
| **Vydávání se za server** | útočník se prohlásí za `firma.cz` | klíč se bere jen z `https://firma.cz/.well-known/…` s platným certifikátem; připnutí klíče po prvním použití; změna klíče mimo ohlášenou rotaci pozastaví partnera |
| **Spam a zneužití** | hromadné pozvánky s odkazy, jako kalendářový spam v e-mailu | federace je vypnutá; seznam povolených domén; zóna žádostí; limity na partnera, odesílatele a příjemce; blokování domény i adresy |
| **SSRF při odchozím doručení** | adresa účastníka nebo `.well-known` nasměruje server do vnitřní sítě | jediné místo pro odchozí požadavky: kontrola všech přeložených adres proti soukromým rozsahům, připnutí těchto adres do spojení, žádná přesměrování, jen `https`, časové limity |
| **SSRF při ověřování klíče** | totéž přes stažení klíče odesílatele při příjmu | stejné místo; klíče se stahují jen pro domény ze seznamu povolených |
| **Přehrání zprávy** | zachycený starý REQUEST nebo CANCEL poslaný znovu | podpis kryje čas vzniku a platnosti (minuty) a ID zprávy; paměť viděných ID po dobu platnosti; `sequence` odmítne starý stav i po jejím vypršení |
| **Vynášení dat pozvánkou** | sledovací odkazy, vzdálené obrázky, přílohy; pozvánka, kterou pravidlo samo zapíše do kalendáře u Googlu | jádro nic nestahuje; jen text; délky polí omezené; přílohy zahozené; nepřijatý odesílatel se do žádného kalendáře nedostane |
| **Soukromí** | zjišťování, zda adresa na serveru existuje; dotaz na free/busy | stejná odpověď pro existujícího, neexistujícího i blokujícího příjemce (přijato do fronty); žádný adresář; žádné free/busy |
| **Napadený partnerský server** | může podvrhnout cokoli za **své** uživatele a číst vše, co jim bylo posláno | nejde tomu zabránit, jen to omezit: za cizí domény nepodvrhne nic; dostává jen pozvánky určené jeho uživatelům; odvolání partnera zastaví příjem i odesílání |
| **Zahlcení** | obří těla, série s tisíci výjimek, pomalé spojení, zesílení přes mnoho příjemců | strop velikosti těla a počtu účastníků a výjimek; rozvinutí opakování až po přijetí; limity před ověřením podpisu podle IP, po něm podle partnera; časové limity |
| **Rozdílné verze** | starší partner rozumí zprávě jinak | verze protokolu v každé zprávě; neznámá verze se odmítne |
| **Napadený vlastní server** | únik podpisového klíče | klíč mimo databázi tenantů, rotace, odvolání zveřejněním nového klíče; partneři starý klíč po rotaci zahodí |

Dvě hrozby řešení nemají a je poctivé je pojmenovat:

1. **Napadený nebo zlovolný partner je pro své uživatele pravda.** Podpis serveru říká „tohle poslal server firma.cz", ne „tohle chtěla Anna". Podpisy uživatelů (koncové) by to řešily, ale znamenaly by správu klíčů u každého člověka. Nenavrhuji je.
2. **Spam nejde odstranit, jen zdražit.** Otevřená federace s neznámými servery nemá proti pozvánkovému spamu dobrou obranu. E-mail ji nemá dodnes a Matrix s ním bojuje také (z paměti).

### 15.3 Povinná opatření

**Identita serveru**

- Každý požadavek nese podpis podle **HTTP Message Signatures (RFC 9421)**, algoritmus Ed25519.
- Podpis kryje: `@method`, `@target-uri`, `@authority`, `content-digest` (RFC 9530), `content-type`, čas vzniku a konce platnosti, `keyid` a ID zprávy.
- Veřejné klíče jsou na `https://<doména>/.well-known/musubi-sync` (název k rozhodnutí), spolu s verzí protokolu a adresou schránky. Stahují se přes TLS s platným certifikátem.
- Doména adresy organizátora = doména, jejíž klíč zprávu podepsal. Delegace na jiný hostitel jen výslovným záznamem v `.well-known` domény adresy.
- Klíč se po prvním ověření připne. Rotace: nový klíč zveřejněný vedle starého po dobu překryvu. Jiná změna partnera pozastaví, dokud ji provozovatel nepotvrdí.

**Souhlas**

- Federace je ve výchozím stavu **vypnutá**.
- Provozovatel vede **seznam povolených domén**. Zprávy odjinud se odmítnou před zpracováním těla.
- I z povolené domény platí zóna žádostí (kapitola 14.9): pozvánka od odesílatele, kterého příjemce nepřijal, se neukáže jako pozvánka, nespustí pravidla a nedostane se do žádného kalendáře. Přes federaci se „známý" počítá úžeji: jen odesílatel, kterého příjemce výslovně přijal nebo sám dřív pozval. Kontakt v adresáři nestačí.
- Příjemce může blokovat adresu i celou doménu.

**Validace obsahu**

- Strop velikosti těla (návrh: 256 kB), počtu účastníků (návrh: 500) a výjimek série (návrh: 1000).
- Jen text. Žádné přílohy, žádné vložené HTML. URL se ukládají jako text a nikdy se nenačítají.
- Tělo se dekóduje stejným kodekem jako data od poskytovatelů a neprojde-li, zpráva se odmítne celá.
- Opakování se před přijetím nerozvíjí.

**Minimum dat**

- Příjemce dostane jen to, co potřebuje: sdílená pole schůzky a seznam účastníků. Organizátor může seznam účastníků skrýt.
- Žádný adresář, žádné free/busy, žádný dotaz „existuje tato adresa?".
- Odpověď na doručení je vždy stejná: přijato do fronty.

**Provoz**

- Limity na partnera, na odesílatele a na dvojici odesílatel–příjemce.
- **Odvolání:** odebrání domény ze seznamu zastaví příjem i odesílání okamžitě. Existující pozvánky zůstanou a označí se jako bez spojení s organizátorem.
- **Jedno místo pro odchozí požadavky** s připnutím adres v okamžiku spojení (stejná vrstva jako pro CalDAV, kapitola 7).
- **Ulož a předej:** zpráva se uloží a doručuje s opakováním. Žádné živé spojení, žádné předávání cizích požadavků.
- Verze protokolu v každé zprávě; neznámá se odmítne.
- Auditní záznam každé přijaté a odmítnuté zprávy bez obsahu.

**Formát zprávy** (otevřený detail): doporučuji tělo jako iTIP v iCalendaru (`text/calendar; method=REQUEST`) ve verzované obálce. Kodek a validace už existují a formát je standardní. Druhá možnost je vlastní JSON; je jednodušší, ale znamená druhý parser na hranici důvěry.

### 15.4 Co dělají ostatní

| Systém | Jak řeší identitu a důvěru | Poučení |
| --- | --- | --- |
| **iSchedule** (`draft-desruisseaux-ischedule`) | iTIP přes HTTP mezi servery; každý požadavek podepsaný DKIM (RFC 6376), klíč z DNS nebo z `.well-known`; koncový bod `/.well-known/ischedule`. Poslední verze -05 z května 2013 vypršela; stav v datatrackeru je „Expired, IESG: Dead"; RFC ani nástupce nevznikl (ověřeno) | nejbližší existující návrh. Nezemřel na bezpečnost, ale na to, že ho nikdo nenasadil. **Žádnou interoperabilitu tedy nezískáme**: federovat budou jen instance musubi-sync mezi sebou |
| **iMIP** (RFC 6047) | iTIP e-mailem; identita stojí na SPF, DKIM a DMARC | pozvánku lze podvrhnout stejně jako e-mail a kalendářový spam je známý problém. Google kvůli němu zavedl volbu přidávat pozvánky jen od známých odesílatelů (z paměti). Odtud zóna žádostí |
| **Matrix** (z paměti, neověřeno) | požadavky mezi servery podepsané Ed25519; klíče na `/_matrix/key/v2/server`; dohledání přes `.well-known` a SRV; notářské servery | podpis serveru s klíčem z `.well-known` funguje ve velkém. Problémy leží jinde: spam pozvánkami, složitost sdíleného stavu místností, únik metadat. My sdílený stav nemáme: zpráva má jednoho vlastníka |
| **ActivityPub / Mastodon** | podpis HTTP požadavků klíčem účtu (starší návrh draft-cavage, přechod na RFC 9421; z paměti); adresy `uzivatel@domena` přes WebFinger; hlavní nástroj obrany je blokování domén | **CVE-2024-23832** (ověřeno): Mastodon nesvázal identifikátor staženého objektu s adresou, odkud ho stáhl. Šlo se tak vydávat za vzdálený účet a převzít ho; CVSS 9,4. Poučení: původ se odvozuje z ověřeného spojení a podpisu, nikdy z toho, co o sobě tvrdí obsah |
| **Nextcloud** | sdílení kalendářů mezi instancemi přišlo ve verzi 32 jen ke čtení, od verze 33 i se zápisem; příjemce se adresuje `uzivatel@instance`; správce ho zapíná přepínačem; oznámení OCM se v novějších verzích ověřují podpisem podle RFC 9421 (ověřeno v dokumentaci a hlášeních chyb). Jak je podepsaná samotná federace kalendářů a historie chyb typu SSRF: **neověřeno** | i velký projekt začal jen čtením, federaci drží za přepínačem a ověřování podpisů mu po zavedení rozbíjelo existující propojení (hlášené chyby). Čekat potíže s provozem, ne jen s návrhem |

### 15.5 Poučení z federace v Musubi

Čteno jen pro čtení: `docs/adr/ADR-005-federation-gateway.md`, `packages/docs/src/content/docs/architecture/federation.mdx`, `apps/api/src/federation_origin.ts`, `federation_peer.ts`, `federation_tokens.ts`, `handlers/federation.ts`.

| Co Musubi dělá | Co se ukázalo | Poučení pro jádro |
| --- | --- | --- |
| Není to spojení server–server. Pozvánkový odkaz je oprávnění; na cílovém serveru vznikne stínový účet (`user.isExternal`, `homeServer`) a 90denní token člena; domovský server pak předává asi 20 volání API | široká plocha; správce domovského serveru může na cílovém serveru jednat za uživatele | žádné předávání API a žádné stínové účty; jen tři typované zprávy |
| `homeServer`, e-mail a jméno z pozvánky slouží jen k zobrazení; nikdy se podle nich nehledá existující identita (dokumentovaná mezera: chybí důkaz identity serveru) | správné rozhodnutí při chybějícím podpisu | neověřené tvrzení není identita. Identitu dává jen podpis |
| ADR-005 nejdřív přijalo zbytkové riziko DNS rebinding s tím, že připnutí IP rozbije SNI. Byl to omyl a opravilo se to: všechny odchozí federační požadavky jdou přes jedno `federationFetch`, které přeloží jméno, zkontroluje celou sadu adres, připne ji do spojení (jméno zůstává pro Host, SNI a certifikát), nenásleduje přesměrování a hlídá časový limit | první návrh ochranu podcenil | jedno místo pro odchozí požadavky s připnutím adres od prvního dne |
| Tokeny člena byly v klientech, kde je mohl ukrást skript; přesunuly se jen na server | dlouho platné tokeny u protistrany jsou cíl | podpis každého požadavku místo dlouho platného tokenu v cizích rukou |
| Verze partnera: `MIN_PEER_VERSION`, odpověď `426`, neznámá verze se odmítne | funguje | verze v každé zprávě, při neznámé odmítnout |
| Domovský server je v cestě dat a drží jedno trvalé spojení na uživatele a server | dostupnost a cena závisí na obou serverech zároveň | ulož a předej místo živého předávání |
| Zrcadlení přes adaptér se zamítlo, protože neunese účast a role; federuje se celá spolupráce na kalendáři se seznamem povolených cest | rozsah je velký a obrana stojí na seznamu cest | úzká typovaná plocha místo průchodu API. Spolupráce na kalendáři je věc aplikace |
| Veřejný náhled pozvánky je omezený podle IP a ukazuje jen jména a krátký výhled, žádné e-maily | funguje | minimum dat před souhlasem |
| Oprávnění se ověřuje vždy na serveru vlastníka; vyhození platí hned | funguje | autorita zůstává u vlastníka, příjemce drží kopii |
| Scénáře `federation-transport` s falešnými servery na loopbacku | znovupoužitelný vzor testů | stejný vzor pro konformační sadu federace |

Federace v jádru federaci Musubi **nenahrazuje**. Musubi federuje spolupráci na kalendáři (členové, role, úkoly). Jádro by federovalo jen pozvánky.

### 15.6 Verdikt

**Proveditelná a dostatečně bezpečná je federace jen v tomto zúženém rozsahu:**

1. **Jen plánovací zprávy:** REQUEST, REPLY, CANCEL (a REFRESH pro dorovnání).
2. **Vypnutá ve výchozím stavu;** zapíná ji provozovatel.
3. **Jen s vyjmenovanými partnery:** seznam povolených domén. Žádná otevřená federace.
4. **Podepsané požadavky** (RFC 9421) s klíčem z `.well-known` domény odesílatele přes TLS; vazba domény adresy na podepisující server.
5. **Souhlas příjemce před čímkoli viditelným:** nepřijatý odesílatel končí v zóně žádostí.
6. **Přísná validace, žádné stahování, ulož a předej, jedno místo pro odchozí požadavky.**

**Výslovně mimo:**

- otevřená federace s neznámými servery: **není dost bezpečná.** Proti spamu a zlovolnému partnerovi nemá obranu, kterou bych uměl doložit;
- free/busy přes servery;
- sdílení kalendářů přes servery (čtení i zápis);
- adresář a vyhledávání uživatelů;
- průchod API, stínové účty, tokeny držené partnerem;
- protinávrhy času, delegace, přílohy;
- interoperabilita s cizím softwarem (není s čím: iSchedule je mrtvý návrh).

**Kdy:** až po vydání 1.0 a až bude místní plánování mezi tenanty v provozu a stabilní (fáze 10 v kapitole 11.1). Podmínkou spuštění je **externí bezpečnostní revize** protokolu i implementace. Bez ní federaci nezapínat ani v zúženém rozsahu.

**Co zbývá jako riziko i potom:**

- napadený povolený partner může podvrhovat pozvánky za své uživatele;
- hodnota je malá, dokud musubi-sync neprovozuje víc instancí, které si věří. Pro lidi na různých serverech dnes funguje pozvánka přes poskytovatele (e-mail) a ta zůstává;
- je to vlastní protokol bez standardu, který bychom museli udržovat sami.

Moje doporučení: **navrhnout zprávy z kapitoly 14 tak, aby federaci unesly** (podepsatelná obálka, verze, vazba adresy na odesílatele, vnitřní RPC mezi uzly stejným protokolem), ale federaci samotnou stavět až na skutečnou poptávku dvou provozovatelů, kteří ji chtějí mezi sebou.

---

## Zdroje

Výsledky fáze 0 s vlastními zdroji a reprodukovatelnými příkazy: [providers.md](../research/phase0/providers.md), [storage.md](../research/phase0/storage.md), [calcard.md](../research/phase0/calcard.md), [recurrence.md](../research/phase0/recurrence.md). Kde se tvrzení v návrhu opírá o ně, mají přednost před seznamem níže.

Poskytovatelé:

- Microsoft Graph, event: delta (v1.0): https://learn.microsoft.com/en-us/graph/api/event-delta?view=graph-rest-1.0
- Microsoft Graph, event resource (v1.0: `cancelledOccurrences`, `exceptionOccurrences`, `iCalUId`, rozšíření): https://learn.microsoft.com/en-us/graph/api/resources/event?view=graph-rest-1.0
- Microsoft Graph, Get event (příklad čtení hlavní události série s výjimkami): https://learn.microsoft.com/en-us/graph/api/event-get?view=graph-rest-1.0
- Microsoft Graph, change notifications (životnosti, latence, limity subscriptions): https://learn.microsoft.com/en-us/graph/change-notifications-overview
- Microsoft Graph, throttling limits: https://learn.microsoft.com/en-us/graph/throttling-limits
- Microsoft Graph, webhooks delivery: https://learn.microsoft.com/en-us/graph/change-notifications-delivery-webhooks
- Průvodce synchronizací Outlook kalendáře (events/delta jen v beta): https://nocal.app/blog/2026/07/microsoft-graph-calendar-sync-guide
- Google Calendar API, usage limits: https://developers.google.com/workspace/calendar/api/guides/quota
- Google Calendar API, push notifications: https://developers.google.com/workspace/calendar/api/guides/push
- Google Tasks API, tasks.list: https://developers.google.com/workspace/tasks/reference/rest/v1/tasks/list
- Google Tasks API, limits: https://developers.google.com/workspace/tasks/limits
- Google Tasks, omezení termínu a opakování: https://tasksboard.com/blog/google-tasks-due-time , https://tasks.org/docs/sync_google_tasks/
- Google People API, connections.list (sync token): https://developers.google.com/people/api/rest/v1/people.connections/list
- Srovnání kontaktových API (Graph `$batch`, contacts delta; sekundární zdroj): https://cli.nylas.com/guides/contacts-api-comparison
- iCloud CalDAV, nastavení a hesla pro aplikace (sekundární zdroj): https://cli.nylas.com/guides/icloud-caldav-settings
- Připomínky Apple mimo CalDAV: https://www.busymac.com/docs/faqs/112990-reminders-in-ios-13-and-macos-catalina-drops-support-for-caldav/
- WebDAV-Push, draft: https://github.com/bitfireAT/webdav-push
- WebDAV-Push ve Stalwartu (požadavek): https://support.stalw.art/t/support-webdav-push-notifications/1289
- WebDAV-Push v DAVx⁵: https://manual.davx5.com/webdav_push.html

Kontakty:

- Google People API, otherContacts.list: https://developers.google.com/people/api/rest/v1/otherContacts/list
- Microsoft Graph, contact: delta (v1.0): https://learn.microsoft.com/en-us/graph/api/contact-delta?view=graph-rest-1.0
- Microsoft Q&A, delta kontaktů bez složky: https://learn.microsoft.com/en-us/answers/questions/1634378/microsoft-graph-api-contacts-delta
- Skupiny v CardDAV a u Applu: https://github.com/mstilkerich/rcmcarddav/blob/master/doc/GROUPS.md
- iCloud CardDAV, vCard 3.0 a hesla pro aplikace (cizí projekty): https://github.com/omarshahine/openclaw-icloud-contacts , https://github.com/skoelle/icloud-contacts-sync

Standardy:

- JSCalendar 2.0, draft: https://datatracker.ietf.org/doc/html/draft-ietf-calext-jscalendarbis-22
- JMAP for Calendars, draft: https://datatracker.ietf.org/doc/draft-ietf-jmap-calendars/
- JMAP pracovní skupina, stav dokumentů: https://datatracker.ietf.org/wg/jmap/
- JMAP Tasks, propadlý draft: https://datatracker.ietf.org/doc/html/draft-ietf-jmap-tasks-06

Rust:

- calcard: https://github.com/stalwartlabs/calcard , https://lib.rs/crates/calcard
- calcard-next (fork s rozbalením opakování): https://docs.rs/calcard-next/latest/calcard_next/
- rrule, port na jiff (PR 151): https://github.com/fmeringdal/rust-rrule/pull/151
- rrule2 (fork): https://github.com/km0e/rrule2
- libdav a pimsync: https://lib.rs/crates/libdav , https://sr.ht/~whynothugo/pimsync/

Existující systémy:

- Nylas v3, uvedení: https://www.nylas.com/blog/introducing-nylas-apiv3/
- Nylas v3, změny proti v2: https://legacy.developer.nylas.com/docs/v2/upgrade-to-v3/features-and-changes/
- Nylas, webhooky vytvoření a změny: https://support.nylas.com/hc/en-us/articles/26826548737053
- Srovnání sync enginů (sekundární zdroje): https://kanopylabs.com/blog/tanstack-db-vs-electricsql-vs-zero-sync , https://www.smashingmagazine.com/2026/05/architecture-local-first-web-development/
- SQLite na tenanta (sekundární zdroje): https://www.javacodegeeks.com/2026/05/sqlite-in-2026-why-serious-apps-are-choosing-it-over-postgres.html , https://www.sitepoint.com/sqlite-edge-production-readiness-2026/

Plánování a federace:

- iTIP (RFC 5546): https://www.rfc-editor.org/rfc/rfc5546
- Plánování v CalDAV (RFC 6638, parametr `SCHEDULE-AGENT`): https://tools.ietf.org/html/rfc6638
- iMIP (RFC 6047): https://www.rfc-editor.org/rfc/rfc6047
- HTTP Message Signatures (RFC 9421): https://www.rfc-editor.org/rfc/rfc9421
- Digest Fields (RFC 9530): https://www.rfc-editor.org/rfc/rfc9530
- iSchedule, vypršelý draft: https://datatracker.ietf.org/doc/draft-desruisseaux-ischedule/ , https://datatracker.ietf.org/doc/html/draft-desruisseaux-ischedule-05
- Google Calendar API, `sendUpdates`: https://developers.google.com/workspace/calendar/api/v3/reference/events/insert , https://developers.google.com/workspace/calendar/api/v3/reference/events/update
- Nextcloud, federace kalendářů: https://docs.nextcloud.com/server/stable/admin_manual/groupware/calendar.html , https://docs.nextcloud.com/server/stable/user_manual/en/groupware/calendar.html
- Nextcloud, hlášení k ověřování podpisů ve federaci: https://github.com/nextcloud/server/issues/65295 , https://github.com/nextcloud/server/issues/57241
- Mastodon, CVE-2024-23832: https://github.com/mastodon/mastodon/security/advisories/GHSA-3fjr-858r-92rw , https://www.openwall.com/lists/oss-security/2024/02/02/4
- Musubi, čteno v repozitáři: `docs/adr/ADR-005-federation-gateway.md`, `packages/docs/src/content/docs/architecture/federation.mdx`, `apps/api/src/federation_origin.ts`, `federation_peer.ts`, `federation_tokens.ts`, `handlers/federation.ts`

Bez ověření na webu, z předchozí znalosti: úložné backendy Stalwartu, architektura Evolution Data Server a Apple CalendarAgent, synchronizace Linearu, Rust v Thunderbirdu, Cronofy a Aurinko nad rámec výzkumu v `docs/research/nylas/`. Pro kapitolu 12 rovněž z předchozí znalosti: rozesílání pozvánek při uložení události s účastníky (Google `sendUpdates`, Graph, CalDAV podle RFC 6638), skrytá pole pro značky (`extendedProperties`, `clientData`), vytvoření s klientským ID, licence DAVx⁵ a Nanga a nástroje pro zrcadlení kalendářů. Fáze 0 ověřila vše kromě nástrojů pro zrcadlení ([providers.md](../research/phase0/providers.md)). Hodnoty `sendUpdates` a `SCHEDULE-AGENT` jsou od revize 4 ověřené.

Pro kapitoly 13 až 15 z předchozí znalosti, bez ověření: obsah RFC 5546, 6047, 9421 a 9530 (v této revizi jsem je nečetl, cituji je podle čísla); federace Matrixu (podpisy, klíče, spam pozvánkami); podpisy v ActivityPub a WebFinger; kalendářový spam přes iMIP a volba Googlu „jen od známých odesílatelů"; chování Googlu při `sendUpdates=none`; potlačení pozvánek v Graphu; `SCHEDULE-AGENT` a `calendar-user-address-set` na iCloudu; jak poskytovatelé hlásí přesun položky mezi kalendáři; jak je podepsaná federace kalendářů v Nextcloudu. Čísla označená „návrh" (limity, lhůty, prahy pojistek) jsou můj odhad k doladění, ne měření. Od revize 5 navíc bez ověření: zda organizátor smí u Googlu a v Graphu zapsat stav odpovědi účastníka; zda samostatný požadavek se `sendUpdates=none` u Googlu ušetří první e-mail; jak se servery chovají k `SCHEDULE-AGENT=CLIENT`; stav a rozhraní knihoven `jsonwebtoken` a `openidconnect`.
