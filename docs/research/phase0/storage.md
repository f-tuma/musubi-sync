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
  - 8 souběžných čtenářů: 6 484 dotazů/s proti 3 009 dotazům/s.
  - Pod souběžným zápisem: p99 4,2 ms proti **11,2 ms** (PG v tomto běhu cíl těsně nesplnil, v předchozím běhu měl 8,1 ms).
  - PG přitom běžel přes localhost, bez reálného síťového skoku.
- **Trvanlivé přijetí zápisu pod souběhem je u SQLite hraniční.**
  - Outbox s plným fsync, 16 tenantů naráz: SQLite p99 8,8 ms, PG 3,0 ms.
  - V předchozím běhu měl SQLite 12,2 ms (cíl nesplněn) a PG 7,8 ms. Recenzent naměřil u SQLite 9,98 ms. SQLite je tedy kolem hranice 10 ms a PG má díky group commitu rezervu.
  - Jeden zapisovatel: SQLite p99 1,8 ms, PG 8,1 ms (chvost PG je šum, předchozí běh 3,3 ms).
- **Velký tenant (100 000 položek) s hustým rozsahem cíl nesplní ani jedna varianta.**
  - Měsíc přes 20 kolekcí vrací 11 761 výskytů s payloadem: SQLite p99 29,5 ms, PG 66,2 ms.
  - Jde o tvar API, ne o volbu backendu. Řešením je stránkování nebo dotaz bez payloadu (ids-only: SQLite 8,4 ms, PG 9,9 ms).
- **Inkrementální sync** (stránka 25 položek včetně žurnálu a kurzoru, n = 3 000): SQLite p99 3,6 ms, PG 2,7 ms. Obě varianty jsou hluboko pod rozpočtem 20 ms.
- **Příjem dat** je u obou řádově desítky tisíc položek/s na tenanta a 95–219 tisíc položek/s souhrnně. Úzkým hrdlem bude poskytovatel.
- **WAL u SQLite neroste sám od sebe jen do rozumné velikosti.** Bez explicitního checkpointeru, který WAL resetuje (`TRUNCATE`), narostl v testu na 1,8–3,1 GB. S checkpointerem ze sekce 3.4 je maximum 51–151 MB. Cenou je chvost sólo příjmu velkého tenanta (p99 83–414 ms proti 17–102 ms u PG).
- **Velikost:** ~4,4 KB na položku bez komprese (SQLite) a ~3,9 KB (PG). Odhad 2 KB z návrhu platí jen se zstd v surové vrstvě: 2,3 KB po řádcích, 1,9 KB se slovníkem.

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
| 1 (výchozí) | 1 629 dotazů/s | 4,48 / 6,89 ms |
| 0 | 6 592 dotazů/s | 0,89 / 1,76 ms |

Rozdíl je **~4×** v propustnosti souběžného čtení. Dřívější údaj „8×“ pocházel ze smoke dat a neplatí. Jednovláknové dotazy se mezi oběma běhy liší jen šumem. V produkci stačí build s `SQLITE_DEFAULT_MEMSTATUS=0` (`.cargo/config.toml`, `LIBSQLITE3_FLAGS`). Bez toho by SQLite v testu souběžného čtení vyšel hůř než PG (1 629 proti 3 009 dotazům/s).

**2. Checkpoint WAL: nutná vlastní politika**

Režim WAL sám o sobě neudrží velikost WAL. `PASSIVE` checkpoint přepíše stránky do databáze, ale soubor WAL se resetuje až dalším zápisem ve chvíli, kdy je celý přenesený a žádný čtenář ho nedrží. Pod nepřetržitými čtenáři nebo zapisovatelem k tomu nedojde nikdy. První verze spiku (jen `PASSIVE` každých 50 ms, `wal_autocheckpoint=0`) proto nechala WAL růst bez omezení. Závěr „WAL se po checkpointu recykluje“ z předchozí verze zprávy byl chybný.

Nynější checkpointer (`src/sqlite.rs`, `Checkpointer`):

- `wal_autocheckpoint=65536` (256 MB) jen jako pojistka a `journal_size_limit=4 MB`, aby se WAL po resetu zkrátil.
- Vlákno na pozadí každých 50 ms změří velikost `-wal` a mezi 8 a 64 MB pustí `PASSIVE`. Pod 8 MB a nad 64 MB ho pouští nejvýš jednou za sekundu.
- Nad 64 MB resetuje WAL **sám zapisovač** po svém commitu: `PRAGMA wal_checkpoint(TRUNCATE)` s busy timeoutem 100 ms. Vlákno na pozadí by zámek zapisovače pod zátěží nedostalo (vývojový smoke běh, necommitovaný: 48 z 49 pokusů `SQLITE_BUSY`, WAL 270 MB).
- Když `TRUNCATE` nestihne čtenáře do 100 ms, vzdá to a zkusí to po dalším commitu. Počet neúspěchů se vypisuje („busy“).
- Spojení checkpointeru otevírá soubor až po jeho vytvoření a nastavení WAL. `busy_timeout` je první pragma, transakce zapisovače jsou `BEGIN IMMEDIATE`.

Strategie checkpointu, velký tenant sólo (`results/ab-ckpt-ingest.md`, `results/ab-ckpt-mixed.md`):

| Strategie | Stránka 250: p50 / p99 / max | položek/s | Stránka 2500 (n = 49): p99 | položek/s | Max WAL |
|---|---|---|---|---|---|
| `wal_autocheckpoint=1000` (výchozí, checkpoint v commitu) | 17,7 / 24,6 / 29,7 | 14 215 | 165,2 | 20 691 | ~4 MB, neměřeno **(neověřeno)** |
| Checkpointer (64 MB) | 8,3 / 82,7 / 91,8 | 19 687 | 215,1 | 23 777 | 82–119 MB |
| Checkpointer s limitem 256 MB | 8,3 / 73,6 / 118,7 | 24 793 | 184,1 | 27 689 | 281–303 MB |
| Jen `PASSIVE` (původní, neomezený) | 8,8 / 17,3 / 20,4 | 26 601 | 110,0 | 29 745 | **1 849 MB** |
| Mix 8 čtenářů + 8 zapisovatelů, jen `PASSIVE` | čtení p99 4,85 | – | – | – | **3 096 MB** |
| Mix 8 čtenářů + 8 zapisovatelů, checkpointer | čtení p99 4,56 | – | – | – | 71 MB |

Závěry:

- Rychlost původní verze (26,6 tisíc položek/s, p99 17 ms) byla daná tím, že zapisovač za checkpoint nikdy neplatil a WAL se nikdy neresetoval. Proti výchozímu `wal_autocheckpoint=1000` je to **1,9×** v propustnosti. Dřívější údaj „3–4×“ (6,4–9,6 tisíc položek/s, p99 až 463 ms) se v A/B nezopakoval. Proč první měření vyšlo hůř, jsme nerozebírali. **(neověřeno)**
- S omezeným WAL je propustnost 1,4× vyšší než u výchozího checkpointu. Chvost sólo příjmu je ale horší (p99 74–83 ms proti 25 ms), protože zapisovač jednou za ~64 MB zapíše WAL do databáze a zavolá fsync. Výchozí checkpoint platí totéž po menších dávkách v každém ~4 MB commitu.
- Max WAL překračuje limit 64 MB: jedna stránka 2500 přidá desítky MB a `TRUNCATE` nečeká na čtenáře déle než 100 ms. V hlavním běhu bylo maximum 151 MB (sólo stránka 2500) a 89 MB (mix).
- Pro latenci čtení na strategii skoro nezáleží (p99 4,6 proti 4,8 ms).
- Příjem nemá v kapitole 5.1 cíl. Inkrementální sync (p99 3,6 ms) a outbox se do limitu WAL dostanou zřídka. Volba prahu je kompromis mezi diskem a chvostem příjmu a patří do fáze 1.
- V řádcích „jen `PASSIVE`“ ukazují výsledkové soubory „0 passive“. V tomto režimu chyběl čítač. Po běhu je to opravené, na velikost WAL ani latence to vliv nemá.

Je to standardní produkční vzor, ale **je to náš kód**:

- musí omezit WAL (bez toho neomezený růst, viz výše),
- musí se vypořádat s tenanty, jejichž reset blokuje dlouhý čtenář (v mixu 315 z 569 pokusů o `TRUNCATE` narazilo na čtenáře; WAL se pak zkrátil až dalším pokusem),
- musí spolupracovat s odvozem WAL (sekce 7).

### 3.5 Reprodukce

```sh
cd spikes/storage-bench
./run.sh                 # celé měření (~40 min); výsledky v results/*.md; pak smaže kontejner i data
KEEP=1 ./run.sh          # ponechá kontejner musubi-storage-bench-pg a data/
SKIP_SQLITE=1 KEEP=1 ./run.sh   # jen PG část (SQLite výsledky zůstanou)
# rychlý smoke test:
TENANTS=20 EVENTS=500 LARGE=3000 CONC=4 COLD=200 QUERIES=50 MIXED_SECS=2 ./run.sh
```

Postup `run.sh`:

1. Spustí `cargo test --release` (determinismus generátoru).
2. Spustí `storage-bench sqlite load size`, pak `sqlite read mixed outbox coldopen`.
3. Spustí `storage-bench pg load size read mixed outbox connect`.
4. Změří zálohu a obnovu.
5. Změří velikost se zstd v surové vrstvě (`size_zstd.py`) a A/B přepínače (MEMSTATUS, strategie checkpointu).

Surové výsledky: `results/sqlite.md`, `results/pg.md`, `results/backup.md`, `results/size-zstd.md`, `results/ab-*.md`, `results/env.md` a celý log `results/run.log`.

`run.sh` odmítne neprázdný `BENCH_DATA` bez vlastní značky `.storage-bench-data`, odstraní zastavený kontejner se stejným jménem a při ukončení (i chybou) uklidí kontejner a data, pokud není `KEEP=1`. Release build má `panic = "abort"`: pád kteréhokoli vlákna, včetně checkpointeru, ukončí celý běh.

### 3.6 Prostředí

| Položka | Hodnota |
|---|---|
| CPU | Intel Core Ultra 7 265K (20 jader, max 6,5 GHz) |
| RAM | 62 GB, z toho ~23 GB obsazeno jinými procesy |
| Disk | NVMe Kingston KC3000 2 TB, btrfs (`compress=zstd:1`, `ssd`, `discard=async`); datový adresář `chattr +C` |
| Software | Linux 7.3.3-cachyos, Docker se 14 dalšími běžícími kontejnery (šum), Rust/cargo 1.99.0 |

Celá sada se vejde do RAM. Čtení proto probíhá s teplou page cache, kromě testu studeného otevírání. PG běží přes TCP na localhostu, takže reálné nasazení přidá síťový RTT na každý round-trip.

## 4. Výsledky

Latence jsou v ms. Výsledky pocházejí z jednoho běhu (`results/run.log`). Tučně je hodnota, která nesplní cíl 10 ms tam, kde se cíl vztahuje. U řádků s n < 100 je p99 rovno maximu, ne stabilnímu percentilu.

### 4.1 Příjem (ingest)

Latence je na jednu transakci, tedy na stránku včetně položek, surových těl, výskytů, entit, žurnálu a kurzoru kolekce. U SQLite běží checkpointer ze sekce 3.4 a jeho cena je v číslech zahrnutá.

| Test | SQLite p50 / p99 / max | SQLite položek/s | PG p50 / p99 / max | PG položek/s |
|---|---|---|---|---|
| Velký tenant sólo, stránka 250 (n = 411) | 8,3 / 135,2 / 205,9 | 16 006 | 10,0 / 17,1 / 30,7 | 23 459 |
| Velký tenant sólo, stránka 2500 (**n = 49**) | 74,7 / 414,1 / 414,1 | 21 303 | 63,4 / 102,1 / 102,1 | 35 415 |
| 100 tenantů, 16 zapisovatelů, stránka 250 | 12,0 / 27,7 / 65,4 | 189 277 souhrnně | 11,9 / 36,7 / 54,3 | 206 042 souhrnně |
| 100 tenantů, 16 zapisovatelů, stránka 2500 | 12,8 / 46,2 / 51,0 | **218 746** souhrnně | 11,0 / 334,8 / 788,8 | 95 422 souhrnně |
| Běžní tenanti sólo, stránka 250 | 3,3 / 8,6 / 8,9 | 44 021 | 4,2 / 7,3 / 7,7 | 39 356 |
| Běžní tenanti sólo, stránka 2500 | 3,4 / 16,7 / 17,0 | 48 869 | 3,9 / 37,5 / 49,2 | 36 000 |
| **Inkrementální sync**, stránka 25 (jen transakce, n = 3 000) | 1,07 / **3,60** / 42,1 | – | 1,23 / **2,68** / 12,1 | – |

Poznámky:

- U běžných tenantů stránka 2500 v praxi znamená celou kolekci (největší má ~1 400 položek), protože stránky se dělí po kolekcích.
- Sólo velký tenant: PG je o 47–66 % rychlejší a má o řád užší chvost. Chvost SQLite (p99 135 ms) tvoří reset WAL v zapisovači každých ~64 MB (sekce 3.4). V A/B běhu se stejným nastavením vyšlo p99 83 ms a 19 687 položek/s, takže mezi běhy je velký šum. Dávkový `unnest` navíc dělá méně práce na řádek než SQLite po řádcích.
- Sólo běžní tenanti: SQLite je rychlejší (44–49 tisíc proti 36–39 tisícům položek/s). WAL u nich nedosáhne 64 MB (max 60 MB), reset tedy neproběhne.
- Souběh 16 zapisovatelů: u stránky 250 jsou varianty srovnatelné, u stránky 2500 má SQLite 2,3× vyšší propustnost a 7× nižší p99. Soubory jsou nezávislé a nesdílí žádný zámek.
- Chvost PG pod souběhem (p99 335 ms u stránky 2500, předchozí běh 174 ms) pochází ze sdíleného WAL a checkpointů. Dál jsme ho nerozebírali.
- Inkrementální sync: max SQLite 42 ms je ojedinělá špička. `TRUNCATE` při něm neproběhl (WAL max 6,7 MB), příčinu jsme nerozebírali. p99 je v rozpočtu.
- Na poskytovatelích to nezáleží: Google i Graph vracejí řádově stovky položek na požadavek za stovky ms.

### 4.2 Dotaz na rozsah

Měří se 2 000 dotazů s náhodným tenantem a náhodným oknem. Běžní tenanti jsou 0..199, velký tenant je `t100001`.

| Test | Průměr řádků | SQLite p50 / p99 / max | PG p50 / p99 / max |
|---|---|---|---|
| Běžný, týden, 5 kolekcí | 35 | 0,13 / 0,38 / 0,46 | 0,26 / 0,69 / 0,95 |
| Běžný, týden, 20 kolekcí | 140 | 0,35 / 0,47 / 0,57 | 0,88 / 1,53 / 2,09 |
| Běžný, měsíc, 5 kolekcí | 150 | 0,21 / 0,53 / 0,64 | 0,58 / 1,56 / 2,36 |
| Běžný, měsíc, 20 kolekcí | 591 | 0,82 / 1,00 / 1,11 | 2,43 / 3,39 / 8,53 |
| Běžný, měsíc, 20 kolekcí, ids-only | 590 | 0,33 / 0,39 / 0,43 | 0,51 / 0,78 / 0,97 |
| Velký, týden, 5 kolekcí | 705 | 1,20 / 3,36 / 4,17 | 2,63 / 7,03 / 8,51 |
| Velký, týden, 20 kolekcí | 2 783 | 5,02 / 8,33 / 9,68 | 15,91 / **40,24** / 83,96 |
| Velký, měsíc, 5 kolekcí | 2 920 | 3,91 / **12,41** / 14,85 | 10,06 / **35,29** / 79,61 |
| Velký, měsíc, 20 kolekcí | 11 761 | 25,08 / **29,51** / 30,78 | 52,83 / **66,17** / 182,58 |
| Velký, měsíc, 20 kolekcí, ids-only | 11 759 | 6,43 / 8,35 / 8,93 | 7,69 / 9,87 / 11,99 |

Poznámky:

- Čas škáluje s počtem vrácených řádků, a hlavně s payloadem: 11 761 řádků po ~1 KB je ~12 MB na jeden dotaz.
- SQLite je 2–4× rychlejší, protože čte v procesu bez serializace přes drát.
- Cíl p99 < 10 ms SQLite drží do ~2 800 výskytů s payloadem a i bez payloadu při 11 760 výskytech. PG drží do ~700 výskytů s payloadem a bez payloadu je při 11 760 výskytech těsně pod cílem (9,9 ms, předchozí běh 10,6 ms).
- Férovost: SQLite skládá `IN (…)` s proměnným počtem kolekcí do textu dotazu, takže různé seznamy jsou různé příkazy v cache připravených dotazů (kapacita 64) a část dotazů se znovu připravuje. PG posílá seznam jako pole. SQLite je tím spíš znevýhodněn, rozsah jsme neměřili. **(neověřeno)**
- Měsíc přes 20 kalendářů se ~12 tisíci výskyty je extrém. Běžný uživatel má pro stejný dotaz ~590 výskytů.
- Řešení nezávislé na backendu: rozsah stránkovat (např. limit 2 000 výskytů) nebo vracet ids + časy a payload dotáhnout zvlášť pro viditelné položky.

### 4.3 Změny od kurzoru (limit 500)

| Test | Průměr řádků | SQLite p50 / p99 / max | PG p50 / p99 / max |
|---|---|---|---|
| Běžný, od head − 100 | 60 | 0,07 / 0,11 / 0,26 | 0,35 / 0,63 / 1,03 |
| Běžný, od náhodného kurzoru | 456 | 0,40 / 0,45 / 0,50 | 2,13 / 2,49 / 2,74 |
| Velký, od head − 100 | 100 | 0,04 / 0,05 / 0,17 | 0,39 / 0,64 / 1,08 |
| Velký, od náhodného kurzoru | 498 | 0,44 / 0,52 / 0,56 | 2,10 / 2,43 / 6,77 |

Obě varianty cíl splňují s velkou rezervou, SQLite je 4–10× rychlejší. Širší chvost SQLite u velkého tenanta z předchozího běhu (p99 5,5 ms) se nezopakoval (0,52 ms), byl to tedy šum.

### 4.4 Souběžné čtení a čtení pod zátěží zápisu

| Test | SQLite p50 / p99 / max | PG p50 / p99 / max |
|---|---|---|
| 8 čtenářů, běžný měsíc / 20 kolekcí | 0,90 / 1,97 / 9,59 (6 484 dotazů/s) | 2,60 / 3,56 / 5,44 (3 009 dotazů/s) |
| Mix čtení (měsíc + změny), bez zapisovatelů | 0,82 / 1,08 / 5,91 | 2,52 / 3,50 / 6,91 |
| Mix čtení se **8 zapisovateli** na stejných 8 tenantech | 1,78 / **4,23** / 10,50 | 4,22 / **11,23** / 276,50 |
| Zápis během mixu (update stránek 250) | 2,93 / 24,76 / 680,30; **297 295 položek/s** | 7,77 / 33,64 / 282,41; 155 508 položek/s |

Test je záměrně nepříznivý: zapisovatelé přepisují celé tenanty (verze 1→2→3) ve stejných souborech nebo řádcích, které se čtou.

- SQLite drží čtení pod zápisem na p99 4,2 ms (A/B běh 4,6 ms). Max WAL byl 89 MB, 315 z 569 pokusů o `TRUNCATE` narazilo na čtenáře.
- PG v tomto běhu cíl nesplnil (p99 11,2 ms, max 276 ms). V předchozím běhu měl 8,1 ms. Je těsně u hranice a závisí na běhu.
- SQLite při tom zapsal 1,9× víc dat. Cenou je chvost zápisu: max 680 ms. Pravděpodobně jde o reset WAL, který čeká na čtenáře a pak zapíše a fsyncne 64+ MB. **(neověřeno)** V A/B běhu se stejným nastavením byl max 191 ms.
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
| 1 zapisovatel, 2 000 mutací | 1,03 / 1,84 / 11,08 | 1,16 / 8,09 / 14,42 |
| 16 tenantů současně, 4 800 mutací | 2,71 / 8,83 / 12,55 | 2,13 / 2,96 / 11,82 |

SQLite v tomto běhu cíl splnil, ale je hraniční:

| Běh | SQLite p99, 16 tenantů | PG p99, 16 tenantů |
|---|---|---|
| Původní zpráva | **12,18** | 7,79 |
| Recenzent | 9,98 | – |
| Tento běh | 8,83 | 2,96 |

Rozptyl mezi běhy (8,8–12,2 ms) je větší než rezerva do cíle. Pro rozhodnutí je proto SQLite potřeba brát jako „kolem 10 ms“.

Příčina:

- 16 souborů znamená 16 nezávislých fsync WAL.
- PG sloučí souběžné commity do jednoho fsync (group commit).
- Při stovkách aktivních tenantů na uzlu se rozdíl pravděpodobně zvětší. **(neověřeno)**
- Měřeno jen na btrfs nodatacow a spotřebitelském NVMe. ext4 ani XFS nebyly k dispozici (bez root práv nejde vytvořit loop svazek). Cena fsync tam může být jiná oběma směry. **(neověřeno)**

Možné cesty pro SQLite (žádná nebyla měřena):

- Krátké dávkování přijetí v rámci jednoho tenanta.
- Sdílený „accept log“ uzlu s group commitem: zápis do jednoho souboru s fsync, do tenantového souboru až potom.
- Disk s PLP (power-loss protection), kde je fsync levný.

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
| běžný | zstd se slovníkem (32 KB, trénovaný na 2 000 tělech tenanta) | 1,0 MB | 5,5 MB | 9,3 MB | 1 869 B |
| velký (100 000) | text | 93,4 MB | 263,8 MB | 412,5 MB | 4 125 B |
| velký | zstd po řádcích | 56,3 MB | 99,6 MB | 230,5 MB | 2 305 B |
| velký | zstd se slovníkem | 17,7 MB | 99,2 MB | 187,1 MB | 1 871 B |

Závěry:

- **Odhad 2 KB z návrhu platí jen s kompresí surové vrstvy:** ~2,3 KB po řádcích, ~1,9 KB se slovníkem. Bez komprese je to ~4,4 KB, tedy ~2,2 TB pro 100 000 × 5 000 místo ~1 TB.
- **Většina úspory není z komprese, ale z přetečení stránek.** Tabulka `raw` je `WITHOUT ROWID` (B-strom indexu). Těla nad ~1 000 B se tam nevejdou na stránku a každé zabere vlastní přetokovou stránku 4 KB. 5,1 MB textu proto zabírá 14,0 MB. Po zstd se těla vejdou a tabulka klesne na 5,5 MB, i když se data zmenšila jen na 3,1 MB. Tabulka `raw` s rowid nebo větší stránka by část úspory dala i bez komprese. **(neověřeno)**
- Se slovníkem jsou těla ~3× menší než se zstd po řádcích, soubor je ale menší jen o ~20 %. Zbytek tvoří kanonická data, výskyty a indexy. Slovník (32 KB na tenanta) v souboru není započtený.
- PG `raw` (1 494 MB) TOAST nekomprimuje, protože těla jsou pod prahem ~2 KB. Komprese by tam musela být v aplikaci stejně jako u SQLite.
- Celá záloha `tar | zstd -1` má 827 MB (6,7×), ale to je komprese mimo databázi, ne velikost na disku.

### 4.7 SQLite: otevírání souborů, LRU a migrace

Data: 10 000 malých tenantů (50 položek, 2 705 MB). Vytvoření trvalo 16,6 s.

| Test | p50 | p99 | max | Pozn. |
|---|---|---|---|---|
| Otevření + pragmy + první dotaz, **studená** page cache (`fadvise DONTNEED`) | 0,53 | 0,86 | 5,91 | 10 000 souborů za 6,3 s |
| Zavření (studená) | 0,05 | 0,10 | 0,19 | |
| Otevření + první dotaz, teplá cache | 0,14 | 0,21 | 0,45 | 10 000 souborů za 1,8 s |
| Zavření (teplá) | 0,04 | 0,06 | 0,33 | |
| Požadavek přes LRU 1 000 otevřených z 10 000 (Zipf s = 1,1) | 0,012 | 0,30 | 0,97 | hit rate 77,8 %, RSS +163 MB při 1 000 otevřených |
| Migrace `ADD COLUMN` při otevření | 2,07 | 11,18 | 98,36 | 10 000 souborů ≈ 21 s sekvenčně |
| *Pro srovnání PG: nové spojení + prepare + první dotaz* (n = 200) | 3,87 | 5,71 | 5,88 | to, co ušetří pool |

Otevření databáze na miss LRU je levné: < 1 ms i se studenou cache. LRU s ~1 000 otevřenými soubory stojí ~163 KB RSS na soubor.

Líná migrace při otevření stojí p99 ~11 ms a max 98 ms na první požadavek po nasazení. Je to přijatelné, ale tuto latenci uvidí první požadavek každého tenanta. Alternativou je migrovat na pozadí po nasazení.

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
| Rozsah p99 < 10 ms, velký tenant s payloadem | ✅ do ~2 800 výskytů; ❌ 12,4–29,5 ms nad 2 900 výskytů | ✅ do ~700 výskytů; ❌ 35,3–66,2 ms nad 2 700 výskytů |
| Rozsah ids-only, 11 760 výskytů | ✅ 8,4 ms | ⚠️ 9,9 ms (předchozí běh 10,6 ms) |
| Změny od kurzoru p99 < 10 ms | ✅ ≤ 0,52 ms | ✅ ≤ 2,5 ms |
| Čtení p99 < 10 ms pod zátěží zápisu | ✅ 4,2 ms | ⚠️ 11,2 ms (předchozí běh 8,1 ms) |
| Přijetí zápisu p99 < 10 ms, 1 zapisovatel | ✅ 1,8 ms | ✅ 8,1 ms (předchozí běh 3,3 ms) |
| Přijetí zápisu p99 < 10 ms, 16 tenantů současně | ⚠️ 8,8 ms (předchozí běhy 12,2 a 9,98 ms) | ✅ 3,0 ms |
| Režie inkrementálního syncu < 20 ms | ✅ p99 3,6 ms | ✅ p99 2,7 ms (+ síť) |
| Velikost WAL omezená | ✅ max 151 MB, jen s vlastním checkpointerem (bez něj 1,8–3,1 GB) | – (sdílený WAL, `max_wal_size`) |

⚠️ = výsledek kolísá kolem hranice cíle mezi běhy.

## 6. Verdikt a doporučení

**Čísla nevedou k PostgreSQL.**

- Na hlavní čtecí cestě je SQLite 2–4× rychlejší.
- Pod souběžným zápisem čte s nižším chvostem (p99 4,2 ms proti 11,2 ms).
- Pod souběhem tenantů s velkými stránkami má vyšší propustnost příjmu.
- Zálohuje a obnovuje rychleji.

PG je výkonově lepší ve třech bodech:

- **trvanlivé přijetí zápisu pod souběhem**: díky group commitu má stabilní rezervu (p99 3,0–7,8 ms). SQLite kolísá kolem cíle (8,8–12,2 ms).
- **sólo příjem velkého tenanta**: 1,5–1,7× vyšší propustnost a o řád užší chvost (p99 17 ms proti 135 ms). Chvost SQLite je cena za omezený WAL. Kapitola 5.1 pro příjem cíl nemá, poskytovatel je pomalejší.
- **WAL bez vlastního kódu**: SQLite potřebuje explicitní checkpointer. Bez něj WAL roste na jednotky GB, s ním zůstává do ~150 MB za cenu výše uvedeného chvostu.

Ani jeden bod nemění odhad z kapitoly 4.3: výkon nerozhoduje, rozhoduje provoz. Body 1 a 3 jsou ale riziko na straně SQLite, které musí fáze 1 vyřešit, ne jen zdokumentovat.

**Doporučení:**

1. **Fáze 1 staví datovou rovinu na SQLite na tenanta** (WAL, jeden zapisovatel na tenanta, LRU otevřených souborů). Platí pro self-host i pro první hostovanou instanci bez smluvního SLA.
2. **Podmínky pro SQLite** (bez nich vychází hůř než PG):
   - build s `SQLITE_DEFAULT_MEMSTATUS=0` (4× propustnost souběžného čtení);
   - `busy_timeout` jako první pragma a zapisovací transakce `BEGIN IMMEDIATE`;
   - vlastní checkpointer: `PASSIVE` na pozadí, `TRUNCATE` v zapisovači nad prahem (zde 64 MB) s omezeným čekáním na čtenáře, `journal_size_limit` a vysoký `wal_autocheckpoint` jako pojistka. Práh nastavit podle priority disku proti chvostu příjmu (sekce 3.4). Monitorovat max WAL a počet neúspěšných resetů na tenanta;
   - zápis stránky jako jedna transakce s `synchronous=NORMAL` a mutace s `FULL`;
   - čtecí a zapisovací spojení zvlášť.
3. **Vyřešit přijetí zápisu pod souběhem dřív, než se to stane problémem:**
   - změřit dávkování nebo sdílený accept log s group commitem ve fázi 1;
   - do té doby počítat s p99 ~9–12 ms při 16 souběžných tenantech na uzel na tomto HW, tedy na hranici cíle;
   - změřit na ext4/XFS a na serverovém disku s PLP.
4. **API rozsahu stránkovat** (limit výskytů, kurzor) a nabídnout variantu bez payloadu. Bez toho cíl 10 ms nesplní žádný backend u hustých kalendářů.
5. **PostgreSQL zůstává druhým backendem za úzkým rozhraním úložiště.** Implementovat ho, až padne rozhodnutí o hostované službě se závazkem dostupnosti. Spike ukazuje, že port je přímočarý (stejné schéma s `tenant_id`; pipelining a `unnest`). Provozní výhody PG (synchronní replika, PITR, spravovaná služba) jsou pak hlavní argument, ne výkon.
6. **Rozhodnout o kompresi surové vrstvy a podle toho upravit odhad velikosti** v kapitole 4.3. Odhad 2 KB na událost platí jen se zstd v `raw` (2,3 KB po řádcích, 1,9 KB se slovníkem na tenanta). Bez komprese je to ~4,4 KB, tedy ~2,2 TB pro 100 000 × 5 000. V každém případě nepoužívat `WITHOUT ROWID` pro těla kolem 1 KB (přetokové stránky, sekce 4.6).

## 7. Provozní trade-offy

| | SQLite na tenanta | PostgreSQL |
|---|---|---|
| Záloha | soubor na tenanta; snímek přes `VACUUM INTO` / backup API; celý uzel 2–3 s na 5,5 GB (měřeno v klidu) | `pg_dump` 5,7 s; PITR přes `pg_basebackup` + archiv WAL (standardní, neměřeno) |
| Průběžný odvoz WAL | Litestream-like proces nebo vlastní kód, musí spolupracovat s naším checkpointerem **(neověřeno)**; na 10 000 souborů to je 10 000 proudů | jeden proud WAL (`archive_command`, pgBackRest, WAL-G) |
| HA | žádná; obnova tenanta na jiný uzel = stažení souboru (+ přehrání WAL); tenant je vázán na uzel, potřeba směrování | synchronní replika, failover (Patroni, spravované služby) |
| Migrace schématu | N souborů; líně při otevření (p99 ~11 ms na první požadavek) nebo na pozadí; ~21 s sekvenčně na 10 000 tenantů | jedna transakce; `ADD COLUMN` bez defaultu je okamžitý, velké `ALTER` zamykají sdílenou tabulku |
| Izolace tenantů | soubor = izolace; export / smazání = práce se souborem | hluční sousedé ve sdíleném WAL a buffer poolu (chvost příjmu p99 174–335 ms pod souběhem); RLS |
| Bloat a WAL | WAL se sám neresetuje, dokud ho drží čtenáři nebo nepřetržitý zapisovatel (měřeno: 1,8–3,1 GB jen s `PASSIVE`). Omezí ho jen vlastní politika s `TRUNCATE` (měřeno: max 51–151 MB); `VACUUM` per soubor podle potřeby | MVCC bloat po masivních upsertech, autovacuum ladění (v testu nesledováno) |
| Spojení | otevření < 1 ms, žádný pool | nové spojení ~3,9 ms, nutný pool (PgBouncer / deadpool) |
| Self-host | jeden binární soubor | další služba k provozu a aktualizaci |

## 8. Otevřená rizika a co spike neměřil

- **Šum a jediný běh.** Na stroji běželo 14 dalších kontejnerů a ~23 GB RAM bylo obsazeno. Čísla jsou z jednoho běhu, bez opakování a intervalů spolehlivosti. Chvosty (p99, max) jsou nejméně stabilní část. Srovnání s předchozím během a s A/B během ukazuje, jak velký šum je: outbox SQLite 8,8 proti 12,2 ms, čtení PG pod zápisem 11,2 proti 8,1 ms, sólo příjem velkého tenanta SQLite p99 135 proti 83 ms. Řádky s n < 100 (sólo stránka 2500, n = 49) jsou označené a jejich p99 je maximum.
- **ext4/XFS neměřeno.** Bez root práv nešlo vytvořit jiný souborový systém. Vše běželo na btrfs nodatacow. Rozdíl v ceně fsync ovlivní hlavně outbox pod souběhem a reset WAL. **(neověřeno)**
- **Checkpointer pod mnoha čtenáři.** V mixu 315 z 569 pokusů o `TRUNCATE` narazilo na čtenáře a WAL se zkrátil až dalším pokusem. Max zápisu v mixu byl 680 ms (A/B běh 191 ms). Při dlouhých čtecích transakcích (export, záloha přes backup API) může WAL růst dál nad práh. Chování při stovkách tenantů na uzlu a čtenářích delších než 100 ms jsme neměřili. **(neověřeno)**
- **Férovost dotazů.** PG platí v mixu round-trip navíc za `head_seq` (sekce 4.4). SQLite skládá `IN (…)` do textu dotazu, takže cache připravených dotazů částečně netrefuje (sekce 4.2). Ani jedno jsme neopravovali, aby čísla zůstala srovnatelná. Vliv odhadujeme na desetiny ms. **(neověřeno)**
- **Teplá cache.** Celá sada se vejde do RAM. Chování při datech větších než RAM (reálný uzel s tisíci tenanty) jsme neměřili. **(neověřeno)**
- **Síť pro PG.** Měřeno přes localhost. V datacentru přidá každý round-trip ~0,1–0,5 ms **(neověřeno)**. Pipelined příjem má 3 round-tripy na stránku, dotaz 1.
- **Disk.** Spotřebitelské NVMe bez PLP, btrfs nodatacow. Cena fsync na serverovém disku s PLP, na ext4/XFS nebo na síťovém blokovém úložišti (EBS) bude jiná. Rozdíl v outboxu pod souběhem se může zmenšit i zvětšit. **(neověřeno)**
- **Group commit pro SQLite** (accept log uzlu, dávkování) je jen návrh. Bez něj je SQLite u přijetí zápisu pod souběhem na hranici cíle (8,8–12,2 ms mezi běhy).
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
