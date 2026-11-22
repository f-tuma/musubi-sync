# Fáze 2: živá kontrola K10 přes `musubi-syncd`

## 1. Otázka

Plán, kapitola 9, K10: projde celý `musubi-syncd` proti skutečnému Googlu (Tasks, People) a iCloudu? Jak rychle se změna u poskytovatele objeví v deníku změn při aktivní a běžné úrovni dotazování?

## 2. Metoda

- 2026-11-22, `musubi-syncd` z `main` (`d567035`), samostatná instance s prázdným datovým adresářem, veřejně přes Tailscale Funnel (návrat OAuth).
- Google: testovací účet `musubi.test.01`, připojený přes `POST /v1/accounts` a souhlas v prohlížeči. iCloud: Apple ID uživatele s heslem pro aplikace, přes `POST /v1/accounts/dav` s profilem `icloud`. Uživatel povolil zápis (T11 byla jen pro čtení).
- Změny dělal skript přímo u poskytovatele: Tasks API (seznam `musubi-t10`), People API (`createContact`/`deleteContact`), CalDAV `PUT`/`DELETE` v kalendáři „Domácí“ a CardDAV v adresáři „card“. Vždy vytvoření a za 20–90 s smazání, v náhodnou chvíli intervalu.
- Měří se čas od odpovědi poskytovatele po první nový záznam v `/v1/changes` (dotaz každých 250 ms). SSE přidává nejvýš 150 ms slučování (B5), proto se neměřilo zvlášť.
- Aktivní úroveň: skript posílal `POST /v1/interest` nejvýš jednou za 60 s, jako klient s otevřenými daty (kapitola 7.3). Běžná úroveň: bez něj.

## 3. První data

Všechny kolekce byly načtené do 2 s od souhlasu (Google: Kontakty, 2 kalendáře, 2 seznamy úkolů) a do 8 s od připojení iCloudu (adresář, 2 kalendáře), bez chyb, s úplným pokrytím.

## 4. Zpoždění změny

Sekundy od změny u poskytovatele do záznamu v deníku. Vzorků je málo, proto bez percentilů.

| poskytovatel a druh, úroveň | vzorků | min | medián | max |
|---|---|---|---|---|
| Google úkoly, aktivní | 8 | 2,8 | 13,3 | 62,3 |
| Google kontakty, aktivní | 8 | 0,6 | 10,0 | 57,6 |
| iCloud události, aktivní | 8 | 1,3 | 13,5 | 51,0 |
| iCloud vizitky, aktivní | 8 | 1,3 | 18,3 | 54,0 |
| Google úkoly, běžná | 10 | 24,9 | 61,6 | 103,9 |
| Google kontakty, běžná | 6 | 40,0 | 75,8 | 94,2 |
| iCloud události, běžná | 6 | 51,4 | 118,2 | 487,0 |
| iCloud vizitky, běžná | 6 | 39,2 | 107,0 | 213,0 |

- Aktivní úroveň (15–60 s) drží očekávání „interval úrovně plus jedna delta“: maximum 62,3 s. Odpovídá K9 proti falešnému serveru (p50 22 s, max 47 s).
- Běžná úroveň (2–15 min) zůstala v mezích. Čtyři vzorky kontaktů Google v běžné úrovni chybí: skript čekal jen 300 s a kontakt vznikl i zmizel mezi dvěma dotazy (dotazy v 9:46:59, 9:54:53 a 10:10:22). Deník správně nezapsal nic. Chyba měření, ne serveru.
- Zpoždění propsání u People se od zpoždění dotazu oddělit nedá; minimum 0,6 s ukazuje, že je pod sekundou, ve shodě s T10 (0,1–0,2 s).
- iCloud přijal `PUT` s `If-None-Match: *` (vytvoření) i `DELETE` bez podmínky. `PUT` s `If-Match` (vdirsyncer #1145) zůstává neověřený, protože syncd do poskytovatele zatím nezapisuje.

## 5. Zbývá (nejdřív 2026-11-24)

- Delta People s tokenem A a s posledním tokenem řetězu B (kapitola 6.3) a tvar chyby vypršeného tokenu (otevřená otázka 3).
- Vypršení kanálu push kalendáře Google 24. 11.
- Kvóta Tasks API (otevřená otázka 7).

## 6. Dopad

Žádná odchylka neměnila kontrakt; K10 zatím nenašla nic, co by blokovalo K11. Meze dotazování zůstávají. D4 může u People nahradit odhad „několik minut“ naměřeným: propsání pod sekundu.
