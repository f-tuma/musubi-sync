# Fáze 1: Důkaz jádra — implementační plán

Stav: návrh plánu, 11. 11. 2026. Vychází z [návrhu architektury, revize 7](../architecture/proposal.md) a z výsledků [fáze 0](../research/phase0/). Co není ověřené, je označeno **(neověřeno)**.

## 1. Cíl a kritéria hotovosti

Fáze 1 dokazuje, že jádro unese čtecí cestu pro Google Calendar rychle a bez ztráty dat. Zápisy k poskytovatelům nejsou. Entity a umístění ale jsou v datech i v API od začátku; každá entita má zatím právě jedno umístění.

Kritéria jsou převzatá z kapitoly 11.1 návrhu. Každé má v plánu test, který ho dokazuje:

| Kritérium | Kde se dokazuje |
| --- | --- |
| první data do 1 s od připojení účtu (okno kolem dneška) | benchmark B1, K7 |
| změna u Googlu v proudu změn do 500 ms od oznámení (proti falešnému serveru) | benchmark B2, K7 |
| dotaz na rozsah p99 pod 10 ms u běžného tenanta | benchmark B3, K7 |
| `kill -9` v libovolném okamžiku bez ztráty a duplicit | crash testy, K7 |
| obnova ze zálohy projde v CI | test zálohy a obnovy, K1 a K7 |
| WAL omezený checkpointerem | test WAL v K1, benchmark B6 |
| živá kontrola proti testovacímu účtu Google včetně T8 | K8, spouští uživatel |

## 2. Workspace

Jeden Cargo workspace, `edition = "2024"`, licence MIT, toolchain stable připnutý v `rust-toolchain.toml`. Každý crate má jednu odpovědnost. Crate, který fáze 1 nepotřebuje, nevzniká.

| Crate | Odpovědnost | Závisí na (interně) |
| --- | --- | --- |
| `musubi-model` | typy a rozhraní bez I/O: identifikátory, druh `Event`, kanonická událost, surový objekt s validátorem, kurzor, stránka změn, žurnál, schopnosti, chyby; traity `Provider`, `PushSource`, `Codec`, `TenantStore`, `ControlStore` | — |
| `musubi-recurrence` | převzaté jádro `rrule` 0.14 a hrana nad `jiff`: `expand(series, window, limit)`; politika gap/fold, povinné okno, strop výskytů | — |
| `musubi-store-sqlite` | implementace `TenantStore` a `ControlStore` nad `rusqlite`: soubory, schéma a migrace, checkpointer, LRU otevřených tenantů, záloha a obnova | `musubi-model` |
| `musubi-google` | `Provider` a `PushSource` pro Google Calendar (transport, stránkování, watch kanály, obnova tokenu) a `Codec<Event>` pro JSON událostí | `musubi-model` |
| `musubi-engine` | aktor účtu, plánovač nad trvalou frontou úloh, příjem stránek, sloučení do entity, index výskytů, žurnál, sběrnice změn, pojistky mazání | `musubi-model`, `musubi-recurrence` |
| `musubi-syncd` | binárka: konfigurace, HTTP API (`axum`), SSE, příjem webhooků, průkazy volajících, OAuth tok, šifrování tokenů, příkazy `serve`, `backup`, `restore` | všechny výše |
| `musubi-testkit` | falešný server Google, skriptovaný `FakeProvider` (`Provider` a `PushSource` v paměti), harness pro `kill -9`, fixtures, generátor zátěže pro benchmarky; `publish = false`, jen dev-závislost a vlastní binárky | `musubi-model` |

Směr závislostí:

```
                       musubi-syncd (bin)
        ┌──────────────┬───────┴────────┬────────────────┐
        ▼              ▼                ▼                ▼
 musubi-engine   musubi-store-sqlite  musubi-google   (musubi-testkit: jen dev)
   │      │            │                │                │
   │      ▼            │                │                │
   │  musubi-recurrence│                │                │
   ▼                   ▼                ▼                ▼
 ──────────────────────── musubi-model ───────────────────────
```

Pravidla:

- `musubi-model` nezávisí na žádném crate projektu a nedělá I/O.
- Všechny crates mají `publish = false`: fáze 1 nic na crates.io nevydává. Crate, který se vydávat bude, změní příznak ve vlastním PR.
- Engine nezná SQLite ani Google. Pracuje proti traitům. V testech dostane `musubi-store-sqlite` nad dočasným adresářem a `FakeProvider` z `musubi-testkit`: skriptované stránky, chyby (`CursorExpired`, `ReauthRequired`, `RateLimited`) a oznámení, s počítadlem volání. Obojí jsou dev-závislosti. `musubi-google` engine nepoužívá ani v testech; cestu engine + adaptér + falešný server Google testuje `musubi-syncd` v K6 a K7. Proto K5 nezávisí na K4.
- Adaptér nezná engine ani úložiště.
- `musubi-store-sqlite` a `musubi-google` smí přímo použít jen `musubi-syncd` (a testy). Dev-závislosti pro testy jsou povolené. CI to hlídá testem `crates/musubi-testkit/tests/dependency_direction.rs`, který běží v `cargo test --workspace`: přečte `cargo metadata` a porovná interní závislosti každého crate s tabulkou povolených (tabulka výše); dev-závislosti povoluje kromě `musubi-engine` → `musubi-google`. Nový crate, který v tabulce chybí, test také zastaví. Test kontroluje jen přímé hrany. Pro běžné závislosti to stačí, protože tabulka je uzavřená (co smí crate použít, používá jen to, co smí i on); u dev-závislostí by cesta přes jiný crate prošla, proto se kontrolují při revizi. Pravidla a postup změny jsou v `CONTRIBUTING.md`.
- Proč ne `cargo-deny` (ověřeno v K0 s cargo-deny 0.20): `[[bans.deny]]` s `wrappers` nerozliší dev-závislosti, takže zakáže i povolené `musubi-engine` → `musubi-store-sqlite` v `[dev-dependencies]`. Volba `[graph] exclude-dev = true` je globální: pustí i zakázané `musubi-engine` → `musubi-google` v testech a vyřadí dev-závislosti i z kontroly licencí a advisories.

Co záměrně nevzniká:

- `musubi-ical` (calcard a vrstva záplat): Google posílá JSON, iCalendar přijde s CalDAV ve fázi 2.
- Samostatný crate pro API, ověřování nebo šifrování: jsou to moduly `musubi-syncd`, dokud je nepotřebuje druhá binárka.
- Backend PostgreSQL: později, za stejným traitem.

Hlavní externí závislosti podle kapitoly 8 návrhu: `tokio`, `reqwest` (rustls), `axum`, `tower-http`, `serde`, `serde_json`, `jiff` (jen přibalená tzdb: `default-features = false`, `tzdb-bundle-always`, bez `tzdb-zoneinfo` a `tz-system`, aby rozbalení opakování dalo na každém stroji stejný výsledek), `rusqlite` (bundled), `oauth2` 5.0, `jsonwebtoken` 11 (backend `rust_crypto`), `aes-gcm`, `secrecy`, `zeroize`, `tracing`, `governor`, v testech `proptest` a `insta`. Nic dalšího bez důvodu v PR.

## 3. Úložiště

### 3.1 Soubory

```
<data>/control.db                         řídicí rovina (jedna na instanci)
<data>/tenants/<tenant_id>/main.db        hlavní soubor tenanta (+ -wal, -shm)
<data>/tenants/<tenant_id>/outbox.db      soubor outboxu; vzniká ve fázi 3
<data>/backups/<čas>/…                    snímky
```

- `tenant_id` je náhodný 128bitový identifikátor; textově 26 znaků malé base32hex (RFC 4648, bez paddingu), stejně jako všechny naše identifikátory (`musubi-model::id`). Base32hex proto, že je to zároveň platné klientské ID události u Googlu a řazení textu odpovídá řazení čísel. Do cesty nikdy nejde vstup od volajícího.
- `control.db`: tenanti, klíče API (jen hash), účty a šifrované tokeny, trvalá fronta úloh, watch kanály (id kanálu → tenant, účet, kolekce, tajný token kanálu). Zápisy `synchronous=FULL`.
- `main.db`: tabulky podle spiku ([storage.md](../research/phase0/storage.md), 3.1): `raw`, `items`, `entities`, `placements`, `occurrences`, `changes`, k tomu `collections` (kurzor, pokrytí, stav) a `meta` (verze schématu; vodoznak `projected` je rezervovaný pro fázi 3). Pět indexů podle kapitoly 5.6 návrhu. Žádné `WITHOUT ROWID` u tabulek s těly. Doplněno v K1: výskyty delší než 7 dní mají vlastní částečný index `occurrences_long` podle konce, takže jedna dlouhá událost nerozšíří časový sken každého dotazu `range`.
- `outbox.db`: mutace, doručení, odchozí zprávy. Ve fázi 1 nevzniká, protože fáze 1 nemá mutace. Jméno, umístění v adresáři tenanta a pořadí v záloze jsou ale dané už teď a test zálohy s ním počítá.

### 3.2 Podmínky z fáze 0 a kdy se plní

Podmínky z kapitoly 4.3 návrhu jsou požadavky. Fáze 1 plní všechny, které se týkají čtecí cesty. Zbytek se plní ve fázi 3, před první mutací, a rozhraní je na to připravené.

| Podmínka | Fáze 1 | Fáze 3 |
| --- | --- | --- |
| build s `SQLITE_DEFAULT_MEMSTATUS=0` (`LIBSQLITE3_FLAGS` v `.cargo/config.toml`) | ano | — |
| `busy_timeout` jako první pragma, zápisy `BEGIN IMMEDIATE`, čtecí a zapisovací spojení zvlášť | ano | — |
| jeden zapisovatel na soubor | ano (zapisovatel tenanta pro `main.db`) | jediný zapisovatel outboxu pro `outbox.db`: přijetí, doručení z promítnutí, výsledky doručení |
| vlastní checkpointer: `PASSIVE` na pozadí, `TRUNCATE` v zapisovači po stránce nad prahem 64 MB, čekání na čtenáře nejvýš 10 ms, back-off 1 s nebo +32 MB; `journal_size_limit`, vysoký `wal_autocheckpoint` (jen soubory tenantů; `control.db` nemá checkpointer a drží výchozí hodnotu). Doplněno v K1: čtení i `PASSIVE` na pozadí procházejí branou v procesu, kterou `TRUNCATE` na ty 10 ms uzavře a vyprázdní (po vyprázdnění pustí čtení dál, jen `PASSIVE` čeká do konce resetu), a zapisovatel před `TRUNCATE` sám pustí `PASSIVE`. Bez toho se překrývající čtenáři střídají na starých značkách čtení, WAL se nikdy nevyprázdní a import rostl přes 256 MB; `TRUNCATE` navíc nemá na zámku checkpointu busy handler, takže souběžný `PASSIVE` ho shodí hned | ano | přijetí reset nikdy nevolá |
| omezená délka čtecích transakcí; dlouhá čtení přes backup API nebo `VACUUM INTO` | ano: rozsah je stránkovaný, export a záloha jdou přes backup API | — |
| id se nikdy znovu nepoužijí | `changes.seq` je `AUTOINCREMENT` | id mutací `AUTOINCREMENT` |
| stránka příjmu jedna transakce `synchronous=NORMAL` | ano | — |
| metriky: největší WAL, délka `TRUNCATE`, počet neúspěšných resetů na tenanta | ano | i pro `outbox.db` |
| outbox v samostatném souboru jako zdroj pravdy, promítnutí s vodoznakem, žádná atomicita přes soubory, kontrola výchozí revize včetně nepromítnutých mutací; doručení z promítnutí commitnutá v `outbox.db` dřív než vodoznak v `main.db`; replay jako první po otevření tenanta a doručení, která už v outboxu jsou, mají přednost před novým výpočtem (návrh, kapitola 3.5) | jen rozhraní a rozvržení souborů | ano |
| záloha: nejdřív `main.db`, potom `outbox.db` | pořadí v kódu zálohy | ano |

### 3.3 Rozhraní úložiště

Úzké rozhraní, proti kterému se píše engine. Logika slučování ani plánování v něm není: jediný zapisovatel tenanta si stav přečte, rozhodne a zapíše výsledek jednou operací. Ve fázi 1 má `TenantStore` 19 operací (5–23) a továrna `Storage` 4, ve fázi 3 přibude 6.

Náčrt (signatury, ne implementace):

```rust
/// Továrna a životní cyklus. Implementuje `musubi-store-sqlite`, později PostgreSQL.
pub trait Storage: Send + Sync {
    type Tenant: TenantStore + 'static;
    fn open(&self, t: TenantId) -> Result<Self::Tenant, StoreError>;         // 1  líná migrace, LRU
    fn create_tenant(&self, t: TenantId) -> Result<(), StoreError>;          // 2
    fn delete_tenant(&self, t: TenantId) -> Result<(), StoreError>;          // 3  smaže všechny soubory
    fn backup(&self, t: TenantId, dest: &Path) -> Result<BackupInfo, StoreError>; // 4  main, potom outbox
}

pub trait TenantStore: Send {
    // Čtení: souběžná, krátké čtecí transakce, výsledky stránkované.
    fn head(&self) -> Result<Seq, StoreError>;                                       // 5
    fn collections(&self) -> Result<Vec<CollectionState>, StoreError>;               // 6
    fn entity(&self, id: EntityId, inc: Include) -> Result<Option<EntityView>, StoreError>; // 7
    fn item(&self, id: ItemId, inc: Include) -> Result<Option<ItemView>, StoreError>;       // 8  i surový obsah
    fn items_by_remote(&self, col: CollectionId, keys: &[RemoteKey])
        -> Result<Vec<ItemState>, StoreError>;                                       // 9  validátor, hash, entita
    fn range(&self, q: &RangeQuery) -> Result<RangePage, StoreError>;                // 10 stránka, varianta jen s id
    fn changes(&self, since: Seq, limit: u32) -> Result<ChangesPage, StoreError>;    // 11 `CursorTooOld` po prořezu
    fn unseen_count(&self, col: CollectionId, read: ReadId) -> Result<u64, StoreError>; // 12 pro pojistku mazání
    fn unseen(&self, col: CollectionId, read: ReadId, after: Option<&RemoteKey>, limit: u32)
        -> Result<Vec<RemoteKey>, StoreError>;                                       // 13 klíče pro pojistku, keyset
    fn series_to_extend(&self, until: Timestamp, limit: u32)
        -> Result<Vec<SeriesRef>, StoreError>;                                       // 14 posun horizontu indexu
    fn stats(&self) -> Result<StoreStats, StoreError>;                               // 15 WAL, TRUNCATE, velikosti

    // Zápis: volá jen zapisovatel tenanta, každá operace je jedna transakce.
    fn apply_discovery(&mut self, d: &DiscoveryWrite) -> Result<(), StoreError>;     // 16
    fn apply_page(&mut self, p: &PageWrite) -> Result<Seq, StoreError>;              // 17 položky, raw, entity,
                                                                                     //    umístění, výskyty,
                                                                                     //    žurnál, kurzor
    fn finish_full_read(&mut self, col: CollectionId, read: ReadId)
        -> Result<Seq, StoreError>;                                                  // 18 náhrobky nepřítomných
    fn purge_collection(&mut self, col: CollectionId) -> Result<Seq, StoreError>;   // 19 zaniklá kolekce, odpojení
    fn set_collection_state(&mut self, col: CollectionId, s: &CollectionStatus)
        -> Result<(), StoreError>;                                                   // 20 stav, chyba, pokrytí
    fn extend_occurrences(&mut self, w: &OccurrenceWrite) -> Result<(), StoreError>; // 21
    fn prune(&mut self, older_than: Timestamp) -> Result<PruneInfo, StoreError>;     // 22 náhrobky; osiřelá raw bez ohledu na stáří
    fn checkpoint(&mut self, mode: CheckpointMode) -> Result<CheckpointInfo, StoreError>; // 23

    // Fáze 3 (soubor outboxu), zde jen pro tvar:
    // fn accept(&self, m: &MutationWrite) -> Result<Accepted, AcceptError>;        // outbox.db, FULL
    // fn unprojected(&self, after: MutationId, limit: u32) -> …;
    // fn insert_deliveries(&self, d: &[DeliveryWrite]) -> …;                       // outbox.db, před `project`;
    //     insert-or-ignore podle (mutace, umístění): nikdy nezmění stav existujícího doručení
    // fn project(&mut self, batch: &ProjectionWrite) -> …;                         // main.db + vodoznak
    // fn due_deliveries(&self, limit: u32) -> …;
    // fn record_outcome(&self, d: DeliveryId, o: &DeliveryOutcome) -> …;           // outbox.db, FULL; i `Sent` před
    //     odesláním: „odesláno" bez výsledku je po restartu nejisté (pozorování, ne nové odeslání)
}
```

Závazné signatury, typy a jejich kontrakty jsou od K0 v `crates/musubi-model/src/store.rs` a `control.rs`; náčrt výše je jejich zkratka. Upřesnění z K0 proti původnímu náčrtu:

- `ReadId` přiděluje engine, musí být větší než `CollectionState::last_read` (o jedna víc je vždy platné). `CollectionState::active_read` drží rozběhnuté úplné čtení, aby po pádu pokračovalo se stejným `ReadId`. Stránka s `read: Some(r)` čtení `r` zahájí; je-li aktivní starší čtení, nahradí ho a jeho značky „viděno" zahodí (nové úplné čtení po `CursorExpired`). Starší `r` nebo `r` nepřevyšující `last_read` je `StoreError::StaleRead` a nic se nezapíše.
- Dokud je čtení aktivní, každá stránka kolekce označí klíče svých `upserts`, `unchanged` i `removals` jako viděné, ať má `read` jakýkoli. Delta stránky zapsané mezi poslední stránkou čtení a `finish_full_read` (pád, dokončení pozdržené pojistkou mazání) se tak počítají a jejich položky se nesmažou.
- `PageWrite::last` označí poslední stránku: na stránce aktivního čtení nastaví `CollectionState::read_complete`. `finish_full_read` projde jen pro aktivní čtení s `read_complete`, jinak vrátí `StaleRead`; po restartu tak engine z `read_complete` pozná, zda čtení ještě běží, nebo zbývá jen dokončit.
- `PageWrite` nese kromě `upserts` a `removals` i seznam `unchanged`: klíče, které přišly se stejným validátorem nebo hashem surového obsahu. Nic se pro ně nezapíše, jen se označí jako viděné (jinak by je `finish_full_read` smazal).
- Pojistka hromadného mazání: `unseen_count` řekne, kolik položek by `finish_full_read` smazal, `unseen` vrátí jejich klíče po stránkách (keyset podle klíče). Engine je ověří přes `Provider::fetch`; objekty, které stále existují, znovu označí jako viděné přes `apply_page` s `read: Some(r)`, `cursor: None` a klíči v `unchanged` (nebo v `upserts`, pokud se změnily). Zbytek smaže `finish_full_read`.
- `finish_full_read` označí nepřítomnou položku za smazanou i s entitou. To platí jen ve fázi 1, kde má entita jedno umístění. Ve fázích 3 a 4 s více umístěními nepřítomnost položky jen odpojí dané umístění a entita zůstane (návrh 12.7); operace se tehdy změní.
- `purge_collection` odstraní kolekci, kterou engine přestal synchronizovat (zmizela z úplného objevení a je to potvrzené, nebo se odpojuje její účet): náhrobky všech položek s entitami a výskyty, záznamy v žurnálu, smazání kolekce. Její `CollectionId` se znovu nepoužije. Tím má `CollectionState::gone` operaci, která ho ukončí.
- `CollectionStatus` nese i `synced_at` (konec posledního úspěšného čtení jakéhokoli průchodu, návrh 3.1) a `full_read_at` (konec posledního dokončeného úplného čtení) pro stav v API.
- Hash surového obsahu je SHA-256 (`Raw::hash`, typ `RawHash`).
- Každá položka nese `schema_version` kanonického tvaru (`Kind::SCHEMA_VERSION`) a `mapping_version` kodeku; `ItemState` vrací obojí, takže zapisovatel pozná položku dekódovanou starším tvarem. Operace, která takové položky vyjmenuje pro nové dekódování ze surové vrstvy, ve fázi 1 není: `Event::SCHEMA_VERSION` je 1 a nic se nepřevádí. Přidá ji první změna kanonického tvaru (nejpozději fáze 2).
- Výjimky série u Googlu jsou ve fázi 1 samostatné položky a entity. Spojuje je `Correlation { uid, recurrence_id }`: výskyt hlavní série nahrazený výjimkou se z `range` vynechá. `recurrence_id` je `Timestamp` (stejně v `OccurrenceRow`, `RangeCursor` a `OccurrenceView`): u výskytu v zóně skutečný okamžik; u plovoucího a celodenního výskytu místní datum a čas zapsaný, jako by byl v UTC. Obě strany korelace tak porovnávají stejná čísla bez zóny.
- Pravidla opakování jsou v kanonickém tvaru textem RRULE; rozbalení dělá engine přes `musubi-recurrence` a výsledek zapíše jako `Occurrences` (řádky plus `SeriesSpan` s `indexed_until`).
- `RangePage::unindexed` (jen na první stránce) vrací série, jejichž index končí dřív než okno dotazu; volající je pro tento dotaz rozbalí sám přes `musubi-recurrence`. Index posouvá `series_to_extend` a `extend_occurrences`.

Obnova (`restore`) je offline příkaz `musubi-syncd restore`, ne operace traitu: nesmí běžet nad otevřeným tenantem.

`ControlStore` je samostatný trait nad `control.db` (25 operací, synchronní, `&self`):

- tenanti: vložit, smazat;
- klíče API: vložit hash, najít podle hashe, zneplatnit;
- účty: vložit nebo nahradit, najít podle id, najít podle (tenant, poskytovatel, `subject`), vypsat účty tenanta, změnit stav, smazat. Trojice (tenant, poskytovatel, `subject`) je unikátní: stejná trojice pod jiným id je `AlreadyExists`, takže nové připojení téhož účtu Google zachová jeho id. Odpojení (`AccountState::Removing`): engine odhlásí kanály účtu, každou jeho kolekci odstraní přes `purge_collection` a nakonec zavolá `delete_account`, které smaže účet s tokeny, úlohami a kanály; po restartu kroky pokračují. Úloha nese účet v indexovaném sloupci `account` (může být prázdný), odvozeném z druhu úlohy; `ExtendIndex` a `Maintain` patří tenantovi a smaže je jen `delete_tenant`;
- šifrované tokeny: zapsat, přečíst (zapečetěný blob);
- rozběhnuté přihlášení OAuth: `put_oauth_state` uloží záznam pod SHA-256 parametru `state` (tenant, poskytovatel, případně účet k novému připojení, ověřovač PKCE, platnost) a smaže prošlé; `take_oauth_state` ho v jedné transakci vrátí a smaže, takže `state` platí jednou. Stav tedy není podepsaný a bezstavový: jednorázovost z K6 vyžaduje záznam;
- fronta úloh: zařadit se sloučením duplicit podle klíče `dedup` v rámci tenanta a jen mezi nepronajatými úlohami (běžící úloha se nesloučí, aby nepřišla o nový podnět), kdy platí dřívější `due`; pronajmout splatné na dobu `lease_for`; prodloužit pronájem (`extend_lease`, dlouhé úplné čtení); dokončit; přeplánovat s volbou `count_attempt` (chyba s odkladem počítá pokus, omezení rychlosti ani plánované pokračování ne). Prodloužení, dokončení i přeplánování jen s platným `LeaseId`, jinak `LeaseLost`. Úloha s pokračováním (další část úplného čtení) zařadí pokračování před `complete`: pád mezi tím nechá starou úlohu znovu běžet (stránky jsou idempotentní), nikdy nezůstane žádná;
- watch kanály: zapsat, najít podle id kanálu, vypsat kanály tenanta nebo jednoho jeho účtu (před `delete_account` a `delete_tenant`), smazat, vypsat ty, kterým brzy vyprší platnost. `ChannelRecord` je `{ tenant, account, collection: CollectionId, sub: Subscription }`, klíčem je `sub.channel` (samostatné `id` by ho jen opakovalo). Token kanálu je v `Subscription` jako `SecretString`, ne prostý `String`. Webhook tak oznámení ověří přes `PushSource::verify` bez otevření úložiště tenanta. V `control.db` je token kanálu uložený nešifrovaně: není to přihlašovací údaj, podvržené oznámení umí jen spustit synchronizaci kolekce kanálu, která čte poskytovatele tokeny účtu, a `verify` potřebuje prostou hodnotu u každého oznámení bez cesty přes trezor. Rozhodnutí znovu posoudí K6 (bezpečnostní revize).

`TokenVault` je pohled enginu na tokeny účtu: `load(account) -> Option<AccountTokens>` a `store(account, &AccountTokens)`, prosté tokeny (`refresh`, `access` jako `SecretString`, `access_until`). Implementuje ho `musubi-syncd` nad `write_tokens`/`read_tokens` se šifrováním AES-GCM vázaným na tenanta a účet; model ani engine šifrování neznají. Když `Provider::refresh` vrátí nový (rotovaný) refresh token, engine ho nejdřív trvale uloží přes `store` a nový access token použije až po `Ok` (návrh 7): starý refresh token už neplatí, ztráta nového by vynutila nové přihlášení.

Invarianty, které hlídají kontraktní testy traitu (běží proti každé implementaci):

- stránka je celá i s kurzorem, nebo není vůbec;
- opakovaná stránka se stejnými validátory nezmění položky, entity ani žurnál a neposune `seq`; jen zaznamená, že ji úplné čtení vidělo;
- `seq` roste a nikdy se znovu nepoužije, ani po prořezu a pádu;
- dokud je úplné čtení `r` aktivní, každá stránka kolekce (i delta stránka po jeho poslední stránce) označí své klíče jako viděné čtením `r`; `finish_full_read(col, r)` projde jen po stránce `r` s `last` a označí jako smazané jen živé položky, které `r` nevidělo;
- jedna položka na (kolekce, vzdálený klíč), jedna entita na položku (ve fázi 1);
- `range` vrací výskyty seřazené, stránkované a bez duplicit přes hranici stránky.

## 4. Kontrakt adaptéru ve fázi 1

Kapitola 6 návrhu, zúžená na čtení. Rozdělení na čtecí a zapisovací trait dovolí přidat zápis ve fázi 3 bez změny čtecí části a bez metod, které by ve fázi 1 jen panikařily.

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;            // včetně úplných schopností (6.2)
    async fn discover(&self, cx: &AccountCx) -> Result<Discovery, SyncError>;
    fn changes<'a>(&'a self, cx: &'a AccountCx, col: &'a CollectionRef,
                   cursor: Option<Cursor>, hint: FetchHint)
        -> BoxStream<'a, Result<ChangePage, SyncError>>;
    async fn fetch(&self, cx: &AccountCx, refs: &[RemoteRef])
        -> Result<Vec<RemoteObject>, SyncError>;
    async fn refresh(&self, refresh_token: &SecretString) -> Result<AccessToken, SyncError>;
    fn push(&self) -> Option<&dyn PushSource> { None }
}

#[async_trait]
pub trait PushSource: Send + Sync {
    async fn subscribe(&self, cx: &AccountCx, col: &CollectionRef, callback: &http::Uri)
        -> Result<Subscription, SyncError>;              // nese skutečnou `expiration`
    async fn unsubscribe(&self, cx: &AccountCx, s: &Subscription) -> Result<(), SyncError>;
    fn channel_of(&self, headers: &HeaderMap) -> Option<ChannelId>;
    fn verify(&self, headers: &HeaderMap, known: &Subscription) -> Result<PushHint, PushRejected>;
}

pub trait Codec<K: Kind> {
    const MAPPING_VERSION: u32;
    fn decode(raw: &Raw) -> Decoded<K::Canonical>;      // čistá funkce, bez I/O
}

// Fáze 3: `ProviderWrite: Provider` s `apply` a `locate`; `Codec` dostane `encode`.
// Fáze 4: `project` a `lift`.
```

Závazné signatury jsou od K0 v `crates/musubi-model/src/provider.rs`. Upřesnění z K0 proti původnímu náčrtu:

- `refresh` je nová metoda: výměna refresh tokenu za přístupový u token endpointu poskytovatele. Volá ji engine, nejvýš jednou na účet najednou; adaptér sám tokeny neobnovuje.
- `channel_of` je nová metoda: webhook potřebuje z hlaviček zjistit id kanálu, aby našel `Subscription` v `ControlStore`, a teprve potom volá `verify`.
- Callback je `http::Uri` (crate `http`, který používá `reqwest` i `axum`), ne `Url`.
- `discover` vrací `Ok` jen s úplným seznamem; při selhání uprostřed vrátí `SyncError::PartialDiscovery { found, .. }` a engine nic nesmaže.
- `ChangePage` má tvar `{ pass, upserts, removals, cursor: Option<Cursor>, last }`. `pass` je `Window`, `Full` nebo `Delta`. Kurzor je na každé stránce úplného čtení i delty (poslední nese kurzor pro příští delta), u okna je `None`: okno se opakuje, nepokračuje. `FetchHint { start: Start::Full | Start::Window(okno), page_size }` určuje, jaké čtení začít, když kurzor chybí.
- `Decoded` je `Canonical(T)` nebo `Unsupported { reason }`: objekt bez kanonického tvaru se uloží jen surově, není to chyba čtení. `Codec` nemá `self`, takže není object-safe; engine ho dostává jako typový parametr.

Co už ve fázi 1 drží tvar pro zápis:

- `RemoteObject` nese surový obsah beze změny a validátor (u Googlu `etag`). To je základ podmíněného zápisu.
- `Decoded` zachová neznámá pole v surové vrstvě; kanonický tvar je jen pohled.
- `ProviderDescriptor` hlásí všechny schopnosti z kapitoly 6.2 včetně `conditional_update`, `conditional_delete` a vytvoření s klientským ID (Google: base32hex, 5–1024 znaků). API je vystaví hned.
- Identifikátory umístění jsou naše, vzdálený klíč je zvlášť. Předem přidělené vzdálené ID ve fázi 3 tak nic nemění.
- `AccountCx` dává adaptéru id účtu, přístupový token, omezovač a hodiny. HTTP klienta v něm není: je to pole adaptéru (sdílený klient na poskytovatele), aby `musubi-model` nezávisel na `reqwest`. Obnovu tokenu dělá engine přes `Provider::refresh` jednou na účet najednou; `invalid_grant` je `SyncError::ReauthRequired`.

Chyby jsou typované podle toho, co má engine udělat: `ReauthRequired`, `Unauthorized` (odmítnutý přístupový token: engine zavolá `refresh` a zkusí to jednou znovu), `RateLimited { retry_after }`, `CursorExpired`, `Transient`, `Permanent`, `PartialDiscovery`.

Google ve fázi 1 (fakta z [providers.md](../research/phase0/providers.md)):

- stránka nejvýš 2500; prázdná stránka s `nextPageToken` neznamená konec;
- `410 Gone` u sync tokenu je `CursorExpired`, následuje úplné čtení;
- první dojem: dotaz s `timeMin`/`timeMax` kolem dneška bez tokenu, potom úplné čtení, které vrátí `nextSyncToken`;
- série: hlavní událost s `recurrence` a výjimky jako samostatné události s `recurringEventId`; rozbalení dělá náš `musubi-recurrence`, ne Google;
- watch kanál: obnova podle vrácené `expiration` (výchozí 604 800 s), ne podle předpokladu; oznámení jen označí kolekci jako špinavou;
- `429` a `403 rateLimitExceeded` s `Retry-After` jdou do omezovače.

## 5. Kontrolní body

Pořadí je závazné jen tam, kde to říkají závislosti. K0 je brána: dokud neprojde, nic dalšího nezačne.

```
K0 ─┬─ K1 úložiště ──┬─ K5 engine ──────────────────┐
    ├─ K2 rekurence ─┘                              │
    │  K1 ─ K6 server (spojení s K4 a K5) ──────────┼─ K7 důkaz ─ K8 živá kontrola
    ├─ K3 falešný Google ─┐                         │
    └─ K4 adaptér Google ─┴─ testy transportu ──────┘
```

### K0. Kostra a rozhraní

- **Cíl:** zafixovat rozhraní, aby K1–K4 mohly běžet souběžně bez dohadování.
- **Crates:** všech sedm. `musubi-model` úplně (typy a traity z kapitol 3 a 4 tohoto plánu); ostatní jen prázdná kostra, která se přeloží.
- **Dále:** `rust-toolchain.toml`, `.cargo/config.toml` s `LIBSQLITE3_FLAGS`, `deny.toml`, workflow CI (kapitola 7), `CONTRIBUTING.md` s pravidly směru závislostí.
- **Přijetí:**
  - CI je zelené na prázdném workspace: fmt, clippy `-D warnings`, test, `cargo deny check`;
  - porušení směru závislostí (třeba `musubi-engine` → `musubi-store-sqlite` v `[dependencies]`) CI zastaví (job test, kapitola 2);
  - `cargo deny check` projde jen s dokumentovanými výjimkami (RUSTSEC-2023-0071 s důvodem a datem revize; v K0 ji cargo-deny hlásí jako nenalezenou, protože `jsonwebtoken` přijde až v K6);
  - kostra `FakeProvider` v `musubi-testkit` implementuje `Provider` a `PushSource` v paměti a projde typový test stránkování: rozhraní jde implementovat bez `unsafe` a bez `todo!()`. Skriptování dodělá K5.
- **Závisí na:** nic.
- **Souběžně:** nic; je to brána.

### K1. Úložiště SQLite

- **Cíl:** `TenantStore`, `ControlStore` a `Storage` podle kapitoly 3, se všemi podmínkami pro fázi 1.
- **Crates:** `musubi-store-sqlite`.
- **Přijetí:**
  - kontraktní testy invariantů z kapitoly 3.3 procházejí;
  - `kill -9` podprocesu uprostřed smyčky `apply_page` (200 opakování v CI): po znovuotevření je kurzor kolekce shodný s poslední celou stránkou a žádná položka není zdvojená;
  - syntetický import 50 000 událostí se souběžnými čtenáři: největší WAL nepřekročí 256 MB a `stats()` hlásí délky `TRUNCATE` a neúspěšné resety; v release buildu (CI ho tak pouští) proběhne aspoň jeden `TRUNCATE`, neúspěšných resetů není víc než úspěšných a nejdelší čekání čtení na bránu je do 50 ms bez ohledu na rychlost fsync (`SqliteTenant::gate_wait_max`, protože `StoreStats` je typ modelu);
  - záloha za běhu zápisu přes backup API, obnova do nového adresáře: `range` a `changes` vrací totéž co zdroj k okamžiku zálohy;
  - líná migrace při otevření: tenant se starší verzí schématu se otevře a migruje, novější verzi odmítne;
  - LRU zavře nejdéle nepoužitý soubor a znovuotevření je pod 1 ms (měří se, nehlídá).
- **Závisí na:** K0.
- **Souběžně:** K2, K3, K4.

### K2. Rekurence

- **Cíl:** `musubi-recurrence` podle verdiktu [recurrence.md](../research/phase0/recurrence.md): převzaté jádro `rrule` 0.14, hrana nad `jiff`, povinných 12 úprav kromě těch, které se týkají vlastní VTIMEZONE (bod 8 jen pro IANA zóny; zbytek s CalDAV ve fázi 2).
- **Crates:** `musubi-recurrence`.
- **Přijetí:**
  - korpus ze spiku jako fixtures v `tests/differential`: 0 rozdílů proti uloženým očekáváním (dateutil a calcard-next se pouští jen při regeneraci, ne v CI);
  - regresní případy: `dst-prague-explicit-gap-dtstart`, `until-before-dtstart`, `rdate-only`, `dtstart-not-in-rule`, `floating-daily-host-gap` (plovoucí čas nikdy v zóně hostitele);
  - bez okna to nejde zavolat (typově), strop výskytů vrací chybu, ne useknutý výsledek;
  - `proptest`: výskyty jsou seřazené, v okně, bez duplicit, nejvýš strop;
  - fuzz harness převzatý z `rrule-afl-fuzz` běží v nočním CI (upstream harness ověřen 11. 11. 2026: s `cargo-afl` 0.18.2 funguje);
  - licence převzatého kódu (`LICENSE-MIT`, `LICENSE-APACHE`, copyright) jsou v adresáři vendored kódu.
- **Závisí na:** K0.
- **Souběžně:** K1, K3, K4.

### K3. Falešný server Google

- **Cíl:** hermetický server, proti kterému běží testy adaptéru, enginu, crash testy i benchmarky. Žádný test v CI nesahá na síť.
- **Crates:** `musubi-testkit`.
- **Umí:** `calendarList`, `events.list` s `pageToken`, `syncToken`, `timeMin`/`timeMax`, `showDeleted`, `maxResults` do 2500; prázdnou stránku s `nextPageToken`; `410` na prošlý token; `events.watch` a `channels.stop` s doručením oznámení na callback včetně hlaviček kanálu; token endpoint OAuth s obnovou a `invalid_grant`; `429` s `Retry-After`; nastavitelnou odezvu a velikost stránky; skriptované změny („za 2 s změň událost X a pošli oznámení").
- **Přijetí:**
  - vlastní testy serveru: tvar odpovědí odpovídá polím z dokumentace a fixtures se zaznamenanými odpověďmi z K8 (doplní se po živé kontrole);
  - stav serveru je deterministický ze seedu, takže crash test umí porovnat výsledek s pravdou;
  - každá odchylka od dokumentace je v souboru `FIDELITY.md` testkitu.
- **Závisí na:** K0.
- **Souběžně:** K1, K2, K4.

### K4. Adaptér Google

- **Cíl:** `Provider`, `PushSource` a `Codec<Event>` pro Google Calendar.
- **Crates:** `musubi-google`.
- **Přijetí:**
  - zlaté fixtures kodeku (`insta`): událost s časem, celodenní, série s výjimkou, zrušený výskyt, událost se zónou mimo zónu kalendáře, neznámé pole; neznámé pole zůstane v surové vrstvě beze změny;
  - `decode` nedělá I/O a nepanikaří na žádném vstupu (`proptest` nad náhodným JSON);
  - proti K3: úvodní okno, úplné čtení se stránkováním přes prázdnou stránku, inkrement, `410` → `CursorExpired`, `429` respektuje `Retry-After`, `invalid_grant` → `ReauthRequired`;
  - watch: založení, obnova podle vrácené `expiration`, zrušení; `verify` odmítne oznámení s cizím tokenem kanálu.
- **Závisí na:** K0; testy transportu na K3 (kodek a fixtures mohou začít hned).
- **Souběžně:** K1, K2, K3.

### K5. Engine

- **Cíl:** čtecí cesta z kapitoly 3 návrhu od úlohy po commit a proud změn.
- **Crates:** `musubi-engine`.
- **Obsah:** stavový automat účtu (3.1); plánovač nad frontou úloh v `ControlStore` s prioritami a omezovači; zapisovatel tenanta (jeden na tenanta); příjem stránky (dekódování a rozbalení výskytů před transakcí, jedna transakce na stránku); sloučení do entity s jedním umístěním; žurnál; sběrnice změn v paměti; slučování oznámení v okně 100–200 ms; obnova watch kanálů podle `expiration`; pojistné dotazování; posun horizontu indexu výskytů.
- **Přijetí** (vše proti `FakeProvider` a K1):
  - připojení: data okna kolem dneška jsou v úložišti a v žurnálu dřív než výsledek úplného čtení;
  - nezměněný validátor: žádný zápis, žádný záznam v žurnálu;
  - mazání podle nepřítomnosti jen po úplném čtení; pojistka hromadného mazání pozdrží změnu a ověří ji druhým čtením;
  - `CursorExpired` vede na úplné čtení bez duplicit;
  - deset oznámení během 150 ms vyvolá jeden delta požadavek;
  - pronajatá úloha po restartu procesu znovu doběhne, žádná se neztratí ani nespustí dvakrát souběžně;
  - `ReauthRequired` přepne účet do stavu „vyžaduje přihlášení" a dotazování se zastaví;
  - chybějící kalendář v discovery není smazání, pokud discovery není úplné;
  - zrušený výskyt importované série (cizí iCalUID) se k předloze připojí podle `recurringEventId` a nezobrazí se jako živý (otevřená otázka 3);
  - `RecurrenceError::TooExpensive` z rozbalení výskytů (série se v tomto okně nedá rozbalit) engine zviditelní u položky a nikdy ji nebere jako prázdnou sérii: nesmaže kvůli ní výskyty z indexu (test se sérií `FREQ=MINUTELY` se starým DTSTART).
- **Závisí na:** K1, K2. Na K4 ne: engine i jeho testy pracují jen s traity.
- **Souběžně:** K6 a dobíhající K3, K4.

### K6. Server

- **Cíl:** veřejné API fáze 1 a provozní příkazy.
- **Crates:** `musubi-syncd`.
- **API:** `POST /v1/accounts` (OAuth odkaz s PKCE a jednorázovým `state` vázaným na tenanta), `GET /v1/accounts/{id}`, `GET /v1/collections`, `GET /v1/entities?from&to&expand=occurrences` (stránkované, varianta jen s id), `GET /v1/entities/{id}?include=placements`, `GET /v1/items/{id}?include=raw`, `GET /v1/changes?since`, `GET /v1/stream` (SSE), `POST /v1/tokens`, webhook Googlu, správcovská volání (tenant, klíč API, statistiky úložiště). Specifikace OpenAPI je soubor v repozitáři; generované SDK ne.
- **Přijetí:**
  - průkaz jiného tenanta, prošlý token a token bez rozsahu dostanou `401`/`403` a žádná data (testy na každou cestu);
  - klíč API je v `control.db` jen jako hash; krátkodobý token je podepsaný Ed25519 a ověří se bez dotazu do databáze;
  - šifrovaný token účtu přenesený do jiného řádku nejde dešifrovat (AEAD s vazbou na tenanta a účet);
  - webhook s neznámým kanálem nebo špatným tokenem kanálu je odmítnut a nic nezařadí; platný odpoví do 50 ms;
  - SSE doručí kurzor po commitu; klient se starým kurzorem dostane „nutný nový úvodní sync";
  - `musubi-syncd backup` a `restore` fungují nad běžícím, respektive zastaveným uzlem.
- **Závisí na:** K1 (čtecí API může začít hned); K4 a K5 pro spojení.
- **Souběžně:** K5.

### K7. Důkaz: crash testy, záloha v CI, benchmarky

- **Cíl:** projít kritéria hotovosti z kapitoly 1 na celé binárce proti falešnému serveru.
- **Crates:** `musubi-testkit` (harness a generátor zátěže), testy v `musubi-syncd`.
- **Přijetí:** testy a benchmarky z kapitoly 6 procházejí s rozpočty níže.
- **Závisí na:** K3, K4, K5, K6.
- **Souběžně:** crash testy a benchmarky jsou nezávislé; měření na ext4/disku s PLP běží souběžně, pokud je stroj k dispozici.

### K8. Živá kontrola proti Google (spouští uživatel)

- **Cíl:** potvrdit, že falešný server se chová jako Google, změřit skutečnou odezvu a provést T8.
- **Kdo:** uživatel, na vlastním testovacím účtu Google a vlastním projektu GCP. Agent ani CI se na živé účty nepřipojují.
- **Výstup:** `docs/research/phase1/live-google.md` s čísly a odchylkami; zaznamenané odpovědi jako fixtures pro K3 (bez osobních dat).
- **Závisí na:** K7.
- **Souběžně:** nic.

## 6. Testovací strategie

### 6.1 Vrstvy

| Vrstva | Co | Kde běží |
| --- | --- | --- |
| jednotkové a `proptest` | kodek, rekurence, čisté funkce enginu | každý PR |
| kontraktní testy úložiště | invarianty z kapitoly 3.3 proti `musubi-store-sqlite` (později i PG) | každý PR |
| konformační sada transportu | scénáře z kapitoly 6.3 návrhu, bod 2, proti falešnému Google | každý PR |
| crash testy `kill -9` | celá binárka proti falešnému Google | krátce v PR, dlouze v noci |
| záloha a obnova | celá binárka | každý PR |
| benchmarky | cíle z 5.1 | malé měřítko v PR, plné v noci |
| živá kontrola | skutečný Google | ručně, uživatel |

### 6.2 Falešný server Google

Viz K3. Je to jediný „poskytovatel" v CI. Scénáře jsou jazykově neutrální JSON (kapitola 6.3 návrhu), takže jdou později převzít i pro CalDAV. Pravdivost serveru se ověřuje proti zaznamenaným odpovědím z K8 a každá známá odchylka je zapsaná.

### 6.3 Crash testy `kill -9`

- Harness spustí `musubi-syncd` jako podproces nad falešným serverem se seedem a s úvodním i inkrementálním syncem a skriptovanými změnami.
- V náhodném okamžiku pošle `SIGKILL`, proces znovu spustí a nechá ho doběhnout do klidu.
- Body vraždy se vybírají i cíleně: uprostřed `apply_page`, během `TRUNCATE`, mezi commitem a odesláním do SSE, během obnovy watch kanálu, během zálohy.
- Po doběhnutí platí:
  - množina položek a jejich validátorů se rovná stavu falešného serveru (žádná ztráta);
  - jedna položka na vzdálený klíč a jedna entita na položku (žádné duplicity);
  - `seq` v žurnálu je rostoucí a žádná hodnota se neopakuje;
  - klient, který četl `/changes` před pádem, po pádu nepřeskočí žádnou změnu;
  - žádná úloha ve frontě nezůstala pronajatá navždy.
- PR: 20 vražd. Noc: 1 000 vražd a nahlášený seed každé chyby.

### 6.4 Záloha a obnova v CI

- Během běžícího syncu se udělá `musubi-syncd backup`, uzel se zastaví, adresář dat se smaže a `musubi-syncd restore` ho obnoví.
- Po startu se odpovědi `range` a `changes` rovnají stavu k okamžiku zálohy a engine potom dožene změny falešného serveru bez duplicit.
- Test počítá s pořadím `main.db` → `outbox.db`, i když `outbox.db` ve fázi 1 neexistuje.

### 6.5 Benchmarky

Měří je `musubi-testkit` (binárka `bench`) proti `musubi-syncd` a falešnému serveru s nastavitelnou odezvou. Na sdílených runnerech GitHubu jsou čísla hlučná, proto PR hlídá jen řádové regrese v malém měřítku a plné rozpočty běží v noci a na vyhrazeném stroji.

| # | Cíl (kapitola 5.1) | Jak | Rozpočet |
| --- | --- | --- | --- |
| B1 | první data po připojení | odezva falešného serveru 150 ms na požadavek; čas od dokončení OAuth po první záznam v `/changes` | pod 1 s |
| B2 | změna → proud změn s pushem | čas od odeslání oznámení po událost v SSE | pod 500 ms |
| B3 | dotaz na rozsah | tenant 5 000 událostí a 50 sérií, okno 1 týden a 1 měsíc; varianta s payloadem i jen s id; hustý rozsah se jen hlásí (fáze 0: 28 ms, jen id 6,8 ms) | p99 pod 10 ms |
| B4 | změny od kurzoru | `/changes` po 500 | p99 pod 10 ms |
| B5 | vlastní režie inkrementu | čas syncu minus čas strávený u falešného serveru | pod 20 ms |
| B6 | WAL | import 50 000 událostí se souběžnými čtenáři | největší WAL pod 256 MB |
| B7 | paměť aktora | 10 000 spících účtů, RSS na účet | nahlásit; rozpočet se stanoví po prvním měření |
| B8 | cena kodeku | `decode` na událost, mikrobenchmark | nahlásit a hlídat regresi |
| B9 | rekurence | 10 000 sérií v okně 2026–2031 (fáze 0: 2,2–3,1 s) | hlídat regresi |

Přijetí zápisu (p99 pod 10 ms, během importu téhož tenanta pod 20 ms) se měří až ve fázi 3, se souborem outboxu. Odezvu Googlu změří K8.

### 6.6 Živá kontrola (K8)

Předpoklady, které zajistí uživatel:

- testovací účet Google bez osobních dat a vlastní projekt GCP s OAuth klientem; aplikace zůstane neověřená (strop 100 nových uživatelů celkem nevadí);
- veřejná HTTPS adresa pro webhook (tunel); bez ní proběhne kontrola jen s dotazováním a B2 se živě nezměří.

Kroky:

1. Připojit účet; změřit čas do prvních dat (B1 živě).
2. V Google Kalendáři vytvořit, upravit a smazat událost; změřit čas od uložení v UI po SSE.
3. Vytvořit sérii, upravit jeden výskyt a jeden zrušit; porovnat výskyty z `/v1/entities` s UI.
4. Celodenní událost a událost v jiné zóně než kalendář.
5. **T8:** `events.watch` s `expiration` nastavenou na teď + 30 dní a druhý požadavek bez `expiration`; zapsat vrácené `expiration`.
6. Nechat běžet přes noc; zkontrolovat, že se watch kanál obnovil podle `expiration` a nic se neztratilo.
7. Zaznamenat odpovědi (bez osobních dat) pro fixtures K3 a velikost surových těl pro rozhodnutí o kompresi.
8. Ověřit předpoklady adaptéru K4: jaký stav vrací Google na prošlý nebo odmítnutý `pageToken` uloženého kurzoru (K4 čte 400 jako `CursorExpired`) a zda `originalStartTime` přichází i bez `timeZone` (K4 pak čte čas v Etc/UTC; korelace porovnává okamžiky).

Výsledek se zapíše do `docs/research/phase1/live-google.md` a podle něj se upraví čísla v kapitole 5.1 návrhu.

## 7. CI

GitHub Actions, workflow `.github/workflows/ci.yml`, akce připnuté na SHA, cache `Swatinem/rust-cache`. Každý job s `--locked`.

| Job | Příkaz | Kdy |
| --- | --- | --- |
| fmt | `cargo fmt --all --check` | PR, `main` |
| clippy | `cargo clippy --workspace --all-targets --locked -- -D warnings` | PR, `main` |
| test | `cargo test --workspace --locked` (včetně kontraktních testů, zálohy a obnovy, krátkých crash testů) | PR, `main` |
| deny | `cargo deny check` (licence, advisories, bans, sources) přes `EmbarkStudios/cargo-deny-action` | PR, `main`, denně |
| bench-smoke | `cargo run -p musubi-testkit --bin bench --release --locked -- --smoke` | PR |
| nightly | dlouhé crash testy, plné benchmarky, fuzz rekurence | v noci |

Od K0 jsou bench-smoke a nightly zástupné joby, které jen vypíšou, že jsou zástupné: binárku `bench` přidá K7, noční obsah K1 a K7. K1 do nightly přidal `kill -9` uprostřed `apply_page` s 1000 koly (`MUSUBI_CRASH_ROUNDS`), v PR běží 200. Fuzz rekurence (K2) běží v nočním jobu `fuzz-recurrence`. Toolchain instaluje `rustup toolchain install` z `rust-toolchain.toml`, bez další akce.

`deny.toml`:

Ověřeno v K0 (cargo-deny 0.20) proti dnešnímu stromu a proti zkušebnímu crate se všemi plánovanými závislostmi z kapitoly 2 v aktuálních verzích (`reqwest` 0.13, `axum` 0.8, `rusqlite` 0.42, `oauth2` 5, `jsonwebtoken` 11.1 `rust_crypto`, `governor` 0.10 atd.):

- **licence:** povolené MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Unicode-3.0, Zlib. Další jen výjimkou po jednotlivých crates s důvodem. Copyleft (GPL, LGPL, AGPL, EUPL, MPL) zakázaný. Plný strom potřebuje BSD-3-Clause (`curve25519-dalek`, `ed25519-dalek`, `subtle`, `matchit`), ISC (`rustls-webpki`, `untrusted`), Unicode-3.0 (ICU přes `url`) a Zlib (`foldhash`); BSD-2-Clause zatím nic nepotřebuje, zůstává jako běžná permisivní licence. Jediná licence mimo seznam je CDLA-Permissive-2.0 u `webpki-root-certs` (přes `reqwest` 0.13 s rustls): K4 přidá výjimku `[[licenses.exceptions]]` jen pro tento crate s důvodem (permisivní licence dat kořenových certifikátů);
- **advisories:** zakázané včetně stažených verzí (`yanked = "deny"` výslovně, výchozí hodnota cargo-deny je jen varování), výjimka pro RUSTSEC-2023-0071 (`rsa` 0.9 přes `jsonwebtoken`). Soukromý klíč RSA nepoužíváme, krátkodobé tokeny podepisujeme Ed25519. Výjimka má datum revize (1. 2. 2027) a padá, až knihovny přejdou na `rsa` 0.10. Zkušební strom ukázal i RUSTSEC-2026-0387 (časová proměnlivost v `crypto-bigint` 0.5 přes `p256`/`p384` v `jsonwebtoken` `rust_crypto`, opraveno v 0.7.6). Rozhodne K6: buď druhá výjimka s důvodem (ECDSA by se použilo jen k ověření veřejným klíčem), nebo podepisovat Ed25519 přímo přes `ed25519-dalek` bez `jsonwebtoken`;
- **bans:** `multiple-versions` jako chyba; každá výjimka (`skip`) s důvodem a crate, který ji tahá, přidá ten kontrolní bod, který závislost přináší. K0 žádnou nepotřebuje. Předpoklad fáze 0 o `p256`/`p384` a `ed25519-dalek` neplatí: dvě verze těchto crates tahá `openidconnect`, který fáze 1 nepoužívá. Plný strom místo toho potřebuje výjimky pro `base64` 0.22/0.23, `getrandom` 0.2/0.3/0.4, `hashbrown` 0.14/0.16/0.17, `rand` 0.8/0.9 (s `rand_chacha`, `rand_core`), `syn` 2/3, `thiserror` 1/2 (s `thiserror-impl`, přes `oauth2`) a `r-efi` 5/6. Dalším duplicitám se vyhneme volbou verzí: `sha2` 0.10 a `aes-gcm` 0.10 (stejná generace RustCrypto jako `jsonwebtoken`), `tower-http` 0.6 (stejně jako `reqwest`). `oauth2` s `default-features = false` (jinak tahá `reqwest` 0.12) a vlastním HTTP klientem. `reqwest` 0.13 s funkcí `rustls` tahá `aws-lc-rs` (C a CMake), který návrh odmítl; K4 použije `rustls-no-provider` s `ring`. Interní závislosti ve workspace mají `version` vedle `path`, jinak je `wildcards = "deny"` hlásí. Směr závislostí `cargo-deny` nehlídá (kapitola 2);
- **sources:** jen crates.io; git závislosti zakázané (převzaté jádro `rrule` je vendored kód, ne git závislost).

`LIBSQLITE3_FLAGS="-DSQLITE_DEFAULT_MEMSTATUS=0"` je v `.cargo/config.toml`, takže platí lokálně i v CI. Test v K1 ověří, že build volbu opravdu má (`PRAGMA compile_options`).

Přehled licencí pro distribuci (`cargo-about`) přijde s prvním vydáním binárky, ne ve fázi 1.

## 8. Mimo rozsah fáze 1

- Jakýkoli zápis k poskytovateli: `POST /v1/mutations`, soubor outboxu, doručení, podmíněné zápisy, konflikty (fáze 3).
- Druhé umístění, průmět, `lift`, rozeslání, pravidla, plánování mezi tenanty, free/busy.
- CalDAV, iCloud, CardDAV, Google Tasks, People API, Microsoft Graph; `musubi-ical`, calcard a vrstva záplat.
- Vlastní VTIMEZONE a mapa Windows zón (fáze 2 a 7).
- Ověřování tokenů z vnějšího IdP (OIDC, fáze 8).
- Backend PostgreSQL, průběžný odvoz WAL, úložiště objektů. Zálohy jsou ve fázi 1 jen snímky.
- Komprese surové vrstvy: rozhodne se po K8 podle reálných payloadů, implementace až potom.
- Group commit nebo sdílený accept log: měří se jako vstupní podmínka fáze 3.
- Odchozí webhooky pro aplikace, export tenanta, generované SDK, obraz kontejneru, `cargo-about`.
- Exportér metrik (Prometheus, OpenTelemetry). Metriky z podmínky 8 jsou ve fázi 1 v `stats()`, ve správcovském volání a v logu `tracing`.
- Směrování tenantů mezi uzly a víc uzlů vůbec.
- Srovnání s kodeky Musubi.

## 9. Otevřené otázky

Rozhodnuto (koordinátor, 11. 11. 2026): soubor outboxu se implementuje ve fázi 3. Fáze 1 má jen jméno souboru, pořadí zálohy a signatury.

1. **Stroj pro měření ext4/XFS a disku s PLP.** Bez něj zůstane toto měření otevřené i po fázi 1.
2. **Testovací účet Google, projekt GCP a veřejná adresa pro webhook** pro K8.
3. **Uid zrušeného výskytu importované série.** Google vrací zrušený výskyt jen jako skořápku s `recurringEventId` bez `iCalUID`. Adaptér K4 odvodí uid z id předlohy, což u série s cizím iCalUID (import) nesedí s uid předlohy. `musubi-model` se v K4 nemění; K5 rozhodne, zda vazbu na sérii ponese `Correlation`, nebo `RemoteObject`.
4. **Přeskok vpřed u drahých sérií (oprava 12 z `recurrence.md`, odložena).** `TooExpensive` dnes znamená „v tomto okně nerozbalitelné". Známé případy: `FREQ=MINUTELY` (nebo `SECONDLY`) se starým DTSTART, kde okno leží za rozpočtem 1 000 000 vygenerovaných okamžiků, a nemožné pravidlo jako `BYMONTH=2;BYMONTHDAY=30` daleko od DTSTART (jádro skončí po `MAX_ITER_LOOP` = 100 000 prázdných period, `was_limited`). Otázka: zda a kdy doplnit přeskok na začátek okna; do té doby to K5 hlásí, nemlčí.
