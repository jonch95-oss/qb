# QBW Viewer

Open and browse a QuickBooks Desktop company file (`.qbw`) **without QuickBooks**.

Everything runs locally: the file is decoded on your machine and shown in your
browser at `127.0.0.1`. Nothing is uploaded.

## How it works

1. `qbw_viewer.py` copies your `.qbw` to a temp folder (the original is never modified)
   and decodes it with [OpenQBW](https://github.com/Sigilweaver/OpenQBW), an open-source
   (Apache-2.0), read-only, reverse-engineered `.qbw` parser.
2. The decoded data is written to `exports/<Company>.sqlite`.
3. A local viewer opens in your browser with:
   - **Overview**: transaction counts and totals by type and by year, and the date range
   - **Transactions**: filter by type, date, ID and amount, sort, click a row to see its line items, export to CSV
   - **Raw tables**: page through every exported table
   - **SQL**: run your own queries against the export (`qb_date(txn_date_raw)` converts dates)

## Requirements

- Python 3.9+
- To build the parser on first run: [git](https://git-scm.com) and [Rust](https://rustup.rs).
  The script clones and builds OpenQBW into `./vendor` automatically; this takes a minute or two
  the first time. If you already have an `openqbw` binary, pass `--openqbw PATH` or put it on `PATH`.

## Usage

```bash
python qbw_viewer.py "C:\Users\me\Documents\Company.qbw"
```

On Windows you can also drag the `.qbw` onto **`View QBW.bat`**.

Other options:

```bash
python qbw_viewer.py Company.qbw --no-browser    # export only -> exports/Company.sqlite
python qbw_viewer.py --view exports/Company.sqlite  # reopen a previous export
```

You can also open `viewer/index.html` directly and drag an exported `.sqlite` onto it.

## Limitations (from the OpenQBW parser)

- **What is decoded today:** transactions (invoices, bills, checks, journal entries, deposits, …)
  with their dates, document counters, line items and amounts.
- **Not decoded yet:** names and text fields such as customers, vendors, accounts, item names and memos.
  Transactions and items show their internal QuickBooks IDs instead.
- Some line amounts use an encoding OpenQBW cannot read yet ("deferred"). They are flagged with `*`
  and left out of totals, so totals can be lower than in QuickBooks.
- Tested by OpenQBW on files from QuickBooks Desktop 2018 through Enterprise. Older files, and files
  protected with a password or strong encryption, may not open. This tool does not break passwords.

If you need complete reports (names, P&L, balance sheet), the reliable route is still to open
the file in a copy of QuickBooks Desktop (a trial works) and export to Excel/CSV/IIF.
