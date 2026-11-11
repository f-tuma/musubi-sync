# Fáze 0: úložiště – SQLite na tenanta vs sdílený PostgreSQL

Spike: [`spikes/storage-bench/`](../../../spikes/storage-bench/). Navazuje na návrh, kapitoly 4.3, 4.4, 5.1 a 12.
Měřeno 11. 11. 2026 na jednom stroji. Výsledky jsou ze single runu, viz rizika.

## 1. Otázka

Kapitola 4.3 návrhu nechává volbu mezi SQLite na tenanta (WAL) a sdíleným PostgreSQL otevřenou: má rozhodnout měření. Cíle z kapitoly 5.1:

| Cíl | Hodnota |
|---|---|
| Dotaz na rozsah, položku nebo změny od kurzoru | p99 < 10 ms na serveru |
| Přijetí zápisu (trvale v outboxu) | p99 < 10 ms |
| Vlastní režie jádra na inkrementální sync | < 20 ms |

Měří se:

- propustnost a latence příjmu,
- p50/p99 dotazu na rozsah a změn od kurzoru,
- čtení pod souběžným příjmem,
- cena transakce s plným fsync,
- velikost na disku,
- cena otevírání souborů u SQLite,
- záloha a obnova.

## 2. Krátká odpověď

**Obě varianty splní cíle pro běžného tenanta (5 000 položek) s velkou rezervou. Výkon tedy mezi nimi nerozhoduje a platí dvouvětvové doporučení z kapitoly 4.3.** Data ho upřesňují takto:

- **SQLite je na čtení 2–3× rychlejší.**
  - Rozsah měsíc přes 20 kolekcí: p99 1,00 ms proti 3,39 ms.
  - 8 souběžných čtenářů: 6 369 dotazů/s proti 3 009 dotazům/s.
  - Pod souběžným zápisem: p99 3,7 ms proti **11,2 ms** (PG v tomto běhu cíl těsně nesplnil, v předchozím běhu měl 8,1 ms).
  - PG přitom běžel přes localhost, bez reálného síťového skoku.
- **Trvanlivé přijetí zápisu pod souběhem je u SQLite hraniční.**
  - Outbox s plným fsync, 16 tenantů naráz: SQLite p99 2,9 ms, PG 3,0 ms.
  - Mezi běhy ale SQLite kolísá 2,9–12,2 ms (předchozí běhy 8,8 a 12,2 ms, recenzent 3,0, 3,1 a 9,98 ms), PG 3,0–7,8 ms. SQLite je tedy občas nad hranicí 10 ms, PG má díky group commitu rezervu.
  - Jeden zapisovatel: SQLite p99 1,7 ms, PG 8,1 ms (chvost PG je šum, předchozí běh 3,3 ms).
- **Přijetí zápisu na tenantovi, který právě přijímá velký import, cíl 10 ms spolehlivě nesplní ani jeden backend** (sekce 4.5.1, 3 běhy):
  - SQLite s jedním zapisovatelem na tenanta: p99 205–373 ms, protože mutace čeká na rozpracovanou stránku a reset WAL.
  - SQLite s outboxem v samostatném souboru (varianta A): p99 7,7–16,1 ms bez čtenáře, 54–83 ms s dlouhým čtenářem.
  - PG: p99 ~11–37 ms bez čtenáře (11,4 ms v běhu recenzenta, 22–37 ms v našich třech bězích).
  - Kratší transakce importu (varianta B) nepomáhají (76–173 ms).
  - Varianta A je proto podmínkou pro SQLite. Zdrojem pravdy je soubor outboxu a promítnutí do položek ho následuje.
- **Velký tenant (100 000 položek) s hustým rozsahem cíl nesplní ani jedna varianta.**
  - Měsíc přes 20 kolekcí vrací 11 761 výskytů s payloadem: SQLite p99 28,1 ms, PG 66,2 ms.
  - Jde o tvar API, ne o volbu backendu. Řešením je stránkování nebo dotaz bez payloadu (ids-only: SQLite 6,8 ms, PG 9,9 ms).
- **Inkrementální sync** (stránka 25 položek včetně žurnálu a kurzoru, n = 3 000): SQLite p99 4,0 ms, PG 2,7 ms. Obě varianty jsou hluboko pod rozpočtem 20 ms.
- **Příjem dat** je u obou řádově desítky tisíc položek/s na tenanta a 95–213 tisíc položek/s souhrnně. Úzkým hrdlem bude poskytovatel.
- **WAL u SQLite neroste sám od sebe jen do rozumné velikosti.** Bez explicitního checkpointeru, který WAL resetuje (`TRUNCATE`), narostl v testu na 1,8–3,1 GB. S checkpointerem ze sekce 3.4 je maximum 51–184 MB. Cenou je chvost sólo příjmu velkého tenanta: stránka 250 p99 83–285 ms mezi běhy proti ~10–17 ms u PG, stránka 2500 p99 347–436 ms proti 102 ms. Čtenář, který drží snapshot 1,5 s a hned začíná další, nechá WAL narůst až na 3–7 GB; to žádný checkpoint nevyřeší.
- **Velikost:** ~4,4 KB na položku bez komprese (SQLite) a ~3,9 KB (PG). Odhad 2 KB z návrhu platí jen se zstd v surové vrstvě: 2,3 KB po řádcích, 1,9 KB se slovníkem (nejlepší případ: slovník trénovaný a měřený na syntetických tělech téhož tenanta).

**Doporučení:**

- Datovou rovinu stavět na **SQLite na tenanta** jako na jediném backendu fáze 1, s rozhraním úložiště, které PG nevylučuje.
- PG implementovat až ve chvíli, kdy padne rozhodnutí o hostované službě se závazkem dostupnosti. Jeho výhody jsou provozní (HA, PITR, spravovaná služba), ne výkonové.
- Podmínky pro SQLite viz sekce 6.

## 3. Metoda

### 3.1 Schéma (stejné na obou stranách)

Tabulky:

- `raw(hash → body)`: surová těla adresovaná hashem.
- `items(collection, remote_key, validator, raw_hash, observed, rev, seq, tombstone)`, unikátní podle (kolekce, vzdálený klíč).
- `entities(kind, canonical, rev, seq, tombstone)` a `placements(entity, item)` podle kapitoly 12.
- `occurrences(entity, item, collection, recurrence_id, start_utc, end_utc)` s indexem `(collection, start_utc)`. Je to rozvinutý index výskytů.
- `changes(entity → seq, op)`: žurnál „poslední změna na entitu“ s monotónním `seq` per tenant.
- `mutations(entity, base_rev, patch, idem_key)` a `outbox(placement, mutation, state, payload)`.

Rozdíly mezi backendy:

- PG má navíc sloupec `tenant_id` na začátku každého klíče a indexu a `fillfactor = 90` na tabulkách s update.
- SQLite má soubor na tenanta.
- Kanonická i surová data jsou na obou stranách text (JSON / iCalendar). Nikde neběží komprese ani binární formát.

Dotaz na rozsah:

- Čte `occurrences` přes `collection_id IN (…)`, `start_utc` v okně `[from − 2 dny, to)` a `end_utc > from`.
- Join na `entities` a `items` vrací kanonický payload.
- Deduplikuje přes `GROUP BY entity_id, recurrence_id`, protože jedna entita může mít víc umístění.
- Varianta „ids-only“ vrací jen identifikátory a časy, bez joinu na payload.

Změny od kurzoru: `WHERE seq > ? ORDER BY seq LIMIT 500`, s joinem na entitu.

### 3.2 Data

Generátor `src/datagen.rs` je deterministický (splitmix64), takže obě strany dostanou stejné položky.

**Předpoklady o mixu** (odhad, nevychází z reálných účtů):

- 10 % opakovaných řad:
  - DAILY 5–30 výskytů,
  - WEEKLY 8–105,
  - MONTHLY 6–37,
  - YEARLY 10.
- 8 % celodenních položek.
- 2 % položek jako druhé umístění entity z kolekce 0 (kapitola 12).
- 20 kolekcí na tenanta s vahou ~1/(c+1): několik velkých kalendářů a dlouhý ocas malých.
- Popis má 250–650 znaků. Surové tělo má ~918 B, kanonické ~1 013 B.

Výsledný objem:

| Sada | Obsah |
|---|---|
| Běžný tenant | 5 000 položek, ~23 tisíc výskytů (4,6 na položku), ~530 řad, 62–74 materializovaných entit s více umístěními |
| Celkem | 200 běžných tenantů (1 M položek) + 16 běžných tenantů pro sólo příjem (80 tisíc) + 2 velcí tenanti po 100 000 položkách |
| Velký tenant `t100001` | 448 768 výskytů, 10 085 řad, 1 419 materializovaných entit |

Velký tenant `t100000` byl nahrán stránkou 250 a `t100001` stránkou 2500. Čtení se měří na `t100001`.

### 3.3 Trvanlivost a ladění (fér na obě strany)

Mapování dvou úrovní trvanlivosti z kapitoly 4.4:

| | Stažená data (stránky příjmu) | Autorská data (mutace + outbox) |
|---|---|---|
| SQLite | `synchronous=NORMAL` (WAL: commit bez fsync, fsync při checkpointu) | `synchronous=FULL` jen pro danou transakci (fsync WAL při commitu) |
| PG | spojení příjmu s `synchronous_commit=off` | spojení outboxu s výchozím `synchronous_commit=on` (fsync WAL, group commit) |

**SQLite** (rusqlite 0.41, bundled SQLite):

- Pragmy:
  - `busy_timeout=5000` jako první pragma (už přepnutí do WAL čeká na zámek, místo aby selhalo),
  - `journal_mode=WAL`, `synchronous=NORMAL`,
  - `journal_size_limit=4MB`, `wal_autocheckpoint=65536` jen jako pojistka a vlastní checkpointer (viz 3.4),
  - `temp_store=MEMORY`, `cache_size=-32768` (32 MB), `mmap_size=256 MB`,
  - cache připravených dotazů 64.
- Spojení: jedno zapisovací spojení na tenanta a čtecí spojení zvlášť.
- Zapisovač vkládá po řádcích přes `prepare_cached` a otevírá transakce `BEGIN IMMEDIATE`. Odložená transakce, která se z čtecí mění na zapisovací, dostane během checkpointu `SQLITE_BUSY` bez čekání na busy handler.

**PostgreSQL 17.11** (oficiální image `postgres:17`, host network):

- Paměť: `shared_buffers=8GB`, `effective_cache_size=32GB`, `work_mem=32MB`, `maintenance_work_mem=1GB`.
- WAL a checkpointy: `max_wal_size=16GB`, `min_wal_size=2GB`, `checkpoint_timeout=15min`, `checkpoint_completion_target=0.9`, `wal_buffers=64MB`, `wal_compression=lz4`.
- Planner a IO: `random_page_cost=1.1`, `effective_io_concurrency=200`, `max_connections=200`.
- Klient: tokio-postgres s pipeliningem, 3 round-tripy na stránku.
  - Multi-row upserty přes `unnest`.
  - ID přiděluje aplikace z jednoho čítače na tenanta, takže se nečeká na `RETURNING`.
  - Stránku se 2 500 položkami proto PG zapíše stejným počtem round-tripů jako stránku s 250 položkami.

Oba backendy:

- Leží na stejném btrfs svazku s `chattr +C` (nodatacow, bez komprese), aby CoW a zstd neovlivnily ani jednu stranu.
- Mají stejné indexy.
- Po nahrání dat proběhne `ANALYZE` (SQLite) nebo `VACUUM ANALYZE` (PG).

### 3.4 Dvě zjištění, bez kterých SQLite vychází špatně

Obě zjištění musí převzít produkční kód.

**1. `SQLITE_DEFAULT_MEMSTATUS=0`**

Bundled SQLite má ve výchozím stavu `MEMSTATUS=1`, tedy globální mutex kolem každého malloc. Ten serializuje čtenáře i na různých spojeních a v různých souborech.

A/B na plných datech, stejný build, přepínač `SQLITE_MEMSTATUS` volá `sqlite3_config(SQLITE_CONFIG_MEMSTATUS)` před prvním spojením (`results/ab-memstatus.md`):

| `MEMSTATUS` | 8 čtecích vláken, běžný měsíc / 20 kolekcí | p50 / p99 |
|---|---|---|
| 1 (výchozí) | 1 633 dotazů/s | 4,42 / 6,42 ms |
| 0 | 6 345 dotazů/s | 0,87 / 2,12 ms |

Rozdíl je **~4×** v propustnosti souběžného čtení. Dřívější údaj „8×“ pocházel ze smoke dat a neplatí. Jednovláknové dotazy se mezi oběma běhy liší jen šumem. V produkci stačí build s `SQLITE_DEFAULT_MEMSTATUS=0` (`.cargo/config.toml`, `LIBSQLITE3_FLAGS`). Bez toho by SQLite v testu souběžného čtení vyšel hůř než PG (1 633 proti 3 009 dotazům/s).

**2. Checkpoint WAL: nutná vlastní politika**

Režim WAL sám o sobě neudrží velikost WAL. `PASSIVE` checkpoint přepíše stránky do databáze, ale soubor WAL se resetuje až dalším zápisem ve chvíli, kdy je celý přenesený a žádný čtenář ho nedrží. Pod nepřetržitými čtenáři nebo zapisovatelem k tomu nedojde nikdy. První verze spiku (jen `PASSIVE` každých 50 ms, `wal_autocheckpoint=0`) proto nechala WAL růst bez omezení. Závěr „WAL se po checkpointu recykluje“ z předchozí verze zprávy byl chybný.

Nynější checkpointer (`src/sqlite.rs`, `Checkpointer`):

- `wal_autocheckpoint=65536` (256 MB) jen jako pojistka a `journal_size_limit=4 MB`, aby se WAL po resetu zkrátil.
- Vlákno na pozadí každých 50 ms změří velikost `-wal` a mezi 8 a 64 MB pustí `PASSIVE`. Pod 8 MB a nad 64 MB ho pouští nejvýš jednou za sekundu.
- Nad 64 MB resetuje WAL **sám zapisovač** po commitu stránky příjmu: `PRAGMA wal_checkpoint(TRUNCATE)` s busy timeoutem 10 ms (dřív 100 ms). Vlákno na pozadí by zámek zapisovače pod zátěží nedostalo (vývojový smoke běh, necommitovaný: 48 z 49 pokusů `SQLITE_BUSY`, WAL 270 MB).
- Když `TRUNCATE` nestihne čtenáře do 10 ms, vzdá to a **další pokus udělá až za 1 s nebo po dalších 32 MB WAL** (co nastane dřív). Krátké dotazy do 10 ms skončí; čtenář, který začal až po přenesení WAL, reset neblokuje. Dlouhý čtenář tak stojí zápis nejvýš 10 ms za sekundu, ne 100 ms na každém commitu. Počet neúspěchů a nejdelší `TRUNCATE` se vypisují („busy“, „max“).
- **Přijetí mutace (outbox) reset WAL nevolá.** Na `TRUNCATE` nečeká; WAL za ním uklidí příští stránka příjmu nebo `PASSIVE` na pozadí.
- Spojení checkpointeru otevírá soubor až po jeho vytvoření a nastavení WAL. `busy_timeout` je první pragma, transakce zapisovače jsou `BEGIN IMMEDIATE`.

Strategie checkpointu, velký tenant sólo (`results/ab-ckpt-ingest.md`, `results/ab-ckpt-mixed.md`):

| Strategie | Stránka 250: p50 / p99 / max | položek/s | Stránka 2500 (n = 49): p99 | položek/s | Max WAL |
|---|---|---|---|---|---|
| `wal_autocheckpoint=1000` (výchozí, checkpoint v commitu) | 20,9 / 36,5 / 38,6 | 11 770 | 188,3 | 18 375 | 8–38 MB |
| Checkpointer (64 MB) | 8,0 / 188,5 / 224,5 | 16 383 | 347,2 | 19 245 | 109–131 MB |
| Checkpointer s limitem 256 MB | 8,3 / 77,4 / 125,1 | 24 695 | 162,4 | 28 009 | 273–313 MB |
| Jen `PASSIVE` (původní, neomezený) | 8,4 / 17,0 / 20,3 | 27 903 | 118,2 | 29 359 | **1 861 MB** |
| Mix 8 čtenářů + 8 zapisovatelů, jen `PASSIVE` | čtení p99 3,83 | – | – | – | **3 141 MB** |
| Mix 8 čtenářů + 8 zapisovatelů, checkpointer | čtení p99 4,05 | – | – | – | 169 MB |

Závěry:

- Rychlost původní verze (27,9 tisíc položek/s, p99 17 ms) byla daná tím, že zapisovač za checkpoint nikdy neplatil a WAL se nikdy neresetoval. Proti výchozímu `wal_autocheckpoint=1000` je to **2,4×** v propustnosti (předchozí A/B běh 1,9×). Dřívější údaj „3–4×“ (6,4–9,6 tisíc položek/s, p99 až 463 ms) se v A/B nezopakoval. Proč první měření vyšlo hůř, jsme nerozebírali. **(neověřeno)**
- S omezeným WAL je propustnost 1,4× vyšší než u výchozího checkpointu. Chvost sólo příjmu je ale horší (p99 189–221 ms v tomto běhu, 83–285 ms napříč běhy, proti 25–36 ms), protože zapisovač jednou za ~64 MB dokončí zápis WAL do databáze a zavolá fsync. Nově to měříme přímo: nejdelší `TRUNCATE` trval 216–355 ms, přičemž na čtenáře čekal nejvýš 10 ms a v sólo příjmu žádní čtenáři nejsou. Chvost je tedy zápis a fsync, ne čekání. Výchozí checkpoint platí totéž po menších dávkách v každém ~4 MB commitu (max WAL 8 MB u stránky 250, 38 MB u stránky 2500).
- S limitem 256 MB je `TRUNCATE` vzácný (6 za příjem, max 27 ms) a p99 klesne na 77 ms za cenu WAL ~300 MB.
- Max WAL překračuje limit 64 MB: jedna stránka 2500 přidá desítky MB a po neúspěšném resetu zapisovač čeká 1 s nebo 32 MB. V hlavním běhu bylo maximum 184 MB (sólo stránka 2500) a 135–169 MB (mix).
- Pro latenci čtení na strategii skoro nezáleží (p99 4,1 proti 3,8 ms).
- Příjem nemá v kapitole 5.1 cíl. Inkrementální sync (p99 4,0 ms) a outbox se do limitu WAL dostanou zřídka. Volba prahu je kompromis mezi diskem a chvostem příjmu a patří do fáze 1.
- Původní verze checkpointeru čekala na čtenáře 100 ms při každém commitu nad 64 MB, i v outbox přijetí. Recenzent tím naměřil zapisovač omezený na ~10 commitů/s, dokud čtenář držel snapshot. Teď jde o nejvýš 10 ms jednou za 1 s / 32 MB a přijetí reset nevolá. V mixu narazilo na čtenáře 42 z 321 pokusů (A/B 38 z 326), dřív 315 z 569.

Je to standardní produkční vzor, ale **je to náš kód**:

- musí omezit WAL (bez toho neomezený růst, viz výše),
- musí se vypořádat s tenanty, jejichž reset blokuje dlouhý čtenář: zapisovač tím už nezpomalí, ale WAL roste, dokud čtenáři nenechají mezeru (sekce 4.5: cyklus 1,5 s snapshot / 0,2 s pauza, všechny resety busy, WAL 3–7 GB). Dlouhé čtení (export, záloha) proto musí jít přes backup API / `VACUUM INTO` nebo mít omezenou délku transakce,
- musí spolupracovat s odvozem WAL (sekce 7).

### 3.5 Reprodukce

```sh
cd spikes/storage-bench
./run.sh                 # celé měření (~40 min); výsledky v results/*.md; pak smaže kontejner i data
KEEP=1 ./run.sh          # ponechá kontejner musubi-storage-bench-pg a data/
SKIP_SQLITE=1 KEEP=1 ./run.sh   # jen PG část (SQLite výsledky zůstanou)
SKIP_PG=1 ./run.sh       # jen SQLite, A/B přepínače a acceptmix (pg.md a backup.md zůstanou)
ACCEPTMIX_ONLY=1 ./run.sh   # jen acceptmix: SQLite (3 režimy) a PG
# rychlý smoke test:
TENANTS=20 EVENTS=500 LARGE=3000 CONC=4 COLD=200 QUERIES=50 MIXED_SECS=2 ./run.sh
```

Postup `run.sh`:

1. Spustí `cargo test --release` (determinismus generátoru).
2. Spustí `storage-bench sqlite load size`, pak `sqlite read mixed outbox coldopen`.
3. Spustí `storage-bench pg load size read mixed outbox connect`.
4. Změří zálohu a obnovu.
5. Změří velikost se zstd v surové vrstvě (`size_zstd.py`) a A/B přepínače (MEMSTATUS, strategie checkpointu).
6. Spustí `storage-bench sqlite acceptmix` a `pg acceptmix` (přijetí zápisu během příjmu stejného tenanta, sekce 4.5.1).

Surové výsledky: `results/sqlite.md`, `results/pg.md`, `results/backup.md`, `results/size-zstd.md`, `results/ab-*.md`, `results/acceptmix.md`, `results/env.md` a logy `results/run.log` (celý běh) a `results/run-rerun.log` (opakovaný SQLite běh) a `results/run-acceptmix.log` (acceptmix na obou stranách), viz kapitola 4.

`run.sh` odmítne neprázdný `BENCH_DATA` bez vlastní značky `.storage-bench-data`, odstraní zastavený kontejner se stejným jménem a při ukončení (i chybou) uklidí kontejner a data, pokud není `KEEP=1`. Release build má `panic = "abort"`: pád kteréhokoli vlákna, včetně checkpointeru, ukončí celý běh.

### 3.6 Prostředí

| Položka | Hodnota |
|---|---|
| CPU | Intel Core Ultra 7 265K (20 jader, max 6,5 GHz) |
| RAM | 62 GB, z toho ~17–23 GB obsazeno jinými procesy |
| Disk | NVMe Kingston KC3000 2 TB, btrfs (`compress=zstd:1`, `ssd`, `discard=async`); datový adresář `chattr +C` |
| Software | Linux 7.3.3-cachyos, Docker se 14 dalšími běžícími kontejnery (šum), Rust/cargo 1.99.0 |

Celá sada se vejde do RAM. Čtení proto probíhá s teplou page cache, kromě testu studeného otevírání. PG běží přes TCP na localhostu, takže reálné nasazení přidá síťový RTT na každý round-trip.

## 4. Výsledky

Latence jsou v ms. Po opravě checkpointeru (10 ms čekání, back-off, outbox bez resetu) byla znovu spuštěna celá SQLite část: `sqlite load size`, `sqlite read mixed outbox coldopen`, `size_zstd.py`, A/B přepínače a nový `sqlite acceptmix` (`SKIP_PG=1 ./run.sh`, log `results/run-rerun.log`). PG a záloha se změnou nemají co dělat a pocházejí z předchozího celého běhu (`results/run.log`); stejný stroj, stejný počet kontejnerů v pozadí. Sekce 4.5.1 (`acceptmix`: SQLite ve třech variantách a PG) běžela znovu samostatně 3× (`ACCEPTMIX_ONLY=1 ./run.sh`, log `results/run-acceptmix.log`); nic jiného se nespouštělo. Tučně je hodnota, která nesplní cíl 10 ms tam, kde se cíl vztahuje. U řádků s n < 100 je p99 rovno maximu, ne stabilnímu percentilu.

### 4.1 Příjem (ingest)

Latence je na jednu transakci, tedy na stránku včetně položek, surových těl, výskytů, entit, žurnálu a kurzoru kolekce. U SQLite běží checkpointer ze sekce 3.4 a jeho cena je v číslech zahrnutá.

| Test | SQLite p50 / p99 / max | SQLite položek/s | PG p50 / p99 / max | PG položek/s |
|---|---|---|---|---|
| Velký tenant sólo, stránka 250 (n = 411) | 7,9 / **220,9** / 362,5 (p99 mezi běhy 83–285) | 15 712 | 10,0 / 17,1 / 30,7 | 23 459 |
| Velký tenant sólo, stránka 2500 (**n = 49**) | 80,0 / 436,4 / 436,4 | 20 756 | 63,4 / 102,1 / 102,1 | 35 415 |
| 100 tenantů, 16 zapisovatelů, stránka 250 | 12,2 / 27,5 / 46,0 | 188 986 souhrnně | 11,9 / 36,7 / 54,3 | 206 042 souhrnně |
| 100 tenantů, 16 zapisovatelů, stránka 2500 | 13,4 / 46,2 / 51,0 | **213 272** souhrnně | 11,0 / 334,8 / 788,8 | 95 422 souhrnně |
| Běžní tenanti sólo, stránka 250 | 3,0 / 8,9 / 9,5 | 47 753 | 4,2 / 7,3 / 7,7 | 39 356 |
| Běžní tenanti sólo, stránka 2500 | 3,3 / 15,8 / 16,1 | 52 216 | 3,9 / 37,5 / 49,2 | 36 000 |
| **Inkrementální sync**, stránka 25 (jen transakce, n = 3 000) | 1,05 / **4,00** / 12,1 | – | 1,23 / **2,68** / 12,1 | – |

Poznámky:

- U běžných tenantů stránka 2500 v praxi znamená celou kolekci (největší má ~1 400 položek), protože stránky se dělí po kolekcích.
- Sólo velký tenant: PG je o 49–71 % rychlejší a má o řád užší chvost. p99 SQLite u stránky 250 mezi běhy: 135 a 83 ms (předchozí běh a A/B), 212 a 285 ms (recenzent), 221 a 189 ms (tento běh a A/B). Rozsah je tedy **~80–290 ms proti ~10–17 ms u PG**. Tvoří ho `TRUNCATE` v zapisovači každých ~64 MB: nejdelší trval 355 ms bez jakýchkoli čtenářů, jde tedy o zápis do databáze a fsync (sekce 3.4). Dávkový `unnest` navíc dělá méně práce na řádek než SQLite po řádcích.
- Sólo běžní tenanti: SQLite je rychlejší (48–52 tisíc proti 36–39 tisícům položek/s). WAL u nich nedosáhne 64 MB (max 60 MB), reset tedy neproběhne.
- Souběh 16 zapisovatelů: u stránky 250 jsou varianty srovnatelné, u stránky 2500 má SQLite 2,2× vyšší propustnost a 7× nižší p99. Soubory jsou nezávislé a nesdílí žádný zámek.
- Chvost PG pod souběhem (p99 335 ms u stránky 2500, předchozí běh 174 ms) pochází ze sdíleného WAL a checkpointů. Dál jsme ho nerozebírali.
- Inkrementální sync: max SQLite 12 ms (předchozí běh 42 ms) je ojedinělá špička. `TRUNCATE` při něm neproběhl (WAL max 7,3 MB), příčinu jsme nerozebírali. p99 je v rozpočtu.
- Na poskytovatelích to nezáleží: Google i Graph vracejí řádově stovky položek na požadavek za stovky ms.

### 4.2 Dotaz na rozsah

Měří se 2 000 dotazů s náhodným tenantem a náhodným oknem. Běžní tenanti jsou 0..199, velký tenant je `t100001`.

| Test | Průměr řádků | SQLite p50 / p99 / max | PG p50 / p99 / max |
|---|---|---|---|
| Běžný, týden, 5 kolekcí | 35 | 0,13 / 0,40 / 0,56 | 0,26 / 0,69 / 0,95 |
| Běžný, týden, 20 kolekcí | 140 | 0,36 / 0,50 / 0,62 | 0,88 / 1,53 / 2,09 |
| Běžný, měsíc, 5 kolekcí | 150 | 0,21 / 0,54 / 0,65 | 0,58 / 1,56 / 2,36 |
| Běžný, měsíc, 20 kolekcí | 591 | 0,83 / 1,00 / 1,16 | 2,43 / 3,39 / 8,53 |
| Běžný, měsíc, 20 kolekcí, ids-only | 590 | 0,34 / 0,40 / 0,45 | 0,51 / 0,78 / 0,97 |
| Velký, týden, 5 kolekcí | 705 | 1,24 / 3,44 / 3,99 | 2,63 / 7,03 / 8,51 |
| Velký, týden, 20 kolekcí | 2 783 | 6,44 / 8,63 / 9,43 | 15,91 / **40,24** / 83,96 |
| Velký, měsíc, 5 kolekcí | 2 920 | 4,24 / **13,38** / 17,42 | 10,06 / **35,29** / 79,61 |
| Velký, měsíc, 20 kolekcí | 11 761 | 24,01 / **28,14** / 30,22 | 52,83 / **66,17** / 182,58 |
| Velký, měsíc, 20 kolekcí, ids-only | 11 759 | 5,83 / 6,80 / 7,91 | 7,69 / 9,87 / 11,99 |

Poznámky:

- Čas škáluje s počtem vrácených řádků, a hlavně s payloadem: 11 761 řádků po ~1 KB je ~12 MB na jeden dotaz.
- SQLite je 2–4× rychlejší, protože čte v procesu bez serializace přes drát.
- Cíl p99 < 10 ms SQLite drží do ~2 800 výskytů s payloadem a i bez payloadu při 11 760 výskytech. PG drží do ~700 výskytů s payloadem a bez payloadu je při 11 760 výskytech těsně pod cílem (SQLite 6,8 ms) (9,9 ms, předchozí běh 10,6 ms).
- Férovost: SQLite skládá `IN (…)` s proměnným počtem kolekcí do textu dotazu, takže různé seznamy jsou různé příkazy v cache připravených dotazů (kapacita 64) a část dotazů se znovu připravuje. PG posílá seznam jako pole. SQLite je tím spíš znevýhodněn, rozsah jsme neměřili. **(neověřeno)**
- Měsíc přes 20 kalendářů se ~12 tisíci výskyty je extrém. Běžný uživatel má pro stejný dotaz ~590 výskytů.
- Řešení nezávislé na backendu: rozsah stránkovat (např. limit 2 000 výskytů) nebo vracet ids + časy a payload dotáhnout zvlášť pro viditelné položky.

### 4.3 Změny od kurzoru (limit 500)

| Test | Průměr řádků | SQLite p50 / p99 / max | PG p50 / p99 / max |
|---|---|---|---|
| Běžný, od head − 100 | 60 | 0,07 / 0,11 / 0,15 | 0,35 / 0,63 / 1,03 |
| Běžný, od náhodného kurzoru | 456 | 0,40 / 0,43 / 0,48 | 2,13 / 2,49 / 2,74 |
| Velký, od head − 100 | 100 | 0,05 / 0,05 / 0,16 | 0,39 / 0,64 / 1,08 |
| Velký, od náhodného kurzoru | 498 | 0,43 / 0,50 / 0,53 | 2,10 / 2,43 / 6,77 |

Obě varianty cíl splňují s velkou rezervou, SQLite je 4–10× rychlejší. Širší chvost SQLite u velkého tenanta z předchozího běhu (p99 5,5 ms) se nezopakoval (0,50 ms), byl to tedy šum.

### 4.4 Souběžné čtení a čtení pod zátěží zápisu

| Test | SQLite p50 / p99 / max | PG p50 / p99 / max |
|---|---|---|
| 8 čtenářů, běžný měsíc / 20 kolekcí | 0,87 / 1,99 / 45,37 (6 369 dotazů/s) | 2,60 / 3,56 / 5,44 (3 009 dotazů/s) |
| Mix čtení (měsíc + změny), bez zapisovatelů | 0,79 / 0,99 / 8,06 | 2,52 / 3,50 / 6,91 |
| Mix čtení se **8 zapisovateli** na stejných 8 tenantech | 1,91 / **3,66** / 11,79 | 4,22 / **11,23** / 276,50 |
| Zápis během mixu (update stránek 250) | 2,78 / 15,89 / 800,70; **377 361 položek/s** | 7,77 / 33,64 / 282,41; 155 508 položek/s |

Test je záměrně nepříznivý: zapisovatelé přepisují celé tenanty (verze 1→2→3) ve stejných souborech nebo řádcích, které se čtou.

- SQLite drží čtení pod zápisem na p99 3,7 ms (A/B běh 4,1 ms). Max WAL byl 135 MB (A/B 169 MB), 42 z 321 pokusů o `TRUNCATE` narazilo na čtenáře (předchozí verze checkpointeru 315 z 569).
- PG v tomto běhu cíl nesplnil (p99 11,2 ms, max 276 ms). V předchozím běhu měl 8,1 ms. Je těsně u hranice a závisí na běhu.
- SQLite při tom zapsal 2,4× víc dat. Cenou je chvost zápisu: max 801 ms (A/B běh 574 ms, předchozí běhy 680 a 191 ms). Nejdelší `TRUNCATE` v tomto běhu trval 797 ms (A/B 569 ms), přičemž na čtenáře čeká nejvýš 10 ms. Chvost je tedy zápis 64+ MB WAL do databáze a fsync pod souběžnou zátěží 8 zapisovatelů, ne čekání na čtenáře.
- Férovost: obě strany měří tři kroky (rozsah, `head_seq`, změny). U PG je každý krok round-trip, i když v produkci by klient kurzor znal a `head_seq` by odpadl. PG tím platí jeden round-trip navíc (na localhostu desetiny ms). **(neověřeno)**

### 4.5 Přijetí zápisu s plnou trvanlivostí (outbox)

Jedna transakce obsahuje:

1. vložení mutace s idempotentním klíčem,
2. zvýšení `rev` entity,
3. zápis do žurnálu,
4. řádky outboxu pro každé umístění.

Transakce se commituje s fsync.

| Test | SQLite (`synchronous=FULL`) p50 / p99 / max | PG (`synchronous_commit=on`) p50 / p99 / max |
|---|---|---|
| 1 zapisovatel, 2 000 mutací | 1,02 / 1,74 / 10,42 | 1,16 / 8,09 / 14,42 |
| 16 tenantů současně, 4 800 mutací | 2,11 / 2,89 / 3,81 | 2,13 / 2,96 / 11,82 |

SQLite v tomto běhu cíl splnil, ale mezi běhy kolísá:

| Běh | SQLite p99, 16 tenantů | PG p99, 16 tenantů |
|---|---|---|
| Původní zpráva | **12,18** | 7,79 |
| Recenzent | 9,98 | – |
| Předchozí běh | 8,83 | 2,96 |
| Tento běh (po opravě checkpointeru) | 2,89 | – |

Recenzent v dalších bězích naměřil 3,0 a 3,1 ms. Rozptyl mezi běhy (2,9–12,2 ms) je větší než rezerva do cíle; oprava checkpointeru outbox nezměnila (16 tenantů s 300 mutacemi WAL 64 MB nedosáhne), takže rozdíl je šum stroje. **(neověřeno)** Pro rozhodnutí je proto SQLite potřeba brát jako „kolem 10 ms“.

Příčina:

- 16 souborů znamená 16 nezávislých fsync WAL.
- PG sloučí souběžné commity do jednoho fsync (group commit).
- Při stovkách aktivních tenantů na uzlu se rozdíl pravděpodobně zvětší. **(neověřeno)**
- Měřeno jen na btrfs nodatacow a spotřebitelském NVMe. ext4 ani XFS nebyly k dispozici (bez root práv nejde vytvořit loop svazek). Cena fsync tam může být jiná oběma směry. **(neověřeno)**

Možné cesty pro SQLite (žádná nebyla měřena):

- Krátké dávkování přijetí v rámci jednoho tenanta.
- Sdílený „accept log“ uzlu s group commitem: zápis do jednoho souboru s fsync, do tenantového souboru až potom.
- Disk s PLP (power-loss protection), kde je fsync levný.

#### 4.5.1 Přijetí zápisu během příjmu stejného tenanta (`acceptmix`)

Velký tenant přijímá import po stránkách 250: tři verze (0–2), 1 189 měřených stránek. Současně přichází ~50 mutací/s na stejného tenanta. Měří se čas od zařazení mutace do commitu s plnou trvanlivostí (`synchronous=FULL` u SQLite, `synchronous_commit=on` u PG). Každá varianta má dva průchody: bez čtenáře a s čtenářem, který opakovaně drží snapshot 1,5 s s pauzou 0,2 s (export, dlouhý dotaz).

Varianty:

- **SQLite `same`** (dosavadní stav): mutace i stránka importu jdou přes jediného zapisovatele tenanta. Mutace čeká, než doběhne rozpracovaná stránka včetně případného `TRUNCATE`.
- **SQLite A – `split`, samostatný soubor outboxu:**
  - Mutace a outbox jsou v druhém souboru na tenanta (`tNNNNN-outbox.db`) s vlastním zámkem zapisovatele a vlastním WAL. Přijetí tedy nečeká na transakci importu.
  - Rev entity a záznam do žurnálu `changes` v hlavním souboru promítá zapisovatel importu až před další stránkou. Dělá to v jedné transakci spolu s vodoznakem `meta.projected` (id poslední promítnuté mutace).
  - Checkpointer ze sekce 3.4 obsluhuje oba soubory.
- **SQLite B – `chunked`, kratší transakce importu:** stránka 250 je rozdělená na 10 transakcí po 25 položkách a čekající mutace se zapíšou mezi nimi.
- **PG:** přijetí má vlastní spojení vedle spojení importu a se zapisovatelem importu se potká jen na zámcích řádků (`tenants.seq`, překrývající se entity).
  - Zkratka v benchmarku: `seq` přijetí běží v disjunktním rozsahu, aby nečekalo na zámek řádku `tenants` přes celou stránku. Skutečný návrh potřebuje kurzor bezpečný vůči pořadí commitů.

Spuštěno 3× (`ACCEPTMIX_ONLY=1 ./run.sh`, `results/acceptmix.md` a `results/run-acceptmix.log`, běhy 1–3). Ostatní části benchmarku se změnou nesouvisí a znovu neběžely.

Přijetí p99 v ms, po bězích 1 / 2 / 3:

| Varianta | Bez čtenáře | Dlouhý čtenář |
|---|---|---|
| SQLite `same` | **204,8 / 280,7 / 372,7** | **316,2 / 264,3 / 280,8** |
| SQLite A `split` | **10,9** / 7,7 / **16,1** | **53,5 / 83,3 / 62,1** |
| SQLite B `chunked` | **172,9 / 78,8 / 76,2** | **613,0 / 206,4 / 218,5** |
| PG | **25,8 / 21,7 / 37,2** | **22,6** / **10,8** / **10,8** (import zpomalen na 4,6 tis. položek/s, viz níže) |

Přijetí p50 je 9,5–13,9 ms u `same`, 1,3–3,1 ms u A, 4,2–6,6 ms u B a 2,0–4,3 ms u PG.

Import (stránka 250) a WAL:

| Varianta | Stránka p99 bez čtenáře / s čtenářem | Položek/s bez / s čtenářem | Max WAL bez / s čtenářem |
|---|---|---|---|
| SQLite `same` | 191–228 / 30–46 | 11,2–12,1 tis. / 14,2–15,5 tis. | 108–145 MB / **4,8–6,0 GB** |
| SQLite A `split` | 139–146 / 23–41 | 15,3–15,4 tis. / 15,5–17,0 tis. | 109 MB / **6,0 GB** |
| SQLite B `chunked` | 122–143 / 214–350 | 9,4–10,5 tis. / 9,0–11,4 tis. | 102–103 MB / **3,4–7,2 GB** |
| PG | 23–29 / 22 (běh 1), 194–209 (běhy 2, 3) | 26,1–28,6 tis. / 26,9 tis. (běh 1), 4,6 tis. (běhy 2, 3) | – |

Max WAL u A se týká hlavního souboru.

**Co z toho plyne:**

- **Cíl 10 ms nesplní žádná varianta spolehlivě.** Bez A je to u SQLite o řád mimo, A je těsně kolem hranice, PG je 1–4× nad.
- **A (samostatný soubor outboxu) problém z velké části řeší.**
  - Bez čtenáře je p99 7,7–16,1 ms, tedy 20× méně než `same`; cíl splnil 1 ze 3 běhů.
  - Zbytek chvostu je pravděpodobně fsync souboru outboxu v souběhu s `TRUNCATE` a zápisem hlavního souboru na stejném disku (btrfs, sdílený stroj). **(neověřeno)**
  - S dlouhým čtenářem je p99 54–83 ms. Hlavní WAL přitom roste na 6 GB a disk je zahlcený zápisem. **(neověřeno)**
  - Import s A je o ~30 % rychlejší než `same`: zapisovatel importu nestřídá stránky s fsync mutací.
- **B (kratší transakce importu) nepomáhá.** p99 76–173 ms bez čtenáře, 206–613 ms s čtenářem.
  - Mutace sice nečeká na celou stránku, ale chvost netvoří délka transakce. Tvoří ho `TRUNCATE` a fsync po commitu. Těch je s menšími transakcemi víc (118–123 resetů proti 81–83).
  - Import je o ~15 % pomalejší.
- **PG cíl v tomto scénáři také nesplní.**
  - Bez čtenáře je p99 ~11–37 ms (22–37 ms v našich třech bězích, 11,4 ms v běhu recenzenta). Pravděpodobné příčiny **(neověřeno)**:
    - commit přijetí musí fsyncnout sdílený WAL včetně předchozích záznamů importu;
    - přijetí čeká na zámky řádků, které drží rozpracovaná stránka.
  - S dlouhým čtenářem vyšlo p99 10,8 ms v bězích 2 a 3, ale import v nich zpomalil 6× (4,6 tis. položek/s, stránka p99 ~200 ms). Přijetí tedy soupeřilo s výrazně menší zátěží. V běhu 1 import nezpomalil a přijetí mělo p99 22,6 ms.
  - Proč držený snapshot v bězích 2 a 3 import zpomalil a v běhu 1 ne, jsme nerozebrali. Možná příčina je, že snapshot brání úklidu (HOT pruning) mrtvých verzí řádků přepisovaných importem. **(neověřeno)**
- **WAL s nepřetržitým cyklem dlouhých čtenářů u SQLite neomezí nic.** Ve všech variantách narostl na 3,4–7,2 GB, protože každý `TRUNCATE` je busy. Checkpoint nesmí přepsat stránky, které čtenář vidí, a reset WAL potřebuje okamžik bez čtenáře. Dlouhá čtení (export, záloha) musí jít přes backup API / `VACUUM INTO` nebo mít omezenou délku transakce. To je i podmínka pro chvost přijetí s A.

**Atomicita u varianty A.**

- Přijetí mutace a její promítnutí do položek a žurnálu jsou dva commity ve dvou souborech. Nejsou jedna atomická transakce.
- `ATTACH` a transakce přes oba soubory atomicitu nevrátí: v režimu WAL je commit atomický jen v rámci každého souboru zvlášť. Při pádu mezi nimi se jeden soubor commitne a druhý ne.
- Zdrojem pravdy proto musí být soubor outboxu:
  - mutace je přijatá, jakmile je commitnutá tam;
  - projekce do hlavního souboru se dělá až potom, idempotentně a s vodoznakem (`meta.projected`) commitnutým ve stejné transakci jako projekce;
  - po pádu se promítnou mutace s id nad vodoznakem.
  - Vodoznak předpokládá, že id mutací jen rostou. `INTEGER PRIMARY KEY` bez `AUTOINCREMENT` může po smazání doručených řádků znovu použít nejvyšší id a replay by pak mutaci s takovým id přeskočil. Proto buď `AUTOINCREMENT`, nebo nikdy nemazat řádky nad vodoznakem (benchmark má `INTEGER PRIMARY KEY` a nemaže).
- Důsledky:
  - Čtení položek může krátce zaostávat za přijatou mutací. Klient, který potřebuje „read-your-writes“, musí číst i nepromítnuté mutace z outboxu nebo počkat na vodoznak.
  - Kontrola základní revize (`base rev`) při přijetí se dělá proti hlavnímu souboru. Musí zahrnout i nepromítnuté mutace téže entity, jinak dvě rychle po sobě jdoucí mutace projdou se stejnou bází. Benchmark tuto kontrolu zjednodušuje (zkratka v kódu). Navíc čte `base rev` a umístění (`placements`) ve dvou samostatných dotazech, tedy ve dvou snapshotech hlavního souboru; mezi nimi může commitnout import. Skutečná implementace je má číst v jedné čtecí transakci.
  - Záloha musí vzít nejdřív hlavní soubor a potom outbox. Outbox nesmí být starší než vodoznak v hlavním souboru; rozdíl pak dorovná replay.

### 4.6 Velikost na disku

| | SQLite | PG |
|---|---|---|
| Celá sada (1,28 M položek: 200 × 5 000 + 16 × 5 000 + 2 × 100 000) | 5,58 GB | 4,95 GB (bez WAL) |
| Na položku | ~4,36 KB | ~3,87 KB |
| Běžný tenant (5 000) | 22,1 MB | – |
| Velký tenant (100 000) | 433 MB | – |

Rozpad PG:

| Tabulka | Velikost |
|---|---|
| items | 1 930 MB |
| raw | 1 494 MB |
| occurrences | 945 MB |
| placements | 237 MB |
| entities | 170 MB |
| changes | 164 MB |

Obě strany ukládají surová i kanonická data jako nekomprimovaný text. Odhad z kapitoly 4.3 (~2 KB na událost) ale předpokládá komprimovanou surovou vrstvu. Proto jsme změřili SQLite soubor se zstd v `raw.body` (`size_zstd.py`, `results/size-zstd.md`). Skript zkopíruje soubor tenanta, přepíše těla na zstd úrovně 3 a udělá `VACUUM`.

| Tenant | Surová vrstva | Uložená těla | Tabulka `raw` | Soubor | Na položku |
|---|---|---|---|---|---|
| běžný (5 000) | text | 5,1 MB | 14,0 MB | 21,2 MB | 4 245 B |
| běžný | zstd po řádcích | 3,1 MB | 5,5 MB | 11,7 MB | 2 343 B |
| běžný | zstd se slovníkem (32 KB, trénovaný na 2 000 tělech téhož tenanta; nejlepší případ) | 1,0 MB | 5,5 MB | 9,3 MB | 1 869 B |
| velký (100 000) | text | 93,4 MB | 263,8 MB | 412,5 MB | 4 125 B |
| velký | zstd po řádcích | 56,3 MB | 99,6 MB | 230,5 MB | 2 305 B |
| velký | zstd se slovníkem | 17,7 MB | 99,2 MB | 187,1 MB | 1 871 B |

Závěry:

- **Odhad 2 KB z návrhu platí jen s kompresí surové vrstvy:** ~2,3 KB po řádcích, ~1,9 KB se slovníkem. 1,9 KB je nejlepší případ: slovník je trénovaný na 2 000 tělech téhož tenanta a použitý na stejná syntetická data, která mají pravidelnější strukturu než reálné události. U reálných dat a slovníku sdíleného mezi tenanty bude výsledek mezi 1,9 a 2,3 KB, možná blíž 2,3 KB. **(neověřeno)** Bez komprese je to ~4,4 KB, tedy ~2,2 TB pro 100 000 × 5 000 místo ~1 TB.
- **Většina úspory není z komprese, ale z přetečení stránek.** Tabulka `raw` je `WITHOUT ROWID` (B-strom indexu). Těla nad ~1 000 B se tam nevejdou na stránku a každé zabere vlastní přetokovou stránku 4 KB. 5,1 MB textu proto zabírá 14,0 MB. Po zstd se těla vejdou a tabulka klesne na 5,5 MB, i když se data zmenšila jen na 3,1 MB. Tabulka `raw` s rowid nebo větší stránka by část úspory dala i bez komprese. **(neověřeno)**
- Se slovníkem jsou těla ~3× menší než se zstd po řádcích, soubor je ale menší jen o ~20 %. Zbytek tvoří kanonická data, výskyty a indexy. Slovník (32 KB na tenanta) v souboru není započtený.
- PG `raw` (1 494 MB) TOAST nekomprimuje, protože těla jsou pod prahem ~2 KB. Komprese by tam musela být v aplikaci stejně jako u SQLite.
- Celá záloha `tar | zstd -1` má 827 MB (6,7×), ale to je komprese mimo databázi, ne velikost na disku.

### 4.7 SQLite: otevírání souborů, LRU a migrace

Data: 10 000 malých tenantů (50 položek, 2 705 MB). Vytvoření trvalo 16,0 s.

| Test | p50 | p99 | max | Pozn. |
|---|---|---|---|---|
| Otevření + pragmy + první dotaz, **studená** page cache (`fadvise DONTNEED`) | 0,55 | 0,94 | 3,41 | 10 000 souborů za 6,5 s |
| Zavření (studená) | 0,05 | 0,10 | 0,17 | |
| Otevření + první dotaz, teplá cache | 0,14 | 0,20 | 0,41 | 10 000 souborů za 1,8 s |
| Zavření (teplá) | 0,04 | 0,06 | 0,21 | |
| Požadavek přes LRU 1 000 otevřených z 10 000 (Zipf s = 1,1) | 0,010 | 0,28 | 0,84 | hit rate 77,8 %, RSS +164 MB při 1 000 otevřených |
| Migrace `ADD COLUMN` při otevření | 2,00 | 8,32 | 40,03 | 10 000 souborů ≈ 21 s sekvenčně |
| *Pro srovnání PG: nové spojení + prepare + první dotaz* (n = 200) | 3,87 | 5,71 | 5,88 | to, co ušetří pool |

Otevření databáze na miss LRU je levné: < 1 ms i se studenou cache. LRU s ~1 000 otevřenými soubory stojí ~164 KB RSS na soubor.

Líná migrace při otevření stojí p99 ~8 ms a max 40 ms (předchozí běh 11 a 98 ms) na první požadavek po nasazení. Je to přijatelné, ale tuto latenci uvidí první požadavek každého tenanta. Alternativou je migrovat na pozadí po nasazení.

### 4.8 Záloha a obnova celé sady

| | Čas | Velikost |
|---|---|---|
| SQLite: kopie checkpointovaných souborů + `sync` (`cp --reflink=never`) | 3,0 s | 5,5 GB |
| SQLite: `tar \| zstd -1 -T0` + `sync` | 2,1 s | 827 MB |
| SQLite: obnova `zstd -d \| tar -x` + `sync` | 3,3 s | |
| PG: `pg_dump -Fc -Z lz4` | 5,7 s | 975 MB |
| PG: `pg_restore -j 8` do nové DB (včetně stavby indexů) | 8,5 s | |

SQLite se zálohoval v klidu, po checkpointu, bez souběžného zápisu. Za provozu je potřeba `VACUUM INTO`, backup API nebo odvoz WAL (viz sekce 7). PG dump je logický a konzistentní za provozu, ale není to PITR. Obnova SQLite zálohy je ~2,5× rychlejší a obnovu jednoho tenanta lze dělat po souboru.

## 5. Srovnání s cíli

| Cíl | SQLite | PG |
|---|---|---|
| Rozsah p99 < 10 ms, běžný tenant | ✅ max p99 1,00 ms | ✅ max p99 3,39 ms |
| Rozsah p99 < 10 ms, velký tenant s payloadem | ✅ do ~2 800 výskytů; ❌ 13,4–28,1 ms nad 2 900 výskytů | ✅ do ~700 výskytů; ❌ 35,3–66,2 ms nad 2 700 výskytů |
| Rozsah ids-only, 11 760 výskytů | ✅ 6,8 ms | ⚠️ 9,9 ms (předchozí běh 10,6 ms) |
| Změny od kurzoru p99 < 10 ms | ✅ ≤ 0,52 ms | ✅ ≤ 2,5 ms |
| Čtení p99 < 10 ms pod zátěží zápisu | ✅ 3,7 ms | ⚠️ 11,2 ms (předchozí běh 8,1 ms) |
| Přijetí zápisu p99 < 10 ms, 1 zapisovatel | ✅ 1,7 ms | ✅ 8,1 ms (předchozí běh 3,3 ms) |
| Přijetí zápisu p99 < 10 ms, 16 tenantů současně | ⚠️ 2,9 ms (mezi běhy 2,9–12,2 ms) | ✅ 3,0 ms (mezi běhy 3,0–7,8 ms) |
| Přijetí zápisu p99 < 10 ms během importu stejného tenanta | ❌ jeden zapisovatel 205–373 ms; ⚠️ s outboxem v samostatném souboru (A) 7,7–16,1 ms bez čtenáře, ❌ 54–83 ms s dlouhým čtenářem; ❌ kratší transakce (B) 76–173 ms (sekce 4.5.1) | ❌ ~11–37 ms bez čtenáře; 11–23 ms s dlouhým čtenářem |
| Režie inkrementálního syncu < 20 ms | ✅ p99 4,0 ms | ✅ p99 2,7 ms (+ síť) |
| Velikost WAL omezená | ⚠️ max 184 MB s vlastním checkpointerem (bez něj 1,8–3,1 GB); s nepřetržitým cyklem dlouhých čtenářů 3–7 GB | – (sdílený WAL, `max_wal_size`) |

⚠️ = výsledek kolísá kolem hranice cíle mezi běhy.

## 6. Verdikt a doporučení

**Čísla nevedou k PostgreSQL.**

- Na hlavní čtecí cestě je SQLite 2–4× rychlejší.
- Pod souběžným zápisem čte s nižším chvostem (p99 3,7 ms proti 11,2 ms).
- Pod souběhem tenantů s velkými stránkami má vyšší propustnost příjmu.
- Zálohuje a obnovuje rychleji.

PG je výkonově lepší ve třech bodech:

- **trvanlivé přijetí zápisu pod souběhem**: díky group commitu má stabilní rezervu (p99 3,0–7,8 ms). SQLite kolísá mezi běhy 2,9–12,2 ms.
- **sólo příjem velkého tenanta**: 1,5–1,7× vyšší propustnost a o řád užší chvost (stránka 250: p99 ~10–17 ms proti **~80–290 ms** mezi běhy). Chvost SQLite je cena za omezený WAL: `TRUNCATE` zapíše WAL do databáze a fsyncne (max 216–355 ms). Kapitola 5.1 pro příjem cíl nemá, poskytovatel je pomalejší.
- **WAL bez vlastního kódu**: SQLite potřebuje explicitní checkpointer. Bez něj WAL roste na jednotky GB, s ním zůstává do ~180 MB za cenu výše uvedeného chvostu. Nepřetržitý cyklus dlouhých čtenářů ho nechá narůst na 3–7 GB i s checkpointerem.

**Přijetí zápisu během importu stejného tenanta PG nezvýhodňuje** (sekce 4.5.1). Cíl p99 < 10 ms tu na tomto HW spolehlivě nesplní žádná varianta:

- SQLite s jedním zapisovatelem na tenanta je o řád mimo (205–373 ms).
- SQLite s outboxem v samostatném souboru (A) je bez dlouhého čtenáře nejblíž cíli (7,7–16,1 ms, PG ~11–37 ms). Náskok proti PG je tu ale menší, než ukazují naše tři běhy: recenzent naměřil u PG 11,4 ms.
- S dlouhým čtenářem je A horší než PG (54–83 ms proti 11–23 ms).
- Kratší transakce importu (B) nepomáhají.

Pro volbu backendu to znamená:

- Bez varianty A by tento bod rozhodl proti SQLite.
- S variantou A a omezenou délkou čtecích transakcí je SQLite na tom stejně nebo lépe než PG.
- Cíl 10 ms pro tento scénář na sdíleném disku bez PLP nesplní ani jeden backend. Buď ho pro přijetí během importu téhož tenanta uvolnit (např. na p99 < 20 ms), nebo ho ověřit na cílovém HW. **(neověřeno)**

Ani jeden bod nemění odhad z kapitoly 4.3: výkon nerozhoduje, rozhoduje provoz. Body 1 a 3 jsou ale riziko na straně SQLite, které musí fáze 1 vyřešit, ne jen zdokumentovat.

**Doporučení:**

1. **Fáze 1 staví datovou rovinu na SQLite na tenanta** (WAL, jeden zapisovatel na tenanta, LRU otevřených souborů). Platí pro self-host i pro první hostovanou instanci bez smluvního SLA.
2. **Podmínky pro SQLite** (bez nich vychází hůř než PG):
   - build s `SQLITE_DEFAULT_MEMSTATUS=0` (4× propustnost souběžného čtení);
   - `busy_timeout` jako první pragma a zapisovací transakce `BEGIN IMMEDIATE`;
   - vlastní checkpointer: `PASSIVE` na pozadí, `TRUNCATE` v zapisovači po stránce příjmu nad prahem (zde 64 MB) s čekáním na čtenáře nejvýš 10 ms a back-offem po neúspěchu (1 s nebo +32 MB), `journal_size_limit` a vysoký `wal_autocheckpoint` jako pojistka. Přijetí mutace reset nevolá. Práh nastavit podle priority disku proti chvostu příjmu (sekce 3.4). Monitorovat max WAL, délku `TRUNCATE` a počet neúspěšných resetů na tenanta;
   - mutace a outbox v **samostatném souboru na tenanta** (vlastní zámek zapisovatele a WAL, `synchronous=FULL`), jinak přijetí během importu téhož tenanta čeká na stránku a reset WAL (p99 205–373 ms místo 7,7–16,1 ms, sekce 4.5.1). Zdrojem pravdy je soubor outboxu. Promítnutí do položek a žurnálu dělá zapisovatel tenanta idempotentně, s vodoznakem ve stejné transakci, a po pádu ho přehraje. `ATTACH` s transakcí přes oba soubory atomicitu v režimu WAL nedává. Kontrola základní revize musí zahrnout nepromítnuté mutace;
   - omezit délku čtecích transakcí; dlouhá čtení (export, záloha) přes backup API / `VACUUM INTO`, jinak WAL roste na GB (3–7 GB v testu) a chvost přijetí i s outboxem v samostatném souboru stoupne na 54–83 ms;
   - zápis stránky jako jedna transakce s `synchronous=NORMAL` a mutace s `FULL`;
   - čtecí a zapisovací spojení zvlášť.
3. **Vyřešit přijetí zápisu pod souběhem dřív, než se to stane problémem:**
   - změřit dávkování nebo sdílený accept log s group commitem ve fázi 1;
   - do té doby počítat s p99 ~3–12 ms při 16 souběžných tenantech na uzel na tomto HW, tedy občas nad cílem;
   - přijetí během importu stejného tenanta: s outboxem v samostatném souboru p99 7,7–16,1 ms, tedy kolem hranice. Zbytek chvostu je pravděpodobně souběh fsync obou souborů na jednom disku **(neověřeno)**. Ověřit na disku s PLP a ext4/XFS; kratší transakce importu (B) nepomohly;
   - sólo chvost importu (p99 ~80–290 ms) zmírnit vyšším prahem WAL (256 MB: p99 77 ms) nebo dávkovým vkládáním;
   - změřit na ext4/XFS a na serverovém disku s PLP.
4. **API rozsahu stránkovat** (limit výskytů, kurzor) a nabídnout variantu bez payloadu. Bez toho cíl 10 ms nesplní žádný backend u hustých kalendářů.
5. **PostgreSQL zůstává druhým backendem za úzkým rozhraním úložiště.** Implementovat ho, až padne rozhodnutí o hostované službě se závazkem dostupnosti. Spike ukazuje, že port je přímočarý (stejné schéma s `tenant_id`; pipelining a `unnest`). Provozní výhody PG (synchronní replika, PITR, spravovaná služba) jsou pak hlavní argument, ne výkon.
6. **Rozhodnout o kompresi surové vrstvy a podle toho upravit odhad velikosti** v kapitole 4.3. Odhad 2 KB na událost platí jen se zstd v `raw` (2,3 KB po řádcích, 1,9 KB se slovníkem na tenanta jako nejlepší případ na syntetických datech). Bez komprese je to ~4,4 KB, tedy ~2,2 TB pro 100 000 × 5 000. V každém případě nepoužívat `WITHOUT ROWID` pro těla kolem 1 KB (přetokové stránky, sekce 4.6).

## 7. Provozní trade-offy

| | SQLite na tenanta | PostgreSQL |
|---|---|---|
| Záloha | soubor na tenanta; snímek přes `VACUUM INTO` / backup API; celý uzel 2–3 s na 5,5 GB (měřeno v klidu) | `pg_dump` 5,7 s; PITR přes `pg_basebackup` + archiv WAL (standardní, neměřeno) |
| Průběžný odvoz WAL | Litestream-like proces nebo vlastní kód, musí spolupracovat s naším checkpointerem **(neověřeno)**; na 10 000 souborů to je 10 000 proudů | jeden proud WAL (`archive_command`, pgBackRest, WAL-G) |
| HA | žádná; obnova tenanta na jiný uzel = stažení souboru (+ přehrání WAL); tenant je vázán na uzel, potřeba směrování | synchronní replika, failover (Patroni, spravované služby) |
| Migrace schématu | N souborů; líně při otevření (p99 ~11 ms na první požadavek) nebo na pozadí; ~21 s sekvenčně na 10 000 tenantů | jedna transakce; `ADD COLUMN` bez defaultu je okamžitý, velké `ALTER` zamykají sdílenou tabulku |
| Izolace tenantů | soubor = izolace; export / smazání = práce se souborem | hluční sousedé ve sdíleném WAL a buffer poolu (chvost příjmu p99 174–335 ms pod souběhem); RLS |
| Bloat a WAL | WAL se sám neresetuje, dokud ho drží čtenáři nebo nepřetržitý zapisovatel (měřeno: 1,8–3,1 GB jen s `PASSIVE`). Omezí ho jen vlastní politika s `TRUNCATE` (měřeno: max 51–184 MB; s nepřetržitým cyklem dlouhých čtenářů 3–7 GB); `VACUUM` per soubor podle potřeby | MVCC bloat po masivních upsertech, autovacuum ladění (v testu nesledováno) |
| Spojení | otevření < 1 ms, žádný pool | nové spojení ~3,9 ms, nutný pool (PgBouncer / deadpool) |
| Self-host | jeden binární soubor | další služba k provozu a aktualizaci |

## 8. Otevřená rizika a co spike neměřil

- **Šum a jediný běh.** Na stroji běželo 14 dalších kontejnerů a ~17–23 GB RAM bylo obsazeno. Čísla jsou z jednoho běhu na variantu, bez opakování a intervalů spolehlivosti. Chvosty (p99, max) jsou nejméně stabilní část. Srovnání běhů ukazuje, jak velký šum je: outbox SQLite při 16 tenantech p99 2,9–12,2 ms, čtení PG pod zápisem 11,2 proti 8,1 ms, sólo příjem velkého tenanta SQLite p99 83–285 ms, max zápisu v mixu 574–801 ms, přijetí během importu (jeden zapisovatel) p99 21–373 ms bez čtenáře. Jen `acceptmix` (sekce 4.5.1) běžel 3× a uvádí rozsah všech tří běhů; u PG s dlouhým čtenářem se v nich import lišil 6× (26,9 proti 4,6 tis. položek/s). Řádky s n < 100 (sólo stránka 2500, n = 49) jsou označené a jejich p99 je maximum.
- **ext4/XFS neměřeno.** Bez root práv nešlo vytvořit jiný souborový systém. Vše běželo na btrfs nodatacow. Rozdíl v ceně fsync ovlivní hlavně outbox pod souběhem a reset WAL. **(neověřeno)**
- **Checkpointer a dlouzí čtenáři.** Po opravě (10 ms čekání, back-off, přijetí bez resetu) narazilo v mixu na čtenáře 42 z 321 pokusů o `TRUNCATE`. S čtenářem, který drží snapshot 1,5 s v nepřetržitém cyklu, zapisovač nehladoví (stránka p99 23–46 ms), ale WAL narostl na 3–7 GB a přijetí mutací na stejném tenantovi má p99 264–316 ms s jedním zapisovatelem a 54–83 ms s outboxem v samostatném souboru (sekce 4.5.1). Příčinu chvostu jsme nerozebrali **(neověřeno)**. Chování při stovkách tenantů na uzlu jsme neměřili.
- **Férovost dotazů.** PG platí v mixu round-trip navíc za `head_seq` (sekce 4.4). SQLite skládá `IN (…)` do textu dotazu, takže cache připravených dotazů částečně netrefuje (sekce 4.2). Ani jedno jsme neopravovali, aby čísla zůstala srovnatelná. Vliv odhadujeme na desetiny ms. **(neověřeno)**
- **Teplá cache.** Celá sada se vejde do RAM. Chování při datech větších než RAM (reálný uzel s tisíci tenanty) jsme neměřili. **(neověřeno)**
- **Síť pro PG.** Měřeno přes localhost. V datacentru přidá každý round-trip ~0,1–0,5 ms **(neověřeno)**. Pipelined příjem má 3 round-tripy na stránku, dotaz 1.
- **Disk.** Spotřebitelské NVMe bez PLP, btrfs nodatacow. Cena fsync na serverovém disku s PLP, na ext4/XFS nebo na síťovém blokovém úložišti (EBS) bude jiná. Rozdíl v outboxu pod souběhem se může zmenšit i zvětšit. **(neověřeno)**
- **Group commit pro SQLite** (accept log uzlu, dávkování) je jen návrh. Bez něj je SQLite u přijetí zápisu pod souběhem na hranici cíle (2,9–12,2 ms mezi běhy).
- **Odvoz WAL pro SQLite** (Litestream nebo vlastní) a jeho souhra s checkpointerem nebyla testována. Totéž platí pro dobu obnovy z odvezeného WAL. **(neověřeno)**
- **Velký tenant.** Hustý rozsah nesplní cíl na žádném backendu. Stránkování rozsahu je nutné navrhnout v API.
- **Mix dat je syntetický** (řady, kolekce, velikosti těl). Reálné účty z Musubi mohou mít delší popisy, přílohy a víc výjimek. Velikost i čas dotazů škálují s payloadem.
- **SQLite zapisovač po řádcích.** Dávkové vkládání (multi-row `INSERT`, `raw` jako tabulka s rowid) by mohlo srovnat sólo chvost s PG. **(neověřeno)**
- **PG bloat a autovacuum** po opakovaných upsertech (mix 1→2→3) nebyly sledovány.
- **Migrace přes 10 000 souborů** je lineární (~21 s). Na 100 000 tenantů by to bylo ~3,5 min sekvenčně. Rozdělení na více vláken jsme neměřili.

## 9. Závislosti a licence

| Crate | Licence |
|---|---|
| rusqlite 0.41 (bundled) | MIT |
| SQLite | public domain |
| tokio, tokio-postgres, futures-util, libc | MIT / Apache-2.0 |

Nic copyleftového. PostgreSQL běží jen v Docker image (PostgreSQL License, permisivní).
