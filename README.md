# QBW Viewer

Open a QuickBooks Desktop company file (`.qbw`) **without QuickBooks**: open
**`QBW Viewer.html`** in Chrome, Edge, Firefox or Safari and drag your `.qbw` onto it.

- Nothing to install. It's one file and works offline.
- Your file never leaves your computer. It is decoded inside the browser and is never modified.

## What you can see

- **Overview**: transaction counts and totals by type and by year, plus the date range
- **Vendors & names**: vendor, customer, account and item lists with the readable details stored
  for each one (address, phone, email…), how many transactions each vendor has, and CSV export
- **Transactions**: filter by type, date, name or amount, then click a row to see its line items. CSV export.
- **All data**: search every piece of readable text in the file

## How it works

The decoding is done by [OpenQBW](https://github.com/Sigilweaver/OpenQBW), an open-source
(Apache-2.0), read-only, reverse-engineered `.qbw` parser. It is compiled to WebAssembly
(`wasm/`) and embedded in the HTML page. A lightly patched copy of OpenQBW lives in
`wasm/vendor/openqbw` (see its `VENDORED.md`): the viewer decodes each page once instead of once per
extraction pass and skips the slow key search on pages that hold no QuickBooks data, which makes
large files several times faster. The patched routines give the same results as the originals
(checked by tests).

OpenQBW extracts transactions, dates and amounts. It does not label name or address fields yet,
so this viewer recovers them from the readable text in each record. It uses the first text in a
vendor record as the name and the rest as details. Bills and other transactions are linked to a
vendor when the transaction record points at that vendor's record.

## Limitations

- Tested by OpenQBW on files from QuickBooks Desktop 2018 through Enterprise. Older files and
  password-protected files cannot be opened.
- Some line amounts use an encoding OpenQBW cannot read yet. They are marked `*` and left out of
  totals, so totals can be lower than QuickBooks shows.
- Name and transaction linking is best-effort. If a list isn't picked automatically, choose it from
  the **List** menu or use **All data** search.
- Very large files (over 1–2 GB) may exceed the browser's memory.

## Rebuilding (developers only)

```bash
rustup target add wasm32-unknown-unknown
python3 build.py        # rebuilds wasm/ and regenerates "QBW Viewer.html" from viewer/app.html
```
