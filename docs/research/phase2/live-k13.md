# Fáze 2: živá kontrola kalendářů úkolů iCloudu (K13b)

Stav: hotovo · datum: 2026-11-25 · plán: [K13b](../../plan/phase2.md#k13b-živá-kontrola) · fixtures: [`crates/musubi-testkit/fixtures/dav/icloud-k13/`](../../../crates/musubi-testkit/fixtures/dav/icloud-k13/)

## 1. Otázka

Čte profil `icloud` (od K13a) kalendáře jen s VTODO na skutečném iCloudu tak, jak předpokládá kapitola 12? Je opakovaný VTODO v rozsahu se správnými výskyty a `done`? Fungují delta (změna, smazání) a prošlý token na kalendáři úkolů? Přepisuje iCloud nahraná těla, jak se chová `PUT` s `If-Match` (otevřená otázka 4)? A zapisují Připomínky Apple do CalDAV?

## 2. Metoda a omezení

- 2026-11-25, osobní Apple ID uživatele s heslem pro aplikace (jako T11 a K10). Uživatel povolil zápisy **jen do objektů, které skript založí**: kalendář `musubi-k13` a testovací VTODO v něm. Existující kalendáře, úkoly, události ani kontakty se neměnily.
- Skript v Pythonu (jen stdlib) mimo repozitář: přímo CalDAV proti iCloudu. Heslo četl ze souboru 0600, do `musubi-syncd` šlo v těle požadavku, nikdy v argumentech.
- `musubi-syncd` z `main` (`80d282f`, s K13a) jako samostatná instance: prázdný datový adresář v `/tmp`, jen `127.0.0.1`, bez Tailscale, aktivní úroveň `{min: 15, max: 60}` a `POST /v1/interest`. Účet přes `POST /v1/accounts/dav {"profile": "icloud"}`. Po kontrole se instance zastavila a datový adresář smazal.
- Z existujících dat se ukládaly jen struktury: sady komponent, počty, oprávnění a to, zda název obsahuje „⚠“. Názvy ani těla úkolů uživatele se neukládaly. Fixtures mají DSID → `DSID`, ID existujících kolekcí → `idN`, partition host → `pNN`, tokeny a etagy `REDACTED`. Těla testovacích VTODO jsou naše vlastní a ve fixtures zůstala.
- Omezení: žádný skutečný klient (Thunderbird, DAVx⁵, Připomínky) do testovacího kalendáře nezapisoval. Všechny tvary VTODO napsal skript. Tvar „kopie a posun“ starých Připomínek proto živě ověřit nešlo (kapitola 5).

## 3. Zjišťování a zakládání

- Calendar home má stejné kolekce jako v T11: dva kalendáře `VEVENT`, jeden kalendář jen s `VTODO`, `inbox`, `outbox`, `notification`.
- **`MKCALENDAR` s `supported-calendar-component-set` jen `VTODO`: `201`.** Nová kolekce hlásí `comp VTODO`, stejné reporty (`sync-collection`, `calendar-query`, `calendar-multiget`, …), stejná oprávnění jako ostatní kalendáře (`read`, `write`, `bind`, `unbind`, …), `sync-token` i `getctag`. Riziko R-b z plánu K13 (odmítnutí na účtu s novými Připomínkami) nenastalo.
- `PROPFIND` s `<propname/>` iCloud nepodporuje: odpoví, jako by `propname` byla neznámá vlastnost.

## 4. Čtení a delta

### 4.1 Těla po `PUT`

Pět VTODO (`If-None-Match: *`, vše `201`): (a) denní série s časem a `TZID=Europe/Prague`, výjimkou 25. 11. se `STATUS:COMPLETED` a výjimkou 26. 11. posunutou z 9:00 na 15:00; (b) denní série jen s datem; (c) jednorázový úkol s časem; (d) úkol bez data; (e) úkol s `COMPLETED` bez `STATUS`.

- Vlastnosti `VCALENDAR` a všech `VTODO` zůstaly beze změny včetně pořadí: `PRODID`, `DTSTAMP`, `STATUS`, `COMPLETED`, `PERCENT-COMPLETE`. iCloud **nepřidal žádné `X-APPLE-*`** ani `STATUS`.
- **`VTIMEZONE` iCloud nahrazuje vlastní:** místo dvou pravidel úplná historie zóny od roku 1850 (17 podkomponent) a `X-LIC-LOCATION`, a přesune ji za `VTODO`. Těla bez `TZID` (b, d) vrací bajt po bajtu stejná.
- `PUT` vrací `ETag` a ten je shodný s `getetag` při následném `GET`, i když se tělo změnilo.

Pro čtení to nic nemění: kodek bere `TZID` z IANA a výskyty sedí (4.3). Riziko R-c z plánu K13 se týká jen `VTIMEZONE`, ne času úkolu.

### 4.2 Podmíněný zápis (otevřená otázka 4)

| požadavek | odpověď |
| --- | --- |
| `PUT` s `If-Match: <aktuální etag>` | `204`, etag se změnil |
| `PUT` s `If-Match: <zastaralý etag>` | `412`, prázdné tělo |
| `DELETE` s `If-Match: <zastaralý etag>` | `412` |
| `DELETE` bez podmínky | `204` |
| `PUT` s `If-None-Match: *`, prostředek neexistuje | `201` |
| **`PUT` s `If-None-Match: *`, prostředek existuje (jiné tělo)** | **`204`, prostředek přepsán** |
| **`PUT` s `If-Match: *`, prostředek existuje** | **`412`** |
| **`PUT` s `If-Match: *`, prostředek neexistuje** | **`201`, prostředek vznikl** |

`If-Match` s konkrétním etagem funguje podle RFC. **Hvězdičky má iCloud prohozené:** `If-None-Match: *` nebrání přepsání a `If-Match: *` se chová jako „jen vytvořit“. Jeden vzorek každého řádku; řádek s `If-None-Match: *` na existujícím prostředku se opakoval dvakrát se stejným výsledkem (druhé opakování viz 4.5).

### 4.3 Rozsah přes `musubi-syncd`

Účet připojený (`200`), kolekce `musubi-k13` načtená **do 2 s** jako `kind: "task"`, `access: "writer"`, `delta: "sync_token"`, `coverage: full`, bez chyb. Rozsah `from=2026-11-23T23:00:00Z&to=2026-11-30T23:00:00Z&zone=Europe/Prague`:

| úkol | výsledek |
| --- | --- |
| (a) denní s časem | 7 výskytů 24.–30. 11. v 8:00Z (9:00 Praha); `done = true` **jen** 25. 11.; výskyt 26. 11. začíná ve 14:00Z s `recurrence_id` 26. 11. 8:00Z |
| (b) jen datum | 3 výskyty 24.–26. 11., každý `[den, den + P1D)`, `floating: true` |
| (c) jednorázový | jeden výskyt 27. 11. 16:00Z nulové délky, `done = false` |
| (d) bez data | v rozsahu není, ve výpisu `kind=task` ano |
| (e) `COMPLETED` bez `STATUS` | `progress: needs-action`, `done = false`; `progressUpdated` nese čas z `COMPLETED` |

`done=false` vrátí totéž bez výskytu 25. 11. `kind=event,task` vrátí stejné úkoly; událost v tom týdnu účet nemá. Výsledek odpovídá e2e testům K12 a K13a.

### 4.4 Delta a prošlý token

- (c) dokončený přes `PUT` s `If-Match`, (d) smazaný. `sync-collection` s tokenem z úvodního čtení: `207`, (c) `200`, (d) `404`, kolekce sama `200`.
- `musubi-syncd` obě změny ukázal **za 29 s** po smazání (30 s po změně), v rámci aktivní úrovně 15–60 s jako v K10: (c) `done = true` v rozsahu i ve výpisu, (d) z výpisu zmizel.
- Pozměněný i cizí token na kalendáři úkolů: **`403` s `<D:error><D:valid-sync-token/></D:error>`**, stejně jako u kalendáře událostí v T11. Adaptér to bere jako `CursorExpired`.

### 4.5 Smazání kalendáře

**`DELETE` kolekce (`204`) nesmaže její členy.** Nový `MKCALENDAR` na stejné adrese vrátil kolekci se všemi dříve nahranými VTODO, a to i těmi z předchozího běhu (`sync-collection` je vypsal jako `200`). Druhý běh kontroly 4.2 proto zastihl prostředky z prvního. Úklid proto smazal členy jednotlivě a pak kolekci. Další `MKCALENDAR` dal prázdnou kolekci, ta se smazala a `PROPFIND` na ni vrací `404`. Smazaný kalendář si iCloud zřejmě drží pro obnovu (iCloud.com, obnovení kalendářů).

## 5. Připomínky, výchozí seznam a R5

- **Negativní kontrola:** uživatel měl v aplikaci Připomínky založit denní opakovanou připomínku `musubi-k13-reminder` a jeden výskyt dokončit. VTODO s tímto `SUMMARY` není v žádné kolekci CalDAV, která VTODO pojme (hledáno v paměti, uložen jen výsledek): nenalezeno v 10:17, 10:19 ani při závěrečné kontrole v 10:22 SEČ (2026-11-25). Žádná nová kolekce s VTODO mezitím nepřibyla. Že ji uživatel do té doby skutečně založil, skript ověřit nemůže.
- **Seznam „Reminders ⚠️“:** žádná kolekce s „⚠“ v názvu neexistuje. Jediný existující kalendář jen s VTODO (už v T11) je prázdný (`PROPFIND` hloubky 1: žádní členové), má barvu a pořadí a stejná oprávnění jako ostatní kalendáře. Strukturní značka, podle které by šel výchozí seznam poznat bez lokalizovaného názvu, se nenašla. Otázka 3 plánu K13 tak zůstává u doporučení: synchronizovat, nic neskrývat.
- **R5, kopie a posun:** tvar starých Připomínek (do iOS 12) živě ověřit nešlo. Připomínky od iOS 13 do CalDAV nezapisují a na účtu žádný VTODO od Applu není. Fixture `apple_reminder.ics` a e2e test v `tests/dav.rs` zůstávají sekundární znalost. Ověřený je tvar s `RECURRENCE-ID` (4.3), tedy to, co zapisují klienti CalDAV.

## 6. Dopad

- **Přijetí K13 je splněné** pro kalendáře úkolů na CalDAV: čtení, výskyty a `done` po dokončení přes `RECURRENCE-ID`, delta (změna a smazání) a prošlý token. Kód ani kontrakt se nemění.
- **`COMPLETED` bez `STATUS` (podmíněná oprava kodeku, plán K13 2.4): neprovádí se.** iCloud tvar uloží beze změny a syncd ho čte jako `done = false`. Do dat ho ale napsal jen tento skript; od skutečného klienta ho nikdo neviděl. Podmínka „najde se ve skutečných datech“ splněná není.
- **Fáze 3 (zápis do iCloudu):** `If-Match` s etagem je spolehlivý (`204`/`412`). Hvězdičky ne: vytvoření s `If-None-Match: *` na iCloudu existující prostředek **tiše přepíše**. Zápis nesmí na tuto ochranu spoléhat. Vytvářet pod novým jménem (UUID), nebo před vytvořením ověřit neexistenci, a hvězdičky u profilu `icloud` neposílat. Patří do profilu jako zápisový fakt, stejně jako odchylky I2 (kapitola 5.4 plánu).
- **Smazání kalendáře** nemaže jeho členy (4.5). Pro čtení bez dopadu: syncd vidí zmizelou kolekci při objevování. Pro zápis ve fázi 3 to znamená, že založení kalendáře na adrese dříve smazaného vrátí stará data.
- `VTIMEZONE` z iCloudu je delší než nahraná, ale `TZID` zůstává. Zápis s záplatou z raw vrstvy (fáze 3) musí počítat s tím, že raw tělo po `PUT` není to, co klient poslal.
