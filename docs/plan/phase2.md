# Fáze 2: Druhý adaptér a celá rodina pro čtení — implementační plán

Stav: návrh plánu, 12. 11. 2026. Vychází z [návrhu architektury, revize 7](../architecture/proposal.md), z [plánu fáze 1](phase1.md), z výsledků [fáze 0](../research/phase0/) a z [živé kontroly Googlu](../research/phase1/live-google.md). Co není ověřené, je označeno **(neověřeno)**. Rozhodnutí z kapitoly 11.4 návrhu plán neotevírá.

## 1. Cíl a kritéria hotovosti

Fáze 2 přidává druhý, protokolově úplně jiný adaptér: obecný CalDAV a CardDAV s iCloudem jako profilem. Přidává druhy úkol a kontakt, u Googlu Tasks a People API, a adaptivní dotazování pro kolekce bez pushe. Všechno jen pro čtení. Na konci se rozhraní adaptéru zmrazí jako v0.

Kritéria jsou z kapitoly 11.1 návrhu, rozepsaná tak, aby každé mělo test:

| Kritérium | Kde se dokazuje |
| --- | --- |
| stejná konformační sada prochází pro Google (falešný server) i DAV (falešný server a Radicale) | `cargo test -p musubi-testkit --test conformance`, job `conformance`, K9 |
| obecný CalDAV a CardDAV: události, VTODO, kontakty a skupiny kontaktů | konformační sada proti Radicale, K9 |
| iCloud: partition host, heslo pro aplikace, známé odchylky | scénáře profilu `icloud` proti falešnému DAV, K7; zpráva T11 |
| Google Tasks a People: úvodní čtení, inkrement, smazání, vypršený token | konformační sada proti falešnému Googlu, K6 a K9; zpráva T10 |
| adaptivní dotazování: meze intervalu, zpomalení v klidu, zrychlení při zájmu | testy enginu s `FakeProvider` a pevnými hodinami, K5 |
| `kill -9` během čtení DAV bez ztráty a duplicit | crash test proti falešnému DAV, K9 |
| rozhraní adaptéru zmrazeno jako v0 | test `contract_freeze`, K9 (kapitola 3.8) |
| živá kontrola Google Tasks a People | T10 před K6, K10 |
| živá kontrola iCloudu | T11 před K7, K10 |

Cíle latence z fáze 1 platí dál pro Google. Pro DAV, Tasks a People je latence shora omezená intervalem dotazování (kapitola 7). Plán to přiznává čísly, ne slibem (kapitola 11.2).

## 2. Workspace

Přibývají dva crates. Každý má jednu odpovědnost, kterou dnešní crates nemají.

| Crate | Odpovědnost | Závisí na (interně) |
| --- | --- | --- |
| `musubi-ical` (nový) | kodeky nad `calcard`: iCalendar VEVENT → `CalendarEvent`, VTODO → `CalendarTask`, vCard → `ContactCard` a `ContactGroup`; řetěz zón včetně vlastní VTIMEZONE; `kind_of(&Raw)` (levné rozpoznání druhu objektu bez parsování celého těla). Čisté funkce, bez I/O | `musubi-model` |
| `musubi-dav` (nový) | `Provider` pro CalDAV a CardDAV: HTTP klient, XML (`quick-xml`), zjišťování RFC 6764, delta přes `sync-collection` nebo ctag+etag, `multiget`, ochrana odchozích adres, profily `generic` a `icloud` jako data | `musubi-model`, `musubi-ical` |

Google Tasks a People zůstávají v `musubi-google`. Jsou to další kolekce téhož účtu Google: stejný OAuth klient, stejná obnova tokenu, stejný HTTP klient, omezovač i zjišťování. Samostatný crate by tohle všechno zdvojil nebo by musel záviset na `musubi-google`. Modul `musubi-google::tasks` a `musubi-google::people` s vlastními kodeky (`TasksCodec`, `PeopleCodec`) stačí.

Vrstva záplat (`musubi-ical::patch`) ve fázi 2 nevzniká. Potřebují ji až zápisy (fáze 3). Pravidla pro ni z [calcard.md](../research/phase0/calcard.md) platí dál a fáze 2 je nesmí porušit: žádné `to_string()` celého objektu a žádná cesta JSCalendar/JSContact → iCalendar/vCard v produkčním kódu.

Směr závislostí:

```
                              musubi-syncd (bin)
     ┌────────────┬───────────────┬────┴────────┬──────────────┐
     ▼            ▼               ▼             ▼              ▼
 musubi-engine  musubi-store-   musubi-google  musubi-dav   (musubi-testkit: jen dev)
   │     │      sqlite            │             │     │        │
   │     ▼        │               │             │     ▼        │
   │  musubi-     │               │             │  musubi-ical │
   │  recurrence  │               │             │     │        │
   ▼              ▼               ▼             ▼     ▼        ▼
 ──────────────────────────────── musubi-model ───────────────────
```

Tabulka `ALLOWED` v `crates/musubi-testkit/tests/dependency_direction.rs` po fázi 2:

| Crate | Smí záviset na |
| --- | --- |
| `musubi-model` | — |
| `musubi-recurrence` | — |
| `musubi-store-sqlite` | `musubi-model` |
| `musubi-google` | `musubi-model` |
| `musubi-ical` | `musubi-model` |
| `musubi-dav` | `musubi-model`, `musubi-ical` |
| `musubi-engine` | `musubi-model`, `musubi-recurrence` |
| `musubi-syncd` | `musubi-model`, `musubi-recurrence`, `musubi-store-sqlite`, `musubi-google`, `musubi-ical`, `musubi-dav`, `musubi-engine` |
| `musubi-testkit` | `musubi-model` |

`DEV_FORBIDDEN` dostane vedle `(musubi-engine, musubi-google)` také `(musubi-engine, musubi-dav)` a `(musubi-engine, musubi-ical)`. Engine zná jen traity a kanonické typy; kodeky dostává z registru (kapitola 3.6).

Pravidla:

- Tabulku i test mění ten kontrolní bod, který crate přidává (K0 přidá oba crates jako kostru). Bez toho test zastaví CI, protože nový člen workspace v tabulce chybí.
- Tabulka zůstává uzavřená: `musubi-dav` smí `musubi-ical` a ten smí jen `musubi-model`.
- `musubi-testkit` dál závisí jen na `musubi-model`. Falešný DAV server a harness Radicale mluví HTTP, ne Rustem adaptéru.
- `CONTRIBUTING.md` dostane oba crates do popisu vrstev.

Nové externí závislosti a proč:

| Závislost | Kde | Proč |
| --- | --- | --- |
| `calcard` `=0.3.x` (dnes 0.3.14 z 15. 9. 2026) | `musubi-ical` | rozhodnuto ve fázi 0; přesný pin, protože serializace normalizuje a změna patch verze může změnit chování |
| `quick-xml` (s `serialize`) | `musubi-dav` | WebDAV odpovědi; malé, bez C |
| žádný nový HTTP klient | `musubi-dav` | stejný `reqwest` (rustls, `ring`) jako `musubi-google` |

Licence obou nových závislostí je potřeba ověřit v K0 přes `cargo deny check` **(neověřeno: calcard deklaruje Apache-2.0 OR MIT)**. DNS SRV z RFC 6764 se nedělá (kapitola 5.1), proto `hickory-resolver` nepřibývá.

## 3. Změny modelu a kontraktu adaptéru

Všechny změny v této kapitole dělá K0 v `musubi-model` naráz. Ostatní kontrolní body je jen implementují.

### 3.1 Nové druhy

`EntityKind` dostane `Task`, `Contact` a `ContactGroup` (je `#[non_exhaustive]`, takže změna je aditivní). Jména v datech: `task`, `contact`, `contact_group` (kapitola 2.7 návrhu). Každý druh má značku s `Kind`:

| Značka | `KIND` | `SCHEMA_VERSION` | `Canonical` |
| --- | --- | --- | --- |
| `Event` | `Event` | 2 (vlastní zóny, kapitola 3.7) | `CalendarEvent` |
| `Task` | `Task` | 1 | `CalendarTask` |
| `Contact` | `Contact` | 1 | `ContactCard` |
| `ContactGroup` | `ContactGroup` | 1 | `ContactGroup` |

**`CalendarTask`** je profil JSCalendar Task (RFC 8984, kapitola 5.2). Pole:

| Pole | Zdroj VTODO | Zdroj Google Tasks |
| --- | --- | --- |
| `uid`, `recurrence_id` | `UID`, `RECURRENCE-ID` | `id`, — |
| `title`, `description` | `SUMMARY`, `DESCRIPTION` | `title`, `notes` |
| `start`, `due`, `time_zone`, `show_without_time` | `DTSTART`, `DUE`, TZID, hodnota DATE | —, `due` (jen datum), —, `true` |
| `estimated_duration` | `DURATION` | — |
| `progress` (`needs-action`, `in-process`, `completed`, `failed`, `cancelled`) | `STATUS` | `status` (`needsAction`, `completed`) |
| `progress_updated`, `percent_complete` | `COMPLETED`, `PERCENT-COMPLETE` | `completed`, — |
| `priority` | `PRIORITY` | — |
| `related_to` (rodič) | `RELATED-TO;RELTYPE=PARENT` | `parent` |
| `recurrence_rules`, `recurrence_overrides`, `alerts`, `locations`, `keywords` | jako u události | — |
| `created`, `updated`, `sequence` | `CREATED`, `LAST-MODIFIED`, `SEQUENCE` | —, `updated`, — |

Pořadí Google úkolu (`position`), `hidden`, `links` a `assignmentInfo` zůstávají jen v surové vrstvě. Kanonický tvar je pohled, ne úplná kopie.

**`ContactCard`** je profil JSContact Card (RFC 9553) pro `kind` `individual`, `org` a `location`. Pole: `uid`, `kind`, `name` (složky a `full`), `nicknames`, `organizations`, `titles`, `emails`, `phones`, `addresses`, `online_services`, `links`, `notes`, `anniversaries` (narozeniny), `photos` (jen URI nebo značka „vloženo, `n` bajtů", bez bajtů), `updated`. Pořadí a preference položek (`pref`) se zachovají.

**`ContactGroup`** je JSContact Card s `kind: group`: `uid`, `name`, `members` (množina UID členů). Kodek vybírá druh podle obsahu: vCard 4 `KIND:group` nebo Apple `X-ADDRESSBOOKSERVER-KIND:group` je `ContactGroup`, jinak `Contact`. Členy u Applu nese `X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:…`.

Fotky se v kanonickém tvaru neukládají. Vložená fotka zůstane v surové vrstvě. Úložiště fotek mimo databázi (kapitola 2.7 návrhu) je mimo rozsah (kapitola 11.4).

### 3.2 Druh objektu a kolekce

CalDAV kalendář může míchat VEVENT a VTODO (Radicale to dovolí; iCloud ne **(neověřeno)**). Adresář míchá kontakty a skupiny. Druh proto nese objekt, ne kolekce:

- `RemoteObject` dostane `kind: EntityKind`. Adaptér ho vyplní: Google podle kolekce, DAV přes `musubi_ical::kind_of`. Objekt bez podporovaného druhu (VJOURNAL, VFREEBUSY) adaptér přeskočí a zapíše do logu na úrovni `debug`.
- `RemoteCollection.kind` zůstává jako hlavní druh kolekce pro výpis kolekcí: kalendář je `Event`, kalendář jen s VTODO je `Task`, seznam úkolů Googlu je `Task`, adresář je `Contact`.
- Položka se ukládá s druhem objektu (`items.kind` a `entities.kind` už existují).

### 3.3 Surový objekt a validátor

- `MediaType` dostane `Calendar` (`text/calendar`) a `VCard` (`text/vcard`). V databázi `calendar` a `vcard`.
- `Validator` u DAV je hodnota `getetag` beze změny, včetně uvozovek a případného `W/`. Porovnává se jen na rovnost. Silné ETag pro zápis s `If-Match` řeší fáze 3; slabý ETag tam znamená `conditional_update: false` pro tu kolekci. Vzor: `packages/provider-caldav/src/native-etag.ts` v Musubi.
- Server bez `getetag` (nebo s prázdným): validátor je `musubi:sha256:<hex>` z těla. Takový validátor nikdy nejde do `If-Match`; adaptér to pozná podle předpony.
- Google Tasks a People: validátor je `etag` z JSON.
- `RemoteObject.raw` je u DAV přesné tělo z `calendar-data` nebo `address-data` po dekódování XML entit. Žádná normalizace konců řádků ani skládání.

### 3.4 Stránka změn: objekty bez těla

DAV vrací seznam změn (adresa a ETag) bez těla. Těla stahuje `multiget` a stahovat se mají jen ta, jejichž ETag se změnil. Rozhodnout to umí jen engine, protože jen on vidí uložené validátory.

`ChangePage` dostane pole `stubs: Vec<Stub>`, `Stub { key: RemoteKey, validator: Option<Validator> }`:

- Engine zjistí uložené validátory přes existující `TenantStore::items_by_remote`.
- Stejný validátor: klíč jde do `PageWrite::unchanged` (jako dnes u úplného čtení).
- Jiný nebo chybějící validátor, nebo nový klíč: engine je po dávkách `limits.max_batch` stáhne přes `Provider::fetch` a zapíše je jako `upserts` téže stránky, v jedné transakci s kurzorem stránky.
- Objekt, který `fetch` nevrátí (smazaný mezi výpisem a stažením), se v té stránce nezapíše. U úplného čtení ho pak smaže nepřítomnost; u delty ho přinese další `sync-collection` jako odebraný.
- Kurzor stránky se uloží až po stažení všech jejích těl, takže pád mezi výpisem a stažením stránku zopakuje.

Google dál vrací jen `upserts`. Jeden kanál pro obě cesty (seznam ETagů u ctag+etag i výsledek `sync-collection` bez dat) je jednodušší než dvě varianty stránky.

### 3.5 Kurzor, schopnosti po kolekcích a pověření

**Kurzor zůstává neprůhledný** (`Cursor(Vec<u8>)`). Varianty jsou vnitřní věc adaptéru, kódované jako JSON s verzí:

| Adaptér | Varianty |
| --- | --- |
| DAV | `SyncToken { token }`; `Ctag { ctag }` (kolekce bez `sync-collection`); `Listing { since: Option<…> }` (rozpracované úplné čtení) |
| Google Tasks | `UpdatedMin { ts, page: Option<token> }` |
| Google People | `SyncToken { token, page: Option<token> }` |
| Google Calendar | beze změny |

**Delta a push po kolekcích.** Dnes jsou `delta` a `push` v `ProviderDescriptor` jednou hodnotou na poskytovatele. Google teď míchá kalendáře (sync token a webhook), úkoly (okno podle `updatedMin`, bez pushe) a kontakty (sync token, bez pushe). DAV to zjistí až u kolekce (`supported-report-set`). Proto:

- `DeltaSupport` a `PushSupport` se přesunou do `KindCapabilities` jako výchozí hodnoty druhu.
- `RemoteCollection` dostane `delta: Option<DeltaSupport>` a `push: Option<PushSupport>` (serde default `None` = výchozí hodnota druhu). DAV je vyplní při zjišťování.
- `collections.meta` je JSON, takže nová pole nepotřebují migraci SQL. Starý řádek se přečte s `None`.
- `Limits` dostane `max_concurrency_per_account: Option<u32>` (iCloud 2, obecný DAV 2, Google bez omezení). Engine ho vynucuje (kapitola 7.4).

**Pověření.** DAV používá Basic s heslem (u iCloudu heslo pro aplikace). OAuth pro DAV ve fázi 2 není.

- `AccountCx.token: SecretString` se nahradí `AccountCx.credential: Credential`, `enum Credential { Bearer(SecretString), Basic { username: String, password: SecretString } }`.
- `AccountTokens` zůstane jako varianta: `enum Credentials { OAuth(AccountTokens), Basic { username, password } }`. `TokenVault::load` a `store` pracují s `Credentials`. Zapečetí se stejně jako dnes do `accounts.tokens` (AES-256-GCM, AAD s tenantem a účtem). Starý blob bez značky varianty se přečte jako `OAuth` (test v K1).
- `Provider::refresh` se přesune do `trait TokenRefresh` a `Provider` dostane `fn oauth(&self) -> Option<&dyn TokenRefresh>` s výchozím `None`, stejně jako `push()`. DAV ho nemá. Engine u účtu bez `oauth()` nikdy neobnovuje; adaptér DAV mapuje `401` rovnou na `ReauthRequired`.
- Heslo se nikdy neloguje, nevrací přes API a v paměti je jen jako `SecretString`.

**Adresa účtu.** DAV účet potřebuje nesoukromý stav: adresu serveru, profil, principal, home sety, partition host. Ten patří k účtu, ne do zapečetěných tajemství:

- `AccountCx` dostane `endpoint: Option<Endpoint>`, `Endpoint { base: String, state: serde_json::Value }`. Google ho nemá.
- `Discovery` dostane `endpoint: Option<Endpoint>`: adaptér vrátí aktualizovaný stav (nalezený principal a home sety) a engine ho uloží (`ControlStore::set_endpoint`).
- `SyncError` dostane `Rediscover`: kolekce nebo principal vrátí `404`, nebo přesměruje mimo uložený host. Engine zařadí `Discover` účtu a úlohu kolekce odloží za něj.
- `Discovery` dostane `unauthorized: Vec<EntityKind>`: druhy, ke kterým účet nemá oprávnění (Google bez scope pro Tasks nebo People, kapitola 6.3). Kolekce těchto druhů se nemažou.

### 3.6 Engine pro víc poskytovatelů a druhů

Dnes je `Engine<S: Storage, C: Codec<Event>>` s jedním poskytovatelem. Fáze 2:

- `Engine<S: Storage>` dostane registr `Providers`: `ProviderId` → `Arc<dyn Provider>` a kodeky po druzích.
- Kodeky zůstávají typované, bez mazání typů: `struct Codecs { event: Option<DecodeFn<Event>>, task: Option<DecodeFn<Task>>, contact: Option<DecodeFn<Contact>>, contact_group: Option<DecodeFn<ContactGroup>> }`, kde `DecodeFn<K> { mapping_version: u32, decode: fn(&Raw) -> Decoded<K::Canonical> }` a `DecodeFn::of::<C: Codec<K>>()`.
- Objekt druhu, pro který poskytovatel kodek nemá, se uloží jen surově (`canonical: None`), jako `Decoded::Unsupported`.
- Index výskytů počítá engine jen pro `Event`. Úkoly a kontakty výskyty nemají (kapitola 4.2).
- `ProviderId` účtu vybírá instanci: `google`, `dav` a `icloud`. `dav` a `icloud` jsou dvě instance `musubi-dav` s různým profilem. Engine nikde nevětví podle `ProviderId`; test v K5 to hlídá tím, že pouští stejné testy se dvěma `FakeProvider` s různým popisem.
- Engine musí přijmout `Pass::Full` i od proudu, který začal s kurzorem delty: kolekce s ctag+etag při změně ctag vypíše celou kolekci a nepřítomnost znamená smazání.

### 3.7 Zóny

Řetěz pro TZID v iCalendaru (`musubi-ical::tz`), v tomto pořadí:

1. IANA jméno (přímo v `jiff` tzdb);
2. `X-LIC-LOCATION` v VTIMEZONE;
3. Windows jméno přes mapu CLDR (přibalená data, pin verze CLDR);
4. známé předpony (`/mozilla.org/…/`, `/softwarestudio.org/…/`) uříznuté na IANA jméno;
5. vlastní VTIMEZONE převedená na pravidlo POSIX TZ (`jiff` ho umí vyhodnotit), pokud má nejvýš jedno pravidlo STANDARD a jedno DAYLIGHT s `RRULE` tvaru `FREQ=YEARLY;BYMONTH=…;BYDAY=…`;
6. jinak `Decoded::Unsupported { reason: "custom VTIMEZONE" }`, nahlas. Nikdy tiché UTC (calcard neznámou VTIMEZONE bere jako UTC, [calcard.md](../research/phase0/calcard.md)).

Kanonická událost s vlastní zónou nese `time_zone: "/<TZID>"` a novou mapu `time_zones` s pravidlem (JSCalendar `timeZones`). Proto `Event::SCHEMA_VERSION` = 2. Google zóny nemění; uložené položky Googlu se přepočtou jednorázově (kapitola 10, bod 1).

### 3.8 Co znamená „zmrazeno jako v0“

Kontrakt v0 tvoří veřejné položky těchto modulů `musubi-model`: `provider`, `capability`, `remote`, `kind` a kanonické typy (`event`, `task`, `contact`). Tedy traity `Provider`, `PushSource`, `TokenRefresh`, `Codec` a vše, co v nich je.

- Zmrazení nastane na konci K9, až oba adaptéry projdou stejnou sadou. Ne dřív (zamítnutá alternativa v kapitole 6.3 návrhu).
- `docs/contract/v0.md` popíše kontrakt slovy a nese SHA-256 normalizovaného textu těch souborů (bez komentářů a prázdných řádků). Test `crates/musubi-testkit/tests/contract_freeze.rs` hash spočítá a porovná. Každá změna kontraktu tak musí v tomtéž PR upravit `v0.md`.
- **Aditivní změna** (nová varianta `#[non_exhaustive]` výčtu, nové pole se serde default, nová metoda traitu s výchozí implementací) je v0.x: záznam do `docs/contract/v0.md` s důvodem a nový hash.
- **Lámající změna** (změna signatury, odebrání, změna významu) znamená v1: nový soubor `docs/contract/v1.md`, všechny adaptéry upravené v tomtéž PR a celá konformační sada zelená. Před vydáním 1.0 je to dovolené, ale nikdy potichu.
- Zápisy fáze 3 přijdou aditivně (kapitola 3.9), takže v0 nezlomí.

### 3.9 Tvar zápisů pro fázi 3 (jen návrh, nic se neimplementuje)

Fáze 2 jen hlídá, aby čtecí kontrakt nesl, co zápisy potřebují: validátor beze změny (pro `If-Match`), surový objekt (pro záplatu), `conditional_update`, `conditional_delete` a `client_id` v `KindCapabilities`. Fáze 3 pak přidá:

```rust
// fáze 3, aditivně k v0
fn writer(&self) -> Option<&dyn Writer> { None }   // nová metoda Provider

trait Writer: Send + Sync {
    async fn apply(&self, cx: &AccountCx, col: &CollectionRef, op: WriteOp)
        -> Result<WriteOutcome, SyncError>;
}
enum WriteOp {
    Create { kind: EntityKind, raw: Raw, client_id: Option<String> },
    Update { key: RemoteKey, if_match: Validator, raw: Raw },
    Delete { key: RemoteKey, if_match: Option<Validator> },
}
enum WriteOutcome {
    Done { key: RemoteKey, validator: Option<Validator>, raw: Option<Raw> },
    Conflict { current: Option<RemoteObject> },   // 412, Google 412, People 400 failedPrecondition
    Uncertain,                                    // timeout po odeslání: rozhodne čtení
}
```

Kodek dostane `encode(base: &Raw, patch: &CanonicalPatch) -> Result<Raw, Unsupported>` přes vrstvu záplat. Tvar je orientační; rozhodne plán fáze 3.

### 3.10 Drobnosti kontraktu z fáze 1

- Dokumentace `PushSource::subscribe` doplní: `RateLimited`, `ReauthRequired` a `Unauthorized` (a trvalé odmítnutí `Permanent`) znamenají „kanál nevznikl"; engine pak čekající řádek smaže. `Transient` znamená „nevím". Engine se tak už chová (K9 fáze 1), jen to kontrakt neříkal.

## 4. Úložiště

### 4.1 Tenant: `MAIN_V2`

Jedna nová migrace v `MAIN_MIGRATIONS`, líná při otevření jako dosud:

```sql
ALTER TABLE collections ADD COLUMN push_resource TEXT;
CREATE INDEX entities_kind ON entities(kind, id) WHERE tombstone = 0;
```

- `collections.push_resource`: Google `resourceId` kalendáře. Engine ho zapíše po prvním úspěšném `subscribe` a při výměně kanálu ho nemaže (všechny kanály kalendáře mají stejné `resourceId`, živě ověřeno 12. 11. 2026). Slouží k zastavení čekajícího kanálu, když otevřený kanál téže kolekce neexistuje (kapitola 10, bod 3).
- `entities_kind`: výpis entit jednoho druhu bez časového okna.

Nové operace `TenantStore`:

| Operace | Co dělá |
| --- | --- |
| `list_entities(kind, collections, after: Option<EntityId>, limit)` | živé entity druhu s umístěním v daných kolekcích, po `id`, stránkovaně (keyset). Pro úkoly a kontakty |
| `stale_items(kind, schema_version, mapping_version, after, limit)` | živé položky druhu s jinou verzí schématu nebo mapování; pro přepočet (kapitola 10, bod 1) |
| `redecode(items: Vec<ItemWrite>)` | zapíše nový kanonický tvar a verze bez nového surového těla; žurnál jen u položek, kde se kanonický tvar změnil |

Meta klíč `mapping:<provider>:<kind>` (hodnota = verze mapování a schématu, po kterých přepočet doběhl) zajistí, že se přepočet spustí jednou.

### 4.2 Výskyty

- **Úkoly:** ve fázi 2 bez indexu výskytů. Opakovaný VTODO zůstane v kanonickém tvaru s pravidlem, nerozbaluje se. Rozsah podle termínu úkolu je otevřená otázka 5.
- **Kontakty a skupiny:** výskyty nemají a mít nebudou.
- Indexy kontaktů podle e-mailu, telefonu a předpony jména (kapitola 2.7 návrhu) ve fázi 2 nevznikají. Čtecí API je nepotřebuje; přijdou s vyhledáváním (kapitola 11.3).

### 4.3 `control.db`: verze schématu a migrace

Dnes `CONTROL` používá `CREATE TABLE IF NOT EXISTS` a jednorázové `migrate_control`. Verze chybí, takže starší binárka by novější soubor otevřela a poškodila. Fáze 2:

- `meta` dostane klíč `schema`. `CONTROL_MIGRATIONS[i]` vede z verze `i` na `i + 1`, stejný `migrate` jako u tenanta.
- Soubor bez klíče `schema` je verze 1, pokud má tabulku `accounts`: projde dnešním `CONTROL` a `migrate_control` a dostane `schema = 1`. Prázdný soubor dostane rovnou poslední verzi.
- Verze 2: `ALTER TABLE accounts ADD COLUMN endpoint TEXT` (JSON `Endpoint`, kapitola 3.5).
- Soubor s vyšší verzí, než binárka zná, je `SchemaTooNew`. `serve` skončí s chybou a soubor nezmění.

Nové operace `ControlStore`: `set_endpoint(account, &Endpoint)`, `endpoint(account)`. `TokenVault` pracuje s `Credentials` (kapitola 3.5).

## 5. DAV: zjišťování, delta, iCloud, pověření

### 5.1 Zjišťování

Postup podle RFC 6764 kapitola 5, RFC 4791 a RFC 6352. Zvlášť pro CalDAV a CardDAV, protože iCloud má pro každé jiný host:

1. **Výchozí adresa.** Profil `icloud`: `https://caldav.icloud.com/` a `https://contacts.icloud.com/`. Profil `generic`: adresa od uživatele.
2. **Well-known.** Má-li adresa prázdnou cestu, `PROPFIND` na `/.well-known/caldav` (`/.well-known/carddav`) s přesměrováním. Bez well-known se použije adresa sama.
3. **Principal.** `PROPFIND` hloubka 0 na `DAV:current-user-principal`. Záloha: `DAV:principal-URL`, pak adresa sama.
4. **Home sety.** `PROPFIND` hloubka 0 na principal: `C:calendar-home-set`, `CR:addressbook-home-set`. Host z odpovědi (iCloud partition host `pNN-caldav.icloud.com`) se uloží do `Endpoint.state`.
5. **Kolekce.** `PROPFIND` hloubka 1 na každý home set: `resourcetype`, `displayname`, `C:supported-calendar-component-set`, `D:supported-report-set`, `CS:getctag`, `D:sync-token`, `D:current-user-privilege-set`, `C:calendar-timezone`.

Pravidla:

- Kolekce je `calendar` nebo `addressbook` z `resourcetype`. Schedule inbox a outbox, notifikace a odběry (`CS:subscribed`) se přeskočí.
- Druh kalendáře podle `supported-calendar-component-set`: obsahuje VEVENT → `Event`; jen VTODO → `Task`; nic z toho → přeskočit. Chybí-li vlastnost, `Event` (RFC 4791: chybějící = všechny komponenty).
- Delta kolekce: `sync-collection` v `supported-report-set` → `SyncToken`; jinak `getctag` → `CtagEtag`; jinak `None` (každé čtení je výpis ETagů).
- Přístup z `current-user-privilege-set`: `write-content` nebo `write` → `Writer`, jinak `Reader`. Kolekce bez vlastnosti pod vlastním home setem je `Owner` (iCloud oprávnění nehlásí, kapitola 2.6 návrhu).
- Klíč kolekce (`RemoteKey`) je absolutní cesta kolekce. Klíč objektu je absolutní cesta objektu. Host do klíče nepatří; je v `Endpoint`.
- Subjekt účtu je URL principalu. Dva účty se stejným principalem téhož tenanta jsou stejný účet (`UNIQUE(tenant, provider, subject)`).
- DNS SRV (`_caldavs._tcp`) se nedělá. Uživatel zadá adresu serveru; well-known na ní stačí pro běžné servery **(neověřeno pro všechny)**.

### 5.2 Delta

| Start | Kolekce s `sync-collection` | Kolekce s ctag | Bez obojího |
| --- | --- | --- | --- |
| `Window` (první dojem, jen události) | `calendar-query` s `time-range` kolem dneška, `getetag` a `calendar-data`; stránky `Pass::Window` | stejně | stejně |
| `Full` | `sync-collection` s prázdným tokenem, jen `getetag` → `stubs`, `Pass::Full`; poslední stránka nese nový token | `getctag` hloubka 0, pak `PROPFIND` hloubka 1 s `getetag` → `stubs`, `Pass::Full`; kurzor `Ctag` | `PROPFIND` hloubka 1 s `getetag` → `stubs`, `Pass::Full` |
| delta s kurzorem | `sync-collection` s tokenem → `stubs` změněných a `removals` (odpověď se stavem `404`), `Pass::Delta` | `getctag` hloubka 0: stejný → jedna prázdná poslední stránka (levná kontrola, jeden požadavek); jiný → výpis ETagů jako u `Full` | výpis ETagů jako u `Full` |

- Stránkování: Radicale vrací celý výsledek najednou, iCloud může zkrátit odpovědí `507` s `D:number-of-matches-within-limits` **(neověřeno)**. Adaptér dělí výsledek na stránky `max_page_size`. Zkrácená odpověď znamená stránku, která není poslední, a pokračování s vráceným tokenem.
- Rozpracované úplné čtení po pádu začne výpis znovu. Už uložené objekty se vrátí jako `stubs` se stejným validátorem, takže stojí jen výpis, ne nové stažení.
- `multiget`: `C:calendar-multiget` nebo `CR:addressbook-multiget`, dávky `limits.max_batch` (obecně 50, iCloud 20 **(neověřeno)**, kapitola 2.6 návrhu říká „v desítkách").
- Okno prvního dojmu je jen u kalendářů s `Event`. Úkoly a kontakty začínají rovnou `Full`.

### 5.3 Neplatný token a chyby

| Odpověď | `SyncError` |
| --- | --- |
| `403` nebo `409` s `DAV:valid-sync-token` (Radicale vrací `403`) | `CursorExpired` → úplné čtení, smazání nepřítomností až po něm |
| `507` u `sync-collection` | není chyba: stránka s `last: false` |
| `401` | `ReauthRequired` (Basic nemá obnovu) |
| `403` bez podmínky, `405`, `501` na REPORT | kolekce přejde na nižší úroveň delty (`SyncToken` → `CtagEtag` → `None`), zapíše se do `RemoteCollection.delta` při dalším zjišťování; jinak `Permanent` |
| `404` kolekce, `301`/`308` mimo uložený host | `Rediscover` |
| `429`, `503` s `Retry-After` | `RateLimited { retry_after }` |
| jiné `5xx`, timeout, chyba sítě | `Transient` |
| nečitelné XML | `Permanent` |

### 5.4 Profil iCloud

Profil je data v `musubi-dav::profile`, ne větvení kódu:

| Vlastnost | `generic` | `icloud` |
| --- | --- | --- |
| výchozí adresy | od uživatele | `caldav.icloud.com`, `contacts.icloud.com` |
| povolené hosty | host od uživatele a hosty z jeho přesměrování | `^(caldav\|p[0-9]+-caldav)\.icloud\.com$`, `^(contacts\|p[0-9]+-contacts)\.icloud\.com$`; jen `https`, bez portu |
| objekt na jiném originu než jeho kolekce | odmítnout | odmítnout (vzor `isIcloudPersonalContentDestination` v Musubi) |
| souběžnost na účet | 2 | 2 (kapitola 2.6 návrhu, I5) |
| `multiget` dávka | 50 | 20 **(neověřeno)** |
| oprávnění kolekce | z `current-user-privilege-set` | chybí: vlastní home set = `Owner` |
| VTODO | ano | kolekce jen s VTODO se přeskočí (úkoly iCloudu nejsou ve v1, kapitola 2.6 návrhu) |
| vCard | 3.0 i 4.0 | 3.0 |
| `sync-collection` u CardDAV | z `supported-report-set` | z `supported-report-set`; T11 ověří **(neověřeno)** |
| `503` dávky | běžné zpomalení | běžné zpomalení; I5 |

Heslo pro aplikace: iCloud přijme Basic jen s heslem pro aplikace (účet s 2FA). Změna hesla Apple ID všechna hesla pro aplikace zneplatní (I6). `401` proto znamená výzvu uživateli, ne opakování. Profil přidá do chyby účtu text „vytvořte nové heslo pro aplikace".

Odchylky z I2 (iCloud ignoruje `SCHEDULE-AGENT=CLIENT`, `412` na zastaralý `If-Match`, přepis vlastních adres na cestu principalu s `EMAIL=`) jsou zápisové. Fáze 2 je jen zaznamená do profilu jako fakta pro fázi 3.

### 5.5 Pověření a připojení účtu

- `POST /v1/accounts/dav` (rozsah správa účtů): `{ "profile": "icloud" | "generic", "server_url"?: string, "username": string, "password": string }`. `server_url` je povinné pro `generic` a zakázané pro `icloud`.
- `musubi-syncd` spustí zjišťování synchronně (timeout 20 s). Teprve když projde, zapečetí pověření do `accounts.tokens` a uloží `Endpoint`. Neúspěch vrátí `400` s důvodem (`unauthorized`, `not_dav`, `blocked_address`, `timeout`) a nic neuloží.
- Odpověď a pohled na účet nikdy nevrací heslo. Pohled na účet ukazuje profil, adresu serveru a uživatelské jméno.
- Opětovné připojení po `ReauthRequired`: stejné volání s `account` id přepíše pověření stávajícího účtu.
- **Ochrana odchozích adres** (vstup od uživatele je cesta k SSRF): jen `https` (`http` jen s volbou konfigurace `dav.allow_http`), adresa po překladu DNS nesmí být loopback, privátní, link-local, CGNAT, multicast ani nespecifikovaná (pokud to konfigurace `dav.allow_private` nepovolí; testy s Radicale na loopbacku ji zapnou). Kontroluje se přeložená IP a spojení jde na ni (žádný druhý překlad, ochrana proti DNS rebinding). Přesměrování nejvýš 5, každé znovu přes stejnou kontrolu, nikdy z `https` na `http`. Vzor: `network-origin.ts` v Musubi. Tělo odpovědi má strop (32 MB) a XML parser nerozbaluje externí entity.

## 6. Google Tasks a People

### 6.1 Scope a zjišťování

- `SCOPES` v `oauth.rs` dostane `https://www.googleapis.com/auth/tasks.readonly` a `https://www.googleapis.com/auth/contacts.readonly`, s `include_granted_scopes=true`.
- Google dovoluje uživateli jednotlivé scope na obrazovce souhlasu odškrtnout. Odpověď tokenu nese udělené scope; chybějící druh adaptér vrátí v `Discovery.unauthorized`. `403` s `insufficientPermissions` u Tasks nebo People znamená totéž.
- Kolekce neudělaného druhu se nemaže a nezakládá. Pohled na účet hlásí `missing: ["task"]` a odkaz na opětovný souhlas (`POST /v1/accounts` s `account`). Odpovídá kapitole 3.3 návrhu: chybějící oprávnění je neúplné zjišťování, ne smazání.
- Účty připojené ve fázi 1 mají jen `calendar.readonly`. Tasks a People u nich začnou až po opětovném souhlasu.
- Zjišťování: `calendarList` (beze změny), `tasklists.list` (každý seznam je kolekce `Task`), pro People jedna pevná kolekce `people/me/connections` (`Contact`, jméno „Kontakty").

### 6.2 Tasks

- Čtení: `tasks.list` s `showCompleted=true`, `showHidden=true`, `maxResults=100`, stránkování `pageToken`.
- `Full`: bez `updatedMin`, `showDeleted=false`. Nepřítomnost po úplném čtení znamená smazání.
- Delta: `updatedMin = ts − 60 s`, `showDeleted=true`. `deleted: true` je odebrání. Nový `ts` je největší `updated` z odpovědí (hodiny serveru), bez výsledků se nemění. Překryv 60 s kryje shodné časy; znovu viděný úkol se stejným `etag` engine přeskočí podle validátoru.
- Vypršení kurzoru Tasks nemají. Neplatný `pageToken` (`400`) je `CursorExpired` jako u kalendáře.
- Push nemají. Kolekce patří adaptivnímu dotazování (kapitola 7).
- Levná kontrola: `tasklists.list` vrací `updated` u seznamu. Mění se i při změně úkolu? **(neověřeno, T10)**. Pokud ano, jedno volání za účet rozhodne, které seznamy číst.
- `hidden` (dokončené a skryté úkoly) zůstávají živé položky; pole je jen v surové vrstvě. Otevřená otázka 6.

### 6.3 People

- Čtení: `people.connections.list` s pevným `personFields` (jména, e-maily, telefony, adresy, organizace, narozeniny, poznámky, URL, přezdívky, fotky, členství, metadata, `clientData`), `pageSize=1000`, `requestSyncToken=true`.
- Pevné `personFields` jsou součást kurzoru: jejich změna (nová verze mapování) vynutí úplné čtení, protože Google token s jinými parametry odmítne (kapitola 2.7 návrhu).
- Delta: `syncToken`. `metadata.deleted: true` je odebrání.
- Token platí 7 dní. Odpověď s `ErrorInfo.reason = EXPIRED_SYNC_TOKEN` je `CursorExpired`, ať je stav `400` nebo `410` **(neověřeno, který)**. Při adaptivním dotazování (nejdelší interval 6 h) token vyprší jen u pozastaveného účtu.
- Zvláštní kvóta na první stránku úplného čtení (`429`) je `RateLimited`; engine pozdrží celé úplné čtení, ne jen stránku.
- Validátor je `etag` osoby.
- Skupiny (`contactGroups`) a `otherContacts` ve fázi 2 nejsou (kapitola 11.3). Členství osoby zůstává v surové vrstvě.

### 6.4 Falešný server Google

`musubi-testkit::google` dostane endpointy `tasklists`, `tasks` a `people.connections` se stejnými chybami jako kalendář (`Fault`): `429` s `Retry-After`, `5xx`, neplatný `pageToken`, `EXPIRED_SYNC_TOKEN`, `403 insufficientPermissions`. Tvary odpovědí podle zaznamenaných odpovědí z T10.

## 7. Adaptivní dotazování

### 7.1 Na co se vztahuje

- Kolekce bez otevřeného kanálu pushe: DAV, Tasks, People a kalendáře Googlu, které push odmítly (svátky: `400 pushNotSupported`, live-google.md).
- Kalendář Googlu s otevřeným kanálem má dál pojistné dotazování `poll_every_push` (3600 s). Adaptivní politika se ho netýká.
- Kalendář, jehož kanál zanikl a obnova selhala, je do další úspěšné obnovy adaptivní.

### 7.2 Interval

Engine drží v paměti u kolekce počítadlo `quiet` (čtení za sebou bez změny). Interval je `clamp(min · 2^quiet, min, max)` podle úrovně zájmu tenanta:

| Úroveň | Kdy | `min` | `max` |
| --- | --- | --- | --- |
| aktivní | čtení API nebo připojené SSE za posledních 120 s | 15 s | 60 s |
| běžná | jinak | 2 min | 15 min |
| spící | žádný zájem 7 dní (od restartu se počítá jako běžná) | 1 h | 6 h |

- Změna (stránka s `upserts` nebo `removals`, ne jen se shodnými `stubs`) vrátí `quiet` na 0.
- Chyby mají dál stávající odklad (`backoff`). Pro chybu se interval nezkracuje.
- K intervalu se přičte náhodný rozptyl ±10 %, aby se kolekce jednoho účtu nestahovaly naráz.
- Meze jsou v `Config` enginu. Čísla v tabulce jsou výchozí a K9 je doladí podle benchmarku D2.
- Kvóty hlídá stávající omezovač podle `limits.requests_per_minute`. Interval ho neobchází.

### 7.3 Zrychlení při zájmu

- `Engine::touch(tenant)` volá `musubi-syncd` při čtení `/v1/entities`, `/v1/changes`, `/v1/items` a při připojení `/v1/stream`; u připojeného SSE každých 60 s.
- Přechod tenanta z běžné nebo spící úrovně do aktivní zařadí úlohy `Sync` všech jeho adaptivních kolekcí s `due ≈ now`. Fronta je už zdvojit neumí: `enqueue` při shodě dělá `due = min(due, ?)`, takže stačí zařadit.
- Přechod se počítá nejvýš jednou za 120 s na tenanta. Díky levné kontrole (ctag, kapitola 5.2) stojí zrychlení u nezměněné kolekce jeden požadavek.
- Úroveň zájmu je jen v paměti. Ztráta po restartu znamená nejvýš jeden interval běžné úrovně.

### 7.4 Souběžnost na účet

`limits.max_concurrency_per_account` (kapitola 3.5) omezí počet běžících úloh `Sync` a `Discover` jednoho účtu. Úloha nad limitem se nezačne a vrátí se do fronty s `due` + 1 s. Dnes engine drží jen „jedna úloha na kolekci a jedno zjišťování na účet"; to platí dál.

## 8. Konformační sada

### 8.1 Tvar

- Scénáře jsou jazykově neutrální JSON v `crates/musubi-testkit/conformance/*.json` (kapitola 6.3 návrhu). Popisují logické objekty (událost s názvem a časem, úkol, kontakt, skupina), kroky u poskytovatele (vytvořit, změnit, smazat, vypršet kurzor, chyba) a očekávání nad API `musubi-syncd` (`/v1/entities`, `/v1/changes`, stav kolekce).
- Běžec `crates/musubi-testkit/tests/conformance.rs` spustí `musubi-syncd` jako proces (stejně jako dnešní e2e testy) proti cíli. Cíl implementuje `trait Target`: `connect`, `put`, `delete`, `expire_cursor`, `inject(Fault)`. Krok, který cíl neumí (Radicale neumí vstřikovat chyby), skončí jako „přeskočeno s důvodem" a běh vypíše seznam přeskočených scénářů. Tiché přeskočení není.
- Cíle: falešný Google (rozšířený v K6), falešný DAV v profilu `generic` a `icloud`, Radicale.

Scénáře (každý pro všechny druhy, které cíl podporuje):

| Scénář | Google | DAV fake | Radicale |
| --- | --- | --- | --- |
| úvodní čtení (u událostí okno, pak úplné) | ano | ano | ano |
| stránkování přes víc stránek | ano | ano | ano (adaptér dělí) |
| inkrement: vytvořit, změnit, smazat | ano | ano | ano |
| beze změny: žádný záznam v žurnálu, žádné `multiget` | ano | ano | ano |
| vypršený kurzor → úplné čtení, smazání až po něm | ano | ano | ano (token zneplatní běžec, kapitola 8.3) |
| `429`/`503` s `Retry-After` | ano | ano | přeskočeno |
| `401` → `ReauthRequired`, po opětovném připojení pokračuje | ano | ano | ano (změna hesla v htpasswd) |
| nová a odebraná kolekce při zjišťování | ano | ano | ano |
| neudělaný druh: kolekce se nemaže | ano | — | — |
| pád `kill -9` uprostřed stránky | ano | ano | — (stačí jeden cíl na protokol) |
| neznámá pole přežijí v surové vrstvě bajt po bajtu | ano | ano | ano |
| smíšený kalendář VEVENT + VTODO | — | ano | ano |
| skupina kontaktů (vCard 4 `KIND:group` i Apple) | — | ano | ano |

### 8.2 Falešný DAV server

`musubi-testkit::dav` (axum, stejně jako falešný Google): kalendáře a adresáře v paměti, `PROPFIND`, `REPORT` (`sync-collection`, `calendar-query`, `calendar-multiget`, `addressbook-multiget`), `GET`, well-known, principal a home sety. Vstřikování chyb přes `Fault`:

- `403` s `valid-sync-token`; `507` zkrácení;
- kolekce bez `getctag`, bez `sync-collection`, bez obojího;
- `503` a `429` s `Retry-After`; pomalá odpověď;
- přesměrování na partition host a mimo něj; `404` kolekce;
- `401`;
- slabý a chybějící ETag; ETag změněný mezi výpisem a `multiget`; objekt smazaný mezi nimi;
- režim `icloud`: partition hosty, chybějící `current-user-privilege-set`, vCard 3.0.

Počítadlo požadavků po metodách, aby testy ověřily levnou kontrolu (jeden `PROPFIND`) a že se stahuje jen změněné.

### 8.3 Radicale v CI

Doporučení: **Radicale 3.9.0** (z 25. 10. 2026; 3.9.1 je z dneška, tedy příliš čerstvá). Proč Radicale:

- umí všechno, co plán potřebuje (ověřeno ze zdrojů 3.9.0): `sync-collection` s `403` a `valid-sync-token` na neplatný token, `calendar-multiget`, `addressbook-multiget`, `getctag`, `supported-report-set`, `current-user-principal`, home sety, well-known, VTODO;
- je to jeden proces v Pythonu bez databáze a bez kontejneru: spustí se z testu na volném portu s dočasným adresářem a skončí s ním;
- konfigurace z příkazové řádky: `python3 -m radicale --server-hosts 127.0.0.1:PORT --auth-type htpasswd --auth-htpasswd-filename F --auth-htpasswd-encryption plain --storage-filesystem-folder D`;
- licence GPL-3.0 nevadí: je to externí testovací proces, ne závislost binárky ani `Cargo.lock`.

Hermetičnost:

- `crates/musubi-testkit/radicale/requirements.txt` s přesnou verzí a hashi (`pip install --require-hashes`), do venv v CI. Bez sítě za běhu testu.
- `musubi-testkit::radicale::Radicale::start()` najde venv podle `MUSUBI_RADICALE`. Bez proměnné test Radicale selže s návodem; v jobu `conformance` je proměnná vždy nastavená.
- Vypršení tokenu: Radicale tokeny sám nezneplatní. Běžec pošle token, který server nikdy nevydal; Radicale ho odmítne `403` s `valid-sync-token` (ověřeno ze zdrojů).
- Odezva Radicale mezi verzemi: hash v `requirements.txt` mění jen PR s důvodem.

Další DAV servery (Baïkal, Nextcloud, Stalwart) se ve fázi 2 nepouštějí. Kontejnery by zpomalily CI a falešný DAV pokryje odchylky.

### 8.4 Zlaté fixtures kodeků

- `musubi-ical`: VEVENT (běžné, opakované s výjimkami, celodenní, s vlastní VTIMEZONE, s Windows TZID), VTODO (s rodičem, opakovaný, dokončený), vCard 3.0 a 4.0, Apple skupina. Fixtures z korpusu spiku calcard a z T11 (anonymizované).
- `musubi-google`: Tasks a People z T10 (bez osobních dat).
- Property testy: dekódování nikdy nespadne (libovolné bajty i mutace fixtures); `Decoded::Canonical` je deterministické.

## 9. Kontrolní body

Pořadí je závazné jen tam, kde to říkají závislosti. K0 je brána. T10 a T11 spouští koordinátor, ne pracovník ani CI.

```
K0 ─┬─ K1 úložiště ───────────────┬─ K5 engine ─────────────────┐
    ├─ K2 musubi-ical ────────────┤                             │
    ├─ K3 falešný DAV + Radicale ─┴─ K4 musubi-dav ─┬───────────┼─ K8 server ─ K9 důkaz ─ K10 živá kontrola
    │                                               └─ K7 iCloud┤
    ├─ T10 živě Google Tasks/People ─ K6 Google Tasks/People ───┘
    └─ T11 živě iCloud ──────────────────────────────── (→ K7)
```

### K0. Rozhraní v0 a kostra

- **Cíl:** zafixovat rozhraní mezi crates, aby K1–K6 běžely souběžně bez dohadování.
- **Crates:** `musubi-model` úplně podle kapitoly 3 (druhy a kanonické typy, `RemoteObject.kind`, `MediaType`, `ChangePage.stubs`, schopnosti po kolekcích, `Credential`, `Credentials`, `Endpoint`, `TokenRefresh`, `SyncError::Rediscover`, `Discovery.endpoint` a `unauthorized`, nové operace `TenantStore` a `ControlStore`, dokumentace `PushSource::subscribe`). `musubi-ical` s `kind_of` hotovým a testovaným; zbytek kostra. `musubi-dav` kostra. Stávající crates se přizpůsobí tak, aby se přeložily a testy prošly (Google vyplní `RemoteObject.kind`, engine `Credential::Bearer`).
- **Dále:** tabulka `ALLOWED` a `DEV_FORBIDDEN` (kapitola 2), `CONTRIBUTING.md`, nové závislosti v `deny.toml`, `FakeProvider` umí `stubs`, `kind` a popis s výběrem delty po kolekcích.
- **Přijetí:**
  - CI zelené, včetně `cargo deny check` s `calcard` a `quick-xml`;
  - `dependency_direction` zná oba crates a zastaví `musubi-engine` → `musubi-dav`;
  - starý `collections.meta` bez nových polí se přečte (test serde);
  - `FakeProvider` implementuje nový kontrakt bez `todo!()`.
- **Závisí na:** nic.
- **Souběžně:** nic; je to brána. T10 a T11 mohou běžet už teď.

### K1. Úložiště

- **Cíl:** kapitola 4 a pečetění `Credentials`.
- **Crates:** `musubi-store-sqlite`, `musubi-syncd` (jen `crypto.rs`).
- **Přijetí:**
  - `MAIN_V2` migruje soubor fáze 1 s daty; `list_entities` stránkuje úkoly a kontakty po `id` a nevrací náhrobky ani entity jiného druhu;
  - `stale_items` a `redecode`: přepočet bez změny kanonického tvaru nepřidá záznam do žurnálu, se změnou přidá;
  - `control.db` z fáze 1 (bez `schema`, se starou tabulkou `channels`) se otevře a dostane verzi 2; soubor s verzí 3 vrátí `SchemaTooNew` a zůstane bajtově stejný;
  - blob zapečetěný fází 1 se otevře jako `Credentials::OAuth`; `Basic` projde pečetěním a otevřením a neotevře se pod jiným účtem;
  - `push_resource` se zapíše a přežije výměnu kanálu.
- **Závisí na:** K0.
- **Souběžně:** K2, K3, K4, K6.

### K2. `musubi-ical`

- **Cíl:** kodeky iCalendar a vCard a řetěz zón (kapitoly 3.1 a 3.7).
- **Crates:** `musubi-ical`.
- **Přijetí:**
  - zlaté fixtures z kapitoly 8.4 dávají očekávaný kanonický tvar (`insta`);
  - každý krok řetězu zón má fixture; vlastní VTIMEZONE se dvěma pravidly dává stejné výskyty jako ekvivalentní IANA zóna (přes `musubi-recurrence` v dev-testu) po dobu 20 let; nepřevoditelná VTIMEZONE je `Unsupported`, nikdy UTC;
  - property test: libovolné bajty i mutace fixtures nespadnou;
  - test, že crate nevolá serializaci celého objektu (`to_string` na `ICalendar`/`VCard`): grep v testu nad zdroji;
  - propustnost dekódování změří benchmark D3 (jen měří).
- **Závisí na:** K0.
- **Souběžně:** K1, K3, K4, K6.

### K3. Falešný DAV, Radicale a běžec konformační sady

- **Cíl:** testovací infrastruktura z kapitoly 8 dřív, než ji K4 potřebuje.
- **Crates:** `musubi-testkit`, `.github/workflows/ci.yml`.
- **Přijetí:**
  - falešný DAV odpovídá na všechny metody z kapitoly 8.2 a každý `Fault` má test tvaru odpovědi (jako `google_server.rs`);
  - harness Radicale spustí server, založí kalendář, adresář a uživatele a po testu po sobě nic nenechá; job `conformance` v CI ho instaluje podle hashů;
  - formát scénářů a `trait Target` jsou hotové; běžec pustí jeden scénář (úvodní čtení) proti falešnému Googlu přes `musubi-syncd` fáze 1.
- **Závisí na:** K0.
- **Souběžně:** K1, K2, K4, K6.

### K4. `musubi-dav`: obecný profil

- **Cíl:** kapitoly 5.1–5.3 a 5.5 (bez API) pro profil `generic`.
- **Crates:** `musubi-dav`.
- **Přijetí (proti falešnému DAV a Radicale, bez enginu):**
  - zjišťování přes well-known i přímou adresu najde principal, home sety a kolekce se správným druhem, deltou a přístupem;
  - `Window`, `Full` a delta podle tabulky v kapitole 5.2 pro všechny tři úrovně delty; levná kontrola s nezměněným ctag pošle právě jeden požadavek;
  - každý řádek tabulky v kapitole 5.3 má test;
  - `multiget` po dávkách `max_batch`;
  - ochrana adres: odmítne loopback, privátní adresy, `http` a přesměrování na ně (pokud konfigurace nedovolí); test s DNS jménem, které ukazuje na `127.0.0.1`.
- **Závisí na:** K0; testy potřebují K3 (lze psát souběžně, přijetí až po K3).
- **Souběžně:** K1, K2, K5, K6.

### K5. Engine

- **Cíl:** kapitoly 3.4–3.6, 7 a přepočet z kapitoly 10.
- **Crates:** `musubi-engine`.
- **Přijetí (s `FakeProvider` a `musubi-store-sqlite` jako dev-závislostí):**
  - registr: dva `FakeProvider` s různým `ProviderId` a popisem procházejí stejnými testy; objekt druhu bez kodeku se uloží jen surově;
  - `stubs`: shodný validátor nestáhne nic, rozdílný stáhne přes `fetch` po dávkách; pád mezi výpisem a stažením stránku zopakuje bez duplicit; objekt chybějící ve `fetch` u delty nic nezmění, u úplného čtení zmizí nepřítomností;
  - `Pass::Full` od proudu s kurzorem delty smaže nepřítomné;
  - `Rediscover` zařadí `Discover` a odloží úlohu kolekce; `ReauthRequired` u účtu bez `oauth()` neobnovuje token;
  - `Discovery.unauthorized`: kolekce druhu zůstanou, účet hlásí chybějící druh;
  - adaptivní dotazování s pevnými hodinami: interval roste od `min` do `max`, změna ho vrátí na `min`, `touch` zařadí úlohy s `due ≈ now` a druhý `touch` do 120 s nic nepřidá; kolekce s otevřeným kanálem zůstává na `poll_every_push`;
  - `max_concurrency_per_account = 2` nepustí třetí úlohu účtu;
  - přepočet: změna `MAPPING_VERSION` v testovacím kodeku přepočítá uložené položky jednou (druhý start nic nedělá), bez síťového volání.
- **Závisí na:** K0, K1 (dev-závislost).
- **Souběžně:** K2, K3, K4, K6. Uvnitř jde rozdělit mezi dva pracovníky: příjem (`ingest.rs`, registr, `stubs`, přepočet) a plánování (`jobs.rs`, adaptivní dotazování, souběžnost, `Rediscover`).

### T10. Živá kontrola Google Tasks a People (spouští koordinátor)

- **Cíl:** potvrdit tvary a chování, na kterých stojí K6. Na stávajícím testovacím účtu Google.
- **Kroky:** T10 z [providers.md](../research/phase0/providers.md) (vložení s `id`, smazání, dokončení, vyčištění, pak `updatedMin` + `showDeleted` + `showHidden`); navíc: mění se `tasklists.updated` při změně úkolu; `people.connections.list` s `requestSyncToken`, změna kontaktu, delta, smazaný kontakt; odezva na token s jinými `personFields`; stav a tvar chyby u neplatného `syncToken`; chování při odškrtnutém scope.
- **Výstup:** `docs/research/phase2/live-google-tasks-people.md` a zaznamenané odpovědi jako fixtures (bez osobních dat).
- **Závisí na:** nic (stačí účet a skript); musí skončit před přijetím K6.

### K6. Google Tasks a People

- **Cíl:** kapitola 6.
- **Crates:** `musubi-google`, `musubi-testkit` (falešný server).
- **Přijetí:**
  - zjišťování vrátí kalendáře, seznamy úkolů a kolekci kontaktů; chybějící scope je v `Discovery.unauthorized`;
  - Tasks: úplné čtení, delta s překryvem bez zdvojení, smazání přes `deleted`, neplatný `pageToken` → `CursorExpired`;
  - People: úplné čtení se sync tokenem, delta, smazání, `EXPIRED_SYNC_TOKEN` → `CursorExpired`, `429` na první stránce → `RateLimited`;
  - zlaté fixtures `TasksCodec` a `PeopleCodec` z T10.
- **Závisí na:** K0, T10.
- **Souběžně:** K1–K5, K7.

### T11. Živá kontrola iCloudu (spouští koordinátor)

- **Cíl:** potvrdit předpoklady profilu `icloud` (I1–I6). Na testovacím účtu iCloud, který dodá uživatel.
- **Kroky:** T11 z providers.md (`supported-report-set` u kalendáře a adresáře; `sync-collection` s prázdným tokenem a po jedné změně s vráceným; token starý 30 dní, pokud ho uživatel z dřívějška má, jinak vymyšlený; `PUT` s `If-Match` na kontakt jen pro záznam); navíc: partition hosty z principalu, `current-user-privilege-set`, verze vCard, velikost dávky `multiget`, zda `sync-collection` zkracuje (`507`), chování při `503`.
- **Výstup:** `docs/research/phase2/live-icloud.md`, anonymizované odpovědi jako fixtures pro falešný DAV v režimu `icloud`.
- **Závisí na:** účet od uživatele; musí skončit před přijetím K7.

### K7. Profil iCloud

- **Cíl:** kapitola 5.4 podle výsledků T11.
- **Crates:** `musubi-dav`, `musubi-testkit` (režim `icloud` falešného DAV).
- **Přijetí:**
  - zjišťování přes partition host; adresa mimo povolené hosty nebo jiný origin objektu se odmítne;
  - kolekce jen s VTODO se přeskočí; vCard 3.0 a Apple skupiny projdou;
  - souběžnost 2 a dávka `multiget` podle T11;
  - `401` vede na `ReauthRequired` s textem o hesle pro aplikace.
- **Závisí na:** K4, T11.
- **Souběžně:** K5, K6.

### K8. Server

- **Cíl:** spojit vše v `musubi-syncd`.
- **Crates:** `musubi-syncd`.
- **Hotovo:**
  - registr s instancemi `google`, `dav`, `icloud` a jejich kodeky;
  - `POST /v1/accounts/dav` (kapitola 5.5) s ochranou adres podle konfigurace;
  - nové scope Googlu a pohled na účet s `missing`;
  - `GET /v1/entities?kind=task|contact|contact_group&collection=…&after=…` přes `list_entities`; bez `kind` beze změny (rozsah událostí);
  - `Engine::touch` z API a SSE;
  - pohled na účet DAV bez hesla.
- **Přijetí:**
  - e2e: připojení DAV účtu proti Radicale, první data v API, změna u Radicale v proudu změn do dvou intervalů aktivní úrovně;
  - špatné heslo vrátí `400 unauthorized` a nic neuloží; adresa `127.0.0.1` bez `dav.allow_private` vrátí `blocked_address`;
  - heslo se neobjeví v žádné odpovědi API ani v logu (test čte log na úrovni `trace`);
  - účet Google fáze 1 bez nových scope dál synchronizuje kalendáře a hlásí `missing`.
- **Závisí na:** K1, K2, K4, K5, K6.
- **Souběžně:** K7 (registr `icloud` je jeden řádek; K7 dodá profil).

### K9. Důkaz a zmrazení

- **Cíl:** kritéria z kapitoly 1.
- **Hotovo:**
  - všechny scénáře z kapitoly 8.1 proti všem cílům, job `conformance` v CI;
  - crash test `kill -9` během čtení DAV (proti falešnému DAV);
  - benchmarky D1–D4 (kapitola 11.2);
  - `docs/contract/v0.md` a test `contract_freeze`;
  - meze adaptivního dotazování doladěné podle D2.
- **Přijetí:** CI zelené na všech jobech; seznam přeskočených scénářů odpovídá tabulce v kapitole 8.1.
- **Závisí na:** K7, K8.
- **Souběžně:** nic.

### K10. Živá kontrola fáze 2 (spouští koordinátor)

- **Cíl:** celý `musubi-syncd` proti testovacímu účtu Google (Tasks a People) a iCloudu: první data, změna u poskytovatele v proudu změn, skutečná doba při aktivní a běžné úrovni.
- **Výstup:** doplnění `docs/research/phase2/` o čísla a odchylky; opravy jako nové úkoly.
- **Závisí na:** K9.

## 10. Nedodělky z fáze 1 jako položky plánu

1. **Jednorázový přepočet po změně `MAPPING_VERSION`.** Dnes se uložené položky přepočtou jen tehdy, když je Google pošle znovu (live-google.md). Úložiště: `stale_items` a `redecode` (K1). Engine: úloha `Redecode` na pozadí při startu, řízená meta klíčem `mapping:<provider>:<kind>` (K5). Bez síťového volání, ze surové vrstvy. Pokryje i zvýšení `Event::SCHEMA_VERSION` na 2.
2. **Dokumentace `PushSource::subscribe`.** Které chyby znamenají „kanál nevznikl" (kapitola 3.10). K0.
3. **`resourceId` kalendáře u kolekce.** `collections.push_resource` (K1); odebrání účtu ho použije k zastavení čekajícího kanálu i bez otevřeného sourozence (K5). Test: pád mezi `put_channel` (čekající) a `subscribe`, pak odebrání účtu bez otevřeného kanálu; falešný Google zaznamená `stop` s uloženým `resourceId`.
4. **Strážce `SchemaTooNew` pro `control.db`.** Verze v `meta` a číslované migrace (kapitola 4.3). K1.

## 11. CI, benchmarky, mimo rozsah, otevřené otázky

### 11.1 CI

| Job | Změna |
| --- | --- |
| fmt, clippy, deny | beze změny; `deny` pokryje nové závislosti |
| test | nové testy crates; `dependency_direction` s novými crates; `contract_freeze` od K9 |
| conformance (nový) | `actions/setup-python` připnuté na SHA, Python 3.12, venv z `crates/musubi-testkit/radicale/requirements.txt` s `--require-hashes`, pak `cargo test -p musubi-testkit --test conformance --locked` s `MUSUBI_RADICALE`; na PR a `main` |
| bench-smoke | přidá D1–D4 v malém měřítku |
| nightly | přidá plné D1–D4, delší crash test DAV (1 000 kol) a fuzz kodeků `musubi-ical` (600 s) |

### 11.2 Benchmarky DAV

| Benchmark | Co měří | Cíl |
| --- | --- | --- |
| D1 | první data po připojení DAV účtu (falešný DAV s odezvou 50 ms na požadavek, 2 000 událostí) | okno kolem dneška do 2 s; měří se, nehlídá **(neověřeno, kolik dá iCloud)** |
| D2 | cena nečinnosti: požadavky za hodinu na kolekci beze změn při každé úrovni zájmu | aktivní ≤ 240, běžná ≤ 30, spící ≤ 6; levná kontrola = 1 požadavek |
| D3 | propustnost dekódování `musubi-ical` (MB/s, VEVENT, VTODO, vCard) | měří se; fáze 0 změřila parse calcard 110–210 MB/s |
| D4 | latence změny u DAV do SSE | nejvýš interval úrovně + jedna delta; zveřejní se rozdělení, ne slib |

Latence u DAV, Tasks a People je omezená dotazováním. Cíl „do 500 ms od oznámení" pro ně neplatí, protože oznámení neexistuje. D4 to ukáže čísly (riziko 11 v kapitole 11.3 návrhu).

### 11.3 Mimo rozsah fáze 2

- Jakýkoli zápis k poskytovateli a vrstva záplat (fáze 3).
- WebDAV-Push (W1, po 1.0).
- Úkoly iCloudu, plánování a T12 (fáze 6).
- Google `contactGroups` a `otherContacts`; členství osob ve skupinách v kanonickém tvaru.
- Fotky kontaktů mimo databázi; indexy kontaktů podle e-mailu, telefonu a jména.
- Index výskytů pro VTODO.
- Digest a OAuth pro DAV; DNS SRV.
- Microsoft Graph (fáze 7).
- Další DAV servery v CI (Baïkal, Nextcloud, Stalwart).

### 11.4 Otevřené otázky

1. **Testovací účet iCloud pro T11 a K10.** Dodá uživatel: Apple ID s 2FA a heslem pro aplikace, s kalendářem, adresářem a pár kontakty a skupinou. Bez něj K7 stojí jen na sekundárních zdrojích.
2. **Scope pro zápis už teď?** Fáze 3 bude potřebovat `tasks` a `contacts` (bez `.readonly`) a `calendar.events`. Žádat je teď ušetří druhý souhlas, ale fáze 2 by držela víc oprávnění, než potřebuje (kapitola 7 návrhu: minimální scope). Plán zatím žádá jen `.readonly`.
3. **Stav a tvar chyby `EXPIRED_SYNC_TOKEN`** u People (`400` nebo `410`) **(neověřeno, T10)**.
4. **iCloud a `sync-collection` u CardDAV, zkracování `507`, velikost dávky `multiget`** **(neověřeno, T11)**.
5. **Rozsah úkolů podle termínu.** Má `/v1/entities?kind=task` umět okno podle `due`? Bez indexu výskytů jde jen o filtr nad `list_entities`; opakované úkoly by potřebovaly rozbalení.
6. **Skryté úkoly Googlu** (`hidden: true`, vyčištěné dokončené): zůstávají živé položky. Pokud T10 ukáže, že je Google po čase maže, stačí to; jinak rozhodnout, zda je API má skrývat.
7. **Meze adaptivního dotazování.** Výchozí čísla v kapitole 7.2 jsou odhad; kvóta Tasks API (výchozí denní limit projektu) **(neověřeno)** může vynutit delší `min` aktivní úrovně.
