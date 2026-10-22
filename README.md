# QBW Viewer

Open a QuickBooks Desktop company file (`.qbw`) **without QuickBooks**: open
**`QBW Viewer.html`** in Chrome, Edge, Firefox or Safari and drag your `.qbw` onto it.

- Nothing to install. It's one file and works offline.
- Your file never leaves your computer. It is decoded inside the browser and is never modified.
- A ~90 MB company file takes about 20–30 seconds, with a progress bar.

## What you can see

- **Overview**: vendor, customer/job and account counts, the most-used vendors, and every list found
- **Vendors & lists**: vendors, customers/jobs/properties, accounts, payment terms, memorized reports and
  other lists, with the date each entry was added and how many records mention it. Click a name to see
  those records: the account, payee, job or property and memo of each. CSV export.
- **Search everything**: full-text search over every readable record in the file

## How it works

1. Pages are de-obfuscated with the key-recovery routines of [OpenQBW](https://github.com/Sigilweaver/OpenQBW),
   an open-source (Apache-2.0) `.qbw` parser. A patched copy lives in `wasm/vendor/openqbw` (see its
   `VENDORED.md`). It computes each page's keystream once instead of up to three times, with results
   identical to the originals (checked by tests).
2. List records (vendors, customers, accounts…) are read directly. They share one row layout
   (two timestamp blocks, the length-prefixed name, the ListID and a GUID), and the row's column bitmap
   tells the lists apart. See the comments at the top of `wasm/src/lib.rs`.
3. Transactions store names as text, so "mentions" counts the records containing a name exactly.

Everything is compiled to WebAssembly and embedded in the HTML page.

## Limitations

- **Transaction amounts and dates are not decoded yet.** OpenQBW's transaction decoding and table map
  were built from older sample files and don't match newer company files, so they are not used.
- List labels ("Vendors", "Accounts"…) are inferred from the contents; pick another list if one looks off.
- Pages the cheaper key checks can't unlock are skipped. On the files examined they held no readable data.
- Password-protected files cannot be opened.

## Rebuilding (developers only)

```bash
rustup target add wasm32-unknown-unknown
python3 build.py        # rebuilds wasm/ and regenerates "QBW Viewer.html" from viewer/app.html
cd wasm && cargo test --release && (cd vendor/openqbw && cargo test --release)
```
