# Fáze 2: živá kontrola iCloudu (T11)

Stav: hotovo jen pro čtení · datum: 2026-11-16 · plán: [T11](../../plan/phase2.md#t11-živá-kontrola-icloudu-spouští-koordinátor) · fixtures: [`crates/musubi-testkit/fixtures/dav/icloud-t11/`](../../../crates/musubi-testkit/fixtures/dav/icloud-t11/)

## 1. Otázka

Platí předpoklady profilu `icloud` (I1–I6)? Které reporty iCloud nabízí, jak vypadají home sety, jak odpovídá `sync-collection` a jak neplatný token?

## 2. Metoda a omezení

Testovací účet iCloud nešel založit (Apple vyžaduje další telefonní číslo). Kontrola proto běžela na **osobním** Apple ID uživatele s heslem pro aplikace, které uživatel po testu zruší, a to **jen pro čtení**:

- žádný zápis, takže `PUT` s `If-Match` na kontakt (vdirsyncer #1145) neproběhl;
- skript neukládal ani nevypisoval obsah: z odpovědí zůstal strom elementů, stavy a výčty protokolu. Odkazy mají DSID a ID kolekcí nahrazené (`DSID`, `idN`), partition host je `pNN-…`, tokeny a etagy jsou `REDACTED`;
- z jedné vizitky se četl jen řádek `VERSION`.

Kalendáře účtu jsou prázdné a adresář má jednu vizitku. Velikost dávky `multiget`, zkracování `507` a delta „po jedné změně" se proto změřit nedaly.

## 3. Zjišťování

- Kořen `https://caldav.icloud.com/` i `https://contacts.icloud.com/` odpoví na `PROPFIND current-user-principal` přímo `207`, **bez přesměrování**. Principal je `/DSID/principal/` na stejném hostu.
- Home sety jsou na **jiném hostu** (partition `pNN-caldav.icloud.com`, `pNN-contacts.icloud.com`) a **nesou výslovné `:443`**: `https://pNN-caldav.icloud.com:443/DSID/calendars/`. Normalizace originu z plánu (kap. 5, `:443`) je tedy nutná.
- Kalendáře: `calendar` kolekce zvlášť pro `VEVENT` a `VTODO` (žádná smíšená), plus `schedule-inbox`, `schedule-outbox` a `notification`. Home set sám hlásí `VEVENT, VFREEBUSY, VTODO`.
- Adresář: jediný `addressbook` (`/carddavhome/card/`). `supported-address-data` vrací `404`, vizitka je **vCard 3.0**.

| Kolekce | `supported-report-set` | `current-user-privilege-set` |
| --- | --- | --- |
| kalendář | `calendar-multiget`, `calendar-query`, `sync-collection`, `free-busy-query`, `calendar-search` a principal reporty | `read`, `write`, `write-content`, `write-properties`, `bind`, `unbind`, `read-free-busy`, `read-current-user-privilege-set` |
| `notification` | jako kalendář | jen `read`, `unbind`, `read-current-user-privilege-set` |
| adresář | `addressbook-multiget`, `sync-collection` (**žádný `addressbook-query`**) | `read`, `write`, `bind`, `read-acl`, `read-current-user-privilege-set` |

`sync-collection` u CardDAV je tím potvrzený (tabulka profilů v plánu).

## 4. `sync-collection`

- Úvodní čtení s prázdným tokenem: `207` s tokenem. **Kolekce sama je mezi členy** se stavem `200`: CalDAV ji vrací s lomítkem na konci, CardDAV **bez** (`…/card` místo `…/card/`). `sync.rs` ji přeskakuje přes `same_path`, takže to sedí.
- Delta s vráceným tokenem bez změny: `207`, nula členů, token stejného tvaru.
- **Neplatný token** (pozměněný i cizí): CalDAV **`403`**, CardDAV **`412`**, oba s `<D:error><D:valid-sync-token/></D:error>`.
- `addressbook-multiget`, ve kterém je href kolekce, skončí **`400 Bad Request` pro celý požadavek**, ne `404` pro jeden href. Multiget proto nesmí dostat odkaz na kolekci; `sync.rs` ho do referencí nedává.
- `503` ani `507` se neobjevily.

Token uložil koordinátor (soubor 0600, mimo repozitář). K10 s ním nejdřív 2026-12-16 zkusí deltu s 30 dní starým tokenem, pokud uživatel heslo pro aplikace do té doby nezruší.

## 5. Dopad

- **Oprava v této změně:** `412` s `valid-sync-token` je teď `CursorExpired` (`musubi-dav/src/sync.rs`). Dřív by zastaralý token adresáře na iCloudu nevedl k úplnému čtení.
- K7: profil `icloud` ve falešném DAV má vracet `412` u CardDAV a `403` u CalDAV, partition host s `:443` a kolekci mezi členy `sync-collection` (CardDAV bez lomítka). Fixtures výše jsou tvary pro něj.
- Neověřeno (vyžaduje zápis nebo data): `PUT` s `If-Match`, delta po změně, velikost dávky `multiget`, `507`.
