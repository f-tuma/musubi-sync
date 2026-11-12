# Fáze 1: živá kontrola proti Google (K8)

Stav: hotovo kromě obnovy kanálu (viz 9) · datum: 2026-11-12 · plán: [§6.6](../../plan/phase1.md#66-živá-kontrola-k8) · fixtures: [`crates/musubi-google/tests/fixtures/live/`](../../../crates/musubi-google/tests/fixtures/live/)

## 1. Otázka

Chová se falešný server K3 jako Google? Jaká je skutečná odezva? Jakou životnost Google dává watch kanálu (T8)? A co přesně rozbilo odběr kalendáře svátků (K8a)?

## 2. Metoda

Testovací účet Google, vlastní projekt GCP s neověřenou aplikací OAuth, `musubi-syncd` s webhookem za veřejným tunelem. Kroky z plánu §6.6 provedl uživatel ručně v UI Google Kalendáře. Časy jsou z logu serveru a z klienta SSE na téže síti. Každý krok proběhl jednou, takže jde o jednotlivé vzorky, ne o percentily.

Zaznamenaná těla jsou bez osobních dat (adresa nahrazená `owner@example.com`) a leží ve fixtures výše. Test `crates/musubi-google/tests/live_fixtures.rs` je pouští produkčním mapováním.

## 3. Připojení a první data

- Od návratu z OAuth (callback) po první synchronizovanou kolekci: **asi 1,4 s**.
- Přesný začátek B1 (t0) chyběl, takže číslo není B1 v pravém smyslu. Zahrnuje výměnu kódu za token, `calendarList` a úplné čtení první kolekce, ne jen okno kolem dneška.

## 4. Watch kanál (T8)

| Požadavek | Vrácená `expiration` |
| --- | --- |
| bez `expiration` | přesně **7,000 dne** |
| `expiration` = teď + 30 dní | **30,0 dne** |

- Engine po zřízení kanálu naplánoval obnovu (ve dvou třetinách životnosti).
- `channels.stop` vrací `204`.
- Google tedy dá aspoň 30 dní, když se o ně řekne. Skutečné maximum zůstává nezdokumentované. Engine si dnes `expiration` neříká, takže dostane 7 dní; delší kanál je možná budoucí úspora požadavků, ne oprava.

## 5. Od uložení v UI Google po událost SSE

S pushem, jeden vzorek na krok:

| Krok | Čas |
| --- | --- |
| vytvoření události | 4,7 s |
| úprava | 3,7 s |
| smazání (tombstone, revize 3) | 4,4 s |
| denní série vytvořena | 7,5 s |
| „Tato událost": změna názvu výskytu | 3,0 s |
| „Tato událost": smazání výskytu | 3,5 s |

- Čas zahrnuje uložení v UI, dobu, než Google pošle oznámení, deltu a commit. Rozklad na čekání na oznámení a vlastní zpracování K8 neměřil.
- Výskyty z `/v1/entities` odpovídaly UI: upravený výskyt je samostatná entita a zrušený výskyt ve výskytech chybí.

## 6. Tvar surových dat

- **Velikost:** JSON těla mají průměrně **1 000 B** (312–1 085 B), 219 těl (209 svátků, 10 vlastních událostí).
- **`dateTime` nese vždy offset**, a to offset kalendáře, ne zóny události.
- **Událost v jiné zóně:** `dateTime` v offsetu kalendáře (`+01:00`), `timeZone` `America/New_York`. Mapování dá 16:30 New York, tedy 21:30Z (správně).
- **`originalStartTime`** má `dateTime` s offsetem a vždy `timeZone`, i u zrušeného výskytu.
- **Zrušený výskyt** nemá `start` vůbec (klíč chybí). Nese jen `kind`, `etag`, `id`, `status`, `recurringEventId` a `originalStartTime`.
- **Celodenní událost** je `{date}`.
- **Bez účastníků** přijde jen `organizer` a `creator` se `self:true`; `responseStatus` chybí.
- `eventType` je `"default"`; `visibility` u výchozí viditelnosti chybí.

## 7. Kalendář svátků (K8a)

`en-gb.czech#holiday@group.v.calendar.google.com`, `accessRole: reader`:

- úplné čtení vrátilo 209 položek **a nese `nextSyncToken`**;
- delta s tímto tokenem hned vrací **`410 fullSyncRequired`**;
- `events.watch` vrací **`400 pushNotSupportedForRequestedResource`**.

Pojistka z K8a držela: po třech úplných čteních odklad 300 s, zdvojovaný až do 1 h. Kolekce se jen dotazovala a watch se znovu zkoušel po 24 h. Kolekce ale nesla chybu napořád, i když se nic nerozbilo.

## 8. Neplatné kurzory a zjišťování

| Požadavek | Odpověď |
| --- | --- |
| vymyšlený nebo poškozený `pageToken` | `400 invalid` („Invalid page token value") |
| vymyšlený `syncToken` | `410 fullSyncRequired` |
| platný `pageToken` znovu, i s jinými parametry | `200` |

- Nově odebíraný kalendář se objeví až při dalším zjišťování, výchozí interval je 6 h.
- Restart (SIGTERM, stop a start) proběhl čistě; klienti SSE se znovu připojili.

## 9. Ještě otevřené

- **Obnova kanálu 17. 11.** (krok 6 plánu). Výsledek doplní koordinátor.

## 10. Odchylky a co se opravilo (K8b)

| Zjištění | Dřív | Teď |
| --- | --- | --- |
| čerstvý `syncToken` hned `410` (svátky) | kolekce v odkladu s chybou napořád | po dvou takových tokenech za sebou engine čte kolekci jen úplně, v běžném intervalu dotazování, bez chyby (stav v paměti do restartu) |
| `400` na `pageToken` | každá `400` u stránky = `CursorExpired` | jen `410` je `CursorExpired`; `400 invalid` u `pageToken` začne stránkování téhož průchodu znovu, nejvýš dvakrát, pak přechodná chyba; jiná `400` je chyba |
| organizátor bez záznamu účastníka | `needs-action` | `accepted` (`MAPPING_VERSION` 2, uložené události se přepočítají) |
| chybějící `visibility` | `public` | beze změny: `public` je výchozí hodnota JSCalendar |
| obnova kanálu, kterou Google odmítne | řádek kanálu zůstal a vypadal jako živý | řádek se smaže a kolekce se jen dotazuje, stejně jako po prvním odmítnutí; watch se znovu zkusí po 24 h |
| úryvky těl odpovědí v logu | řídicí znaky prošly a šlo jimi podvrhnout řádek logu | řídicí znaky se escapují; log o prošlém kurzoru nenese id kalendáře (u primárního je to e-mail) |
| falešný server | `dateTime` v zóně, v jaké byl zapsán; `originalStartTime` bez `timeZone` | `dateTime` v offsetu kalendáře, `originalStartTime` vždy s `timeZone`, zrušený výskyt bez `start` |

Odchylky fake od Googlu a jejich ověření jsou v [`crates/musubi-testkit/FIDELITY.md`](../../../crates/musubi-testkit/FIDELITY.md).

## 11. Důsledky pro návrh a plán

- **Kapitola 5.1 návrhu:** první data po připojení naměřena asi 1,4 s pro celou první kolekci; cíl „pod 1 s pro okno" tím není potvrzený ani vyvrácený. Změna u Googlu do proudu změn trvala 3,0–7,5 s od uložení v UI; cíl „pod 500 ms od přijetí oznámení" K8 neměřil odděleně.
- **Komprese surové vrstvy** (plán §8): těla kolem 1 000 B jsou zhruba čtvrtina syntetického odhadu 4,4 KB na celou událost. Ve fázi 1 zůstává surová vrstva nekomprimovaná. Úsporu zstd po řádcích na tak malých tělech jsme na živých datech neměřili.
