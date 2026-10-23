# QBW Viewer

Open a QuickBooks Desktop company file (`.qbw`) **without QuickBooks**: open
**`QBW Viewer.html`** in Chrome, Edge, Firefox or Safari and drag your `.qbw` onto it.

- Nothing to install. It's one file and works offline.
- Your file never leaves your computer. It is decoded inside the browser and is never modified.
- A ~90 MB company file takes about 20–30 seconds, with a progress bar.

## What you can see

A QuickBooks-style layout with a sidebar:

- **Home**: counts, money in and out by year, top vendors, transactions by type, most recent transactions
- **Vendor, Customer and Employee Centers, plus Other Names**: pick a name to see its transactions (type, date,
  account, amount) with totals, filters and CSV export
- **Chart of Accounts**: every account, job and class used on transactions, each with its register
- **Transactions**: the full journal, filterable by type, year or date range, name/account and amount
- **Lists**: vendors, customers/jobs, accounts, terms, memorized reports and other lists, with dates added
- **Search**: full-text search over every readable record

## How it works

1. Pages are de-obfuscated with the key-recovery routines of [OpenQBW](https://github.com/Sigilweaver/OpenQBW),
   an open-source (Apache-2.0) `.qbw` parser. A patched copy lives in `wasm/vendor/openqbw` (see its
   `VENDORED.md`). It computes each page's keystream once instead of up to three times, with results
   identical to the originals (checked by tests).
2. List records (vendors, customers, accounts…) are read directly. They share one row layout
   (two timestamp blocks, the length-prefixed name, the ListID and a GUID), and the row's column bitmap
   tells the lists apart. See the comments at the top of `wasm/src/lib.rs`.
3. Transactions are read from their header rows (key, entry time, transaction date as minutes since
   1600-02-29, names) and line rows (account and amount). The transaction type (check, credit card charge,
   deposit, invoice, payment) is inferred from the header's column bitmap and accounts.

Everything is compiled to WebAssembly and embedded in the HTML page.

## Limitations

- Amounts use SQL Anywhere's decimal (digit count, sign/exponent byte, base-100 pairs). Small values read as an
  integer whose last pair sits at 100^exponent (e.g. $98.04 = `02 3f 62 04`); larger multi-pair values put their first
  pair at the exponent's place (an $8,000 estimate = `02 41 50 01`, matching that job's `01 41 50` invoices). This was
  fitted against invoices, payments and checks in four company files, not taken from documentation.
- Home's yearly figures keep invoices, money received and spending in separate columns (they describe the same money
  at different stages), ignore estimates, and use each transaction's own total rather than summing its item lines.
- QuickBooks keeps old copies of edited or reprinted checks, so a check can appear more than once.
- Often only some of a transaction's lines are decoded (e.g. the expense side, not the bank side).
- Transaction types are inferred; a few hundred show as "Other".
- List labels ("Vendors", "Accounts"…) are inferred from the contents; pick another list if one looks off.
- Pages the cheaper key checks can't unlock are skipped. On the files examined they held no readable data.
- Password-protected files cannot be opened.

## Rebuilding (developers only)

```bash
rustup target add wasm32-unknown-unknown
python3 build.py        # rebuilds wasm/ and regenerates "QBW Viewer.html" from viewer/app.html
cd wasm && cargo test --release && (cd vendor/openqbw && cargo test --release)
```
