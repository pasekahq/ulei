# Runbook: a places list into the places bed

The owner's secretary agent runs it on the owner's host when the owner hands over a list of
places: it converts the list into the format `ulei-garden places import` accepts (ADR-0080
section 4) and imports it through the running garden daemon. The list and the converted file
stay on the host. Examples below are invented: the town of Fennwick and its places do not exist.

## Converting a free-form places list for import

The importer reads only its own strict format. It refuses a whole file for one bad line, so
convert the list once, by hand or with your own tools, into a new file and keep the original as
it was.

### The format in short

```markdown
# Places of Fennwick
as_of: 2031-03-20

## Profile
- min_rating: 4.2 [owner, 2031-02-01]
- ban: celery [owner, 2031-02-01]

## Active
### Lantern Noodle Bar
- cuisine: noodles, dumplings [fixture-listing, 2031-02-08]
- rating: 4.5 [fixture-maps, 2031-03-08]
- verdict: +1 broth was excellent [chat:msg-9001, 2031-02-28]

## Candidates
### Tidepool Kitchen
- address: Harbour Row 9 [fixture-delivery-app, 2031-03-18]

## Gone
### Old Pier Diner
- operating_status: permanently_closed [fixture-osm, 2031-01-20]
```

- `as_of` is the date of the list: the day its newest value was written down, not the day of the
  conversion. No field may carry a later date.
- The sections are `## Profile`, `## Active` (on the owner's list), `## Candidates` (found, not
  yet confirmed by the owner) and `## Gone` (closed for good). Every place is one `### <name>`
  heading under one of the last three; a name appears once in the file.
- Every field is one line `- <key>: <value> [<source>, <date>]`. The keys of a place are
  `rating`, `review_count`, `price_for_two`, `hours`, `operating_status`, `cuisine`, `location`,
  `address`, `osm_id`, `maps_url`, `vibe`, `warnings`, `notes` and `verdict`; the keys of the
  profile are `home`, `min_rating`, `budget_for_two`, `currency` and `ban`. The value forms are
  in ADR-0080 section 2: `4.5`, `412`, `30-40`, `Mo-Sa 12:00-22:00; Su off`, `open`,
  `0.5041, -25.0032`, `node/9000001`, an `https://` link, comma-separated terms or free text.
- A date is `YYYY-MM-DD`, or as precise as the list knows it: `17.03` (the latest such day on
  or before `as_of`), `2031-03` or `03.2031` (the first day of the month). Never make a date
  more precise than the list: a value must not look fresher than it is.
- `n/a` marks a value nobody has confirmed: `- rating: n/a`. It writes nothing and is counted
  as unconfirmed.
- A verdict is `1`, `+1`, `0` or `-1`, then the owner's words if any; its source is the message
  that carries it (`chat:<message id>`) and its date the day the owner said it.

### Shapes of a free-form list and their lines

Two attributes in one line. `noodles and dumplings, 4.5 on Fixture Maps (March 8)` holds a
cuisine and a rating. Write one field per key, each with the source and the date that belong to
it; a part that names no source of its own takes the source of the line it came from:

```markdown
- cuisine: noodles, dumplings [fixture-listing, 2031-02-08]
- rating: 4.5 [fixture-maps, 2031-03-08]
```

Several sources on one value. When they checked it on different days, write one line per source
and date: the latest is the current value and the others go into the value's history. When they
checked it on the same day, write one line and join the sources with ` + `:
`- hours: Mo-Su 10:00-23:00 [fixture-osm + fixture-listing, 2031-03-14]`. Two lines of one key
and one date are refused.

A date without a source. A source is never invented. Name where the value came from: the
provider the list names, `owner` for the owner's own words, or your own research record, such
as `research:2031-03-19-fennwick` for a lookup you made and logged. When nobody can say where it
came from, write the key as `n/a` and say so in your report; the value is checked again later.
A value with a source but no date takes a date the list itself gives for it, such as the date
heading of the part it stands in, never `as_of` or the day of the conversion; with none, it is
`n/a` as well.

Link values. A map link goes into `maps_url` as written, `https://` only, with its query string
and a long numeric place id. Other links (a menu, a booking page) have no key: put them into
`notes` with their source. Drop a query parameter that carries a key or a token
(`key=`, `token=`, a long random string): the importer refuses a value that may hold a
credential, and the link works without it.

Prose entries. `Lovely courtyard, go before 14:00 for the plov; loud on Fridays` is split by
meaning: the atmosphere into `vibe` (`courtyard, lively`), cautions into `warnings`
(`loud on fridays`), and the rest into `notes` as written, each with the prose's source and date.
Keep the owner's own words for the owner's verdict note.

Another mark of an unconfirmed value. `?`, `unknown`, `tbd`, `-` or an empty value all become
`n/a`.

### What the converter adds

- `as_of`, the date of the list.
- For a place that exists on OpenStreetMap: its `osm_id` and its `location`, with the source
  you took them from and the day you looked them up, such as `[osm, 2031-03-19]`. The bed
  rechecks a place by its `osm_id`, so add it wherever one exists; a place without one keeps its
  stale values until someone writes new ones.
- Nothing else. A value you looked up yourself carries your research record as its source and
  the day of the lookup.

### Ratings of several providers

`rating` and `review_count` take the figures of the one provider the owner's `min_rating` was set
on: the threshold compares only figures of one scale. Write that provider's rating and count with
its name as the source. Every other provider's figures go into `notes` with their own source
(`- notes: Fixture Guide 4.1 of 5, 230 reviews [fixture-guide, 2031-03-02]`). Anything else
without a typed key goes into `notes`; a caution (an allergen, a long queue, cash only) goes into
`warnings`.

### List values and the search filters

`cuisine`, `vibe`, `warnings` and the profile's `ban` are compared term by term: a value is split
at commas, and each term is compared after case folding, collapsing whitespace and dropping
trailing punctuation. A `cuisine` filter matches a whole term only, so `noodles` does not match
`noodle` or `noodle soup`: use one plain spelling of each term across the file, the word the
owner asks with. A `ban` term drops a place whose `cuisine` holds it and marks a place whose
`warnings` hold it, so write an allergen as its own term (`warnings: celery, cash only`), never
inside a sentence. `vibe` words are compared by their stems, so word forms there may differ.

### The order

1. Convert the whole list into a new file.
2. Run the dry run until it prints no error:

   ```sh
   ulei-garden places import <FILE> --dry-run
   ```

   An error is one JSON object `{"error":"refused","lines":[...]}` that names every refused line
   with its number, its `rule` and a `message`; fix them all, then run it again.
3. Compare the dry run's counts with the original list: `places` by section, `attrs` (one per
   field line), `unconfirmed` (the `n/a` lines), `verdicts` and `profile`. A count that differs
   from what you see in the list is a place or a value lost or doubled in the conversion.
4. Import:

   ```sh
   ulei-garden places import <FILE> [--config <ulei.toml>]
   ```

   The report counts the places `created`, `adopted` (a place the garden knew without the bed)
   and `found`, the attributes `written`, `replaced`, `history` and `unchanged`, the verdicts
   and the profile rows. `refused` lists the lines the running bed refused, such as a name two
   places of the bed share, with exit 1; the rest is written. A second import of the same file
   reports every value `unchanged` and writes nothing.

## Single places from the command line

Between imports the secretary adds and judges places one at a time and searches the bed through
three commands of the same binary, clients of the running daemon like the import. Each prints
one JSON object on stdout: the answer with exit 0, or an error with exit 1, or exit 2 for
arguments that form no command (`{"error":"usage",...}`). A refusal is
`{"error":"refused","refused":[{"rule":...,"message":...}]}`, with `key` for an attribute, and
writes nothing; an unreachable socket is `{"error":"failed","message":...}`. Times are Unix
seconds and default to now; `--config <ulei.toml>` works as for the import.

Search. The filters are flags (`--cuisine` and `--status` may repeat), or the whole
`PlacesSearchRequest` of C7 goes in `--request`. The answer is the daemon's
`PlacesSearchResponse` as sent, best first, each place with its `maps_url`:

```sh
ulei-garden places search --cuisine noodles --near '0.5041, -25.0032' --max-distance-m 1500 \
  --min-rating 4.2 --max-price-for-two 60 --status active --vibe 'quiet courtyard' \
  --open-now --local-time 2031-03-20T19:30 --limit 3
ulei-garden places search --request '{"filters":{"cuisine":["noodles"]},"limit":3}'
```

`--open-now` without `--local-time`, or `--max-distance-m` without `--near` and without a
profile `home`, is refused as `invalid_request`.

A place found in a search. Write it as a candidate with where it was found as `--source`, and
each attribute as `--attr key=value` with the keys and value forms of the import; they take the
same source and `--checked-at`, the day the value was seen:

```sh
ulei-garden places add-candidate 'Tidepool Kitchen' --source fixture-delivery-app \
  --attr 'address=Harbour Row 9' --attr cuisine=seafood --attr 'location=0.5049, -25.0011' \
  --checked-at 1931990400
```

The answer is the place's `outcome` (`created`, `adopted` or `found`), its `id` and one
`outcome` per attribute. A place the bed already holds keeps its status: a candidate write never
demotes an active place. `--source` is required.

The owner's verdict. `--verdict` is `-1`, `0` or `+1`, `--source-ref` the message that carries
it (`chat:<message id>`, required), `--note` the owner's words and `--said-at` when the owner said
it. Name the place by `--id`, or by `--name`, which finds the one place of the bed with that name
or alias and writes a name it does not hold as a new candidate with the `--source-ref` as its
source. `--better-than` is the id of a place the owner said this one beats:

```sh
ulei-garden places verdict --name 'Tidepool Kitchen' --verdict +1 \
  --source-ref chat:msg-9003 --note 'good chowder, slow service' --said-at 1932076800
```

The answer is `{"outcome":"written","seq":...,"place":...}`, or `repeated` with the same `seq`
for a `--source-ref` the place already has. A place whose latest verdict is `-1` is not answered
by a search again.
