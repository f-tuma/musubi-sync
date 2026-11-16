# Fáze 2: živá kontrola Google Tasks a People (T10)

Stav: hotovo kromě stáří sync tokenu (K10) · datum: 2026-11-16 · plán: [T10](../../plan/phase2.md#t10-živá-kontrola-google-tasks-a-people-spouští-koordinátor) · fixtures: [`crates/musubi-google/tests/fixtures/live/t10/`](../../../crates/musubi-google/tests/fixtures/live/t10/)

## 1. Otázka

Platí tvary a chování, na kterých stojí K6? Konkrétně: klientské `id` u `tasks.insert`, `updatedMin` se smazanými a skrytými úkoly, `tasklists.updated` jako levná kontrola, sync tokeny People, tvar chyb `403` a neplatného tokenu a zpoždění změny kontaktu v deltě.

## 2. Metoda

Testovací účet Google (stejný jako v K8 fáze 1), vlastní projekt GCP, OAuth se scope `tasks` a `contacts`. Skript volal REST přímo, bez Musubi. Zápisy šly jen do seznamu `musubi-t10` a do kontaktu, které skript sám založil. Testovací účet předtím neměl žádné kontakty a jen výchozí seznam úkolů. Každý krok proběhl jednou (zpoždění čtyřikrát), takže jde o jednotlivé vzorky.

Chybějící scope se ověřil tokenem zúženým při obnově (`grant_type=refresh_token` se `scope` = jen jeden ze dvou scope), ne odškrtnutím na obrazovce souhlasu. Výsledek je stejný: přístupový token bez daného scope.

Uložená těla jsou bez sync a page tokenů, adresa účtu je `owner@example.com` a číslo projektu je nahrazené.

## 3. Google Tasks

| Krok | Výsledek |
| --- | --- |
| `tasks.insert` s `id` v těle (`musubiT10clientId01`; fixture odeslané tělo nenese) | `200`, **server `id` tiše ignoruje** a přidělí vlastní |
| `tasks.delete` | `204` |
| dokončení (`patch status=completed`) | `200`, `completed` vyplněné |
| `tasks/clear` | `204`; dokončený úkol dostane `hidden: true` |
| `list` bez parametrů | jen otevřené úkoly (dokončený a skrytý ani smazaný ne) |
| `list` se `showDeleted` + `showHidden` | i smazaný (`deleted: true`, `status` zůstává `needsAction`) a skrytý (`completed`, `hidden: true`) |
| `list` s `updatedMin` bez příznaků | **smazané ani skryté nevrací** |
| `list` s `updatedMin` (minutu před vložením) + `showDeleted` + `showHidden` | smazané i skryté; všechny úkoly vznikly po `updatedMin`, filtr dokládá až budoucí `updatedMin` |
| `list` s `updatedMin` v budoucnosti | prázdný seznam, bez chyby |

- Inkrement musí posílat `showDeleted=true` i `showHidden=true`; samotné `updatedMin` smazání ztratí.
- Klientské ID se nepřenese, takže nejistý výsledek vytvoření dál řeší pozorování podle obsahu (návrh 12.5).
- **Přejmenování úkolu `tasklists.updated` nemění.** Vložení úkolu ho posune (`tasklists.get` nese čas vložení posledního úkolu). Po smazání, dokončení a vyčištění se podle logu skriptu změnilo, ale fixture to nedokládá a nejde říct, který krok to způsobil. Jako levná kontrola „který seznam číst" proto nestačí: adaptér čte `tasks.list` s `updatedMin` u každého seznamu (plán 6.2).
- Seznam `musubi-t10` s jedním smazaným a jedním skrytým úkolem v účtu zůstal; K10 podle něj ověří, zda je Google po čase odstraňuje (otevřená otázka 6).

## 4. People API

| Krok | Výsledek |
| --- | --- |
| tři úplná čtení hned po sobě (`requestSyncToken=true`) | tři **různé** tokeny |
| delta s tokenem a jinými `personFields` | `200`, přijato |
| neplatný `syncToken` | **`400 INVALID_ARGUMENT`**, bez `ErrorInfo`, text „Request contains an invalid argument." |
| tentýž token podruhé (po deltě, která vrátila nový) | `200`; token není jednorázový |
| smazaný kontakt v deltě | `resourceName`, `etag` a `metadata` s `deleted: true` a `objectType: PERSON`; žádná jiná pole |

- Neplatný token nenese důvod `EXPIRED_SYNC_TOKEN`. Adaptér proto správně bere jakoukoli `400`/`410` s tokenem jako `CursorExpired` (`people.rs`). Tvar **vypršeného** tokenu zůstává neověřený; zjistí ho K10 (otevřená otázka 3).
- Že token přijme jiná `personFields`, neznamená, že delta vrátí nová pole u nezměněných kontaktů. Změna `personFields` má dál znamenat úplné čtení.

### Zpoždění změny v deltě

| Změna | Vidět v deltě po |
| --- | --- |
| `createContact` | 0,1 s |
| `updateContact` (1.) | 0,1 s |
| `updateContact` (2.) | 0,2 s |
| `deleteContact` | 0,1 s |

Plán (K10, D4) počítal s propsáním „několik minut". Na testovacím účtu s jedním kontaktem se změna objevila hned v první deltě. Čtyři vzorky z jednoho dne nejsou rozdělení; K10 změří znovu přes `musubi-syncd`. Odhad „několik minut" v D4 by se měl nahradit naměřenou hodnotou.

### Stáří sync tokenu

Tokeny A a B jsou z úplných čtení 2026-11-16 10:43:31 UTC. A se nepoužívá. B posouvá denní časovač koordinátora (jedna delta denně, uloží se poslední vrácený token). K10 nejdřív 2026-11-24 zkusí deltu s oběma (plán 6.3).

## 5. Chybějící scope a vypnuté API

| Situace | Tasks | People |
| --- | --- | --- |
| token bez scope | `403 PERMISSION_DENIED`, `errors[].reason = insufficientPermissions` **a** `ErrorInfo.reason = ACCESS_TOKEN_SCOPE_INSUFFICIENT` | `403 PERMISSION_DENIED`, jen `ErrorInfo.reason = ACCESS_TOKEN_SCOPE_INSUFFICIENT` |
| API v projektu vypnuté | `403`, `errors[].reason = accessNotConfigured` a `ErrorInfo.reason = SERVICE_DISABLED` | `403`, jen `ErrorInfo.reason = SERVICE_DISABLED` |

- Rozhoduje tedy `ErrorInfo.reason`; starší `errors[].reason` posílá jen Tasks. `provider.rs` hledá oba tvary, takže sedí.
- Token bez scope dostane `ACCESS_TOKEN_SCOPE_INSUFFICIENT`, i když je API vypnuté: scope se kontroluje dřív. Vypnuté API se proto pozná až s úplným tokenem.
- Vypnuté API není chybějící scope. `provider.rs` ho už rozlišuje (`accessNotConfigured`/`SERVICE_DISABLED`).

## 6. Dopad

- K6 (follow-up): nahradit syntetické fixtures `TasksCodec` a `PeopleCodec` tvary z `live/t10/` a rozhodnout `showAssigned`.
- Plán 6.2: `tasklists.updated` jako levná kontrola vypustit.
- Otevřená otázka 3: neplatný token = `400 INVALID_ARGUMENT`; vypršený ověří K10.
- D4 a K10: zpoždění People nahradit měřením.
