//! WebAssembly entry point for the drag-and-drop QBW viewer.
//!
//! Exposes a tiny C ABI (no wasm-bindgen) so the page can load the module
//! with plain `WebAssembly.instantiate`:
//!
//! * `alloc(len) -> ptr`        reserve `len` bytes for the .qbw contents
//! * `analyze(ptr, len) -> 0|1` decode the file (takes ownership of the buffer)
//! * `result_ptr()` / `result_len()` the UTF-8 JSON produced by `analyze`
//! * imports `env.progress(step, done, total)` to report progress
//!
//! Pages are de-obfuscated with OpenQBW's key-recovery routines (vendored and
//! sped up, see `vendor/openqbw/VENDORED.md`). On top of that this module
//! reads QuickBooks *list* records directly - vendors, customers and jobs,
//! accounts, terms, and so on - which share one row layout:
//!
//! ```text
//! [row_len u16][flags 0x40][n][column bitmap ...][seq u32]
//! [modified 8 bytes][modified 8 bytes again][created 4][created 4 again]
//! [len u8][name ...] ... [0x10][ListID, 16 base62] ... [0x24][GUID, 36]
//! ```
//!
//! The column bitmap is the same for every row of a list, so it is used to
//! group names into lists.
//!
//! Transactions are read the same way. A transaction header row is
//!
//! ```text
//! [row_len u16][00][bitmap 4][key_a u32][key_b u32][created u32][modified u32]
//! [txn date: minutes since 1600-02-29, always midnight][...]
//! ... [0x80][name kind][00][len][name] ...   kind 0 customer/job, 1 vendor,
//!                                           2 employee, 3 other name
//! ```
//!
//! and each of its lines (splits) starts with the same `key_a key_b`, then a
//! line number and line id, the account (`[00 00 80][len][name]`) and, after
//! the line's name reference, the amount as an SQL Anywhere decimal
//! (`[digit count][sign|exponent][base-100 digits]`). The header bitmap
//! differs by transaction type (and by which optional columns are filled).
//! OpenQBW's own table map and transaction decoding do not match newer company
//! files, so they are not used here.

use std::collections::{HashMap, HashSet};


use openqbw::{
    deobfuscate_with_steps, oracle_bv_e_page, recover_bv_brute_with_steps,
    recover_bv_qb_data_with_steps, sector_steps,
};
use opensqlany::{Page, PageStore, PageType, SlottedPage};
use serde_json::{Value, json};

const PAGE_BODY_END: usize = 0xFF0;
const MAX_TEXT_ROWS: usize = 300_000;
const ID_LEN: usize = 16;

static mut RESULT: Vec<u8> = Vec::new();

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn progress(step: u32, done: u32, total: u32);
}

/// Tell the page how far along we are (step 1..=2, `done` of `total`).
fn report(step: u32, done: usize, total: usize) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        progress(step, done as u32, total as u32)
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (step, done, total);
}

#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// `ptr` must come from `alloc(len)` and hold `len` initialised bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn analyze(ptr: *mut u8, len: usize) -> u32 {
    let bytes = unsafe { Vec::from_raw_parts(ptr, len, len) };
    let (ok, value) = match analyze_bytes(bytes) {
        Ok(v) => (1, v),
        Err(e) => (0, json!({ "error": e })),
    };
    let out = serde_json::to_vec(&value).unwrap_or_default();
    unsafe { RESULT = out };
    ok
}

#[unsafe(no_mangle)]
pub extern "C" fn result_ptr() -> *const u8 {
    unsafe { (*std::ptr::addr_of!(RESULT)).as_ptr() }
}

#[unsafe(no_mangle)]
pub extern "C" fn result_len() -> usize {
    unsafe { (*std::ptr::addr_of!(RESULT)).len() }
}

/// De-obfuscate one data page, trying OpenQBW's key checks from cheapest to
/// most expensive: the QuickBooks data-page anchor, the page-start oracle
/// (accepted when the result has at least 5 % zero bytes), then the
/// histogram brute force. Pages none of these unlock are skipped; on the
/// files examined they hold no readable data, and the remaining fallback
/// (`ApModel::deobfuscate_with_store`) costs more than the rest combined.
fn decode_page(pn: u64, raw: &[u8]) -> Option<Vec<u8>> {
    let steps = sector_steps(raw);
    if let Some(bv) = recover_bv_qb_data_with_steps(pn, raw, &steps) {
        return Some(deobfuscate_with_steps(raw, pn, bv, &steps));
    }
    let plain = deobfuscate_with_steps(raw, pn, oracle_bv_e_page(pn, raw), &steps);
    let zeros = plain[..PAGE_BODY_END].iter().filter(|&&b| b == 0).count();
    if zeros * 100 / PAGE_BODY_END >= 5 {
        return Some(plain);
    }
    recover_bv_brute_with_steps(pn, raw, &steps).map(|bv| deobfuscate_with_steps(raw, pn, bv, &steps))
}

/// One record of a QuickBooks list.
#[derive(Debug, Clone, PartialEq)]
pub struct ListEntry {
    pub name: String,
    pub list_id: Option<String>,
    /// Column bitmap of the row, hex; identical for every row of one list.
    pub group: String,
    /// Other readable fields of the row (address lines, phone, notes ...).
    pub details: Vec<String>,
    /// Creation time, Unix seconds.
    pub created: u32,
    pub page: u64,
}

pub fn analyze_bytes(bytes: Vec<u8>) -> Result<Value, String> {
    analyze_bytes_with(bytes, false)
}

/// `deep`: also unlock the pages the cheap key checks miss, using OpenQBW's
/// slow per-page fallback (`ApModel::deobfuscate_with_store`).
pub fn analyze_bytes_with(bytes: Vec<u8>, deep: bool) -> Result<Value, String> {
    let file_size = bytes.len();
    let store = PageStore::from_bytes(bytes)
        .map_err(|e| format!("This does not look like a QuickBooks company file ({e})."))?;
    if store.superblock().is_err() {
        return Err("This is not a QuickBooks Desktop company file (.qbw), or it is from a \
                    version older than 2018 that this reader does not support."
            .into());
    }
    let n = store.page_count() as usize;

    let model = deep.then(|| opensqlany::ApModel::learn(&store));

    // Step 1: decode every data page once.
    let mut plain: Vec<Option<Vec<u8>>> = vec![None; n];
    let mut decoded = 0usize;
    for pn in 1..n as u64 {
        if pn % 64 == 0 {
            report(1, pn as usize, n);
        }
        let Ok(page) = store.page(pn) else { continue };
        if page.trailer().page_type() != PageType::Extent {
            continue;
        }
        if let Some(p) = decode_page(pn, page.bytes()) {
            plain[pn as usize] = Some(p);
            decoded += 1;
        } else if let Some(model) = &model {
            plain[pn as usize] = Some(model.deobfuscate_with_store(page.bytes(), pn, &store));
            decoded += 1;
        }
    }

    // Step 2: read lists, transactions and readable text. Transactions store
    // names as text, so the page counts and shows the records mentioning
    // each name from `text`.
    let mut entries: Vec<ListEntry> = Vec::new();
    let mut text: Vec<Value> = Vec::new();
    for (pn, p) in plain.iter().enumerate() {
        let Some(p) = p else { continue };
        if pn % 256 == 0 {
            report(2, pn, n);
        }
        let before = entries.len();
        list_entries(pn as u64, p, &mut entries);
        if text.len() < MAX_TEXT_ROWS {
            let is_list = entries.len() > before;
            for strings in page_text_rows(pn as u64, p) {
                text.push(json!([pn, is_list, strings]));
            }
        }
    }
    let mut headers: Vec<TxnHeader> = Vec::new();
    for (pn, p) in plain.iter().enumerate() {
        if let Some(p) = p {
            txn_headers(pn as u64, &p[..PAGE_BODY_END], &mut headers);
        }
    }
    let mut seen = HashSet::new();
    headers.retain(|h| seen.insert(h.key));
    let keys: HashSet<(u32, u32)> = headers.iter().map(|h| h.key).collect();
    let mut lines: HashMap<(u32, u32), Vec<TxnLine>> = HashMap::new();
    for p in plain.iter().flatten() {
        txn_lines(&p[..PAGE_BODY_END], &keys, &mut lines);
    }
    report(2, n, n);
    dedupe(&mut entries);
    if decoded == 0 {
        return Err("The file opened, but none of its pages could be unlocked. It may be \
                    password-protected or use a format this reader does not support yet."
            .into());
    }

    Ok(json!({
        "file_size": file_size,
        "pages": n,
        "decoded_pages": decoded,
        "lists": entries.iter().map(|e| {
            json!({ "n": e.name, "id": e.list_id, "g": e.group, "d": e.details, "c": e.created, "p": e.page })
        }).collect::<Vec<_>>(),
        "text": text,
        "txns": headers.iter().map(|h| {
            let mut ls = lines.remove(&h.key).unwrap_or_default();
            ls.sort_by_key(|l| (l.line_no, l.line_id));
            ls.dedup_by_key(|l| l.line_id);
            let main = ls.iter().find(|l| l.line_no == 0);
            json!({
                "t": h.bitmap, "d": h.date_days, "c": h.created,
                "n": h.names.iter().map(|(k, s)| json!([k, s])).collect::<Vec<_>>(),
                "l": ls.iter().filter(|l| l.line_no > 0).map(|l| json!([l.account, l.amount])).collect::<Vec<_>>(),
                "m": main.map(|l| json!([l.account, l.amount, l.ref_no])),
            })
        }).collect::<Vec<_>>(),
    }))
}

/// SQL Anywhere TIMESTAMP/DATE values count minutes from this day.
const SA_EPOCH_DAYS_BEFORE_UNIX: i64 = 135_081; // 1600-02-29 .. 1970-01-01
const MIN_TXN_MINUTES: u32 = 220_000_000; // ~2018
const MAX_TXN_MINUTES: u32 = 228_000_000; // ~2033

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn is_unix_time(v: u32) -> bool {
    (1_400_000_000..2_100_000_000).contains(&v)
}

/// A transaction header row.
#[derive(Debug, Clone, PartialEq)]
pub struct TxnHeader {
    pub key: (u32, u32),
    /// Column bitmap of the row, hex: identifies the transaction type.
    pub bitmap: String,
    /// Transaction date, days since 1970-01-01.
    pub date_days: i64,
    /// Entry time, Unix seconds.
    pub created: u32,
    /// Names on the transaction: (kind, name); kind 0 customer/job,
    /// 1 vendor, 2 employee, 3 other name.
    pub names: Vec<(u8, String)>,
}

/// Find transaction header rows: `[key_a][key_b][created][modified][date]`
/// with both times plausible Unix times and the date exactly midnight.
fn txn_headers(pn: u64, body: &[u8], out: &mut Vec<TxnHeader>) {
    let _ = pn;
    let mut starts = Vec::new();
    let mut j = 15;
    while j + 16 <= body.len() {
        let (c1, c2, d) = (u32_at(body, j), u32_at(body, j + 4), u32_at(body, j + 8));
        if is_unix_time(c1)
            && is_unix_time(c2)
            && c1.abs_diff(c2) < 400_000_000
            && (MIN_TXN_MINUTES..MAX_TXN_MINUTES).contains(&d)
            && d % 1440 == 0
        {
            starts.push(j);
            j += 16;
        } else {
            j += 1;
        }
    }
    // A bitmap made of letters is text that happened to match ("dmin").
    starts.retain(|&j| !body[j - 12..j - 8].iter().all(|b| b.is_ascii_alphabetic()));
    for (k, &j) in starts.iter().enumerate() {
        // The row length sits before `[00][bitmap 4][key 8]`; fall back to
        // the next header when it does not fit.
        let row_end = (j - 15) + u16::from_le_bytes([body[j - 15], body[j - 14]]) as usize;
        let next = starts.get(k + 1).map_or(body.len(), |&s| s.saturating_sub(15));
        let end = if row_end > j + 16 && row_end <= body.len() { row_end } else { next.min(j + 420) };
        let end = end.max(j + 16);
        out.push(TxnHeader {
            key: (u32_at(body, j - 8), u32_at(body, j - 4)),
            bitmap: body[j - 12..j - 8].iter().map(|x| format!("{x:02x}")).collect(),
            date_days: (u32_at(body, j + 8) / 1440) as i64 - SA_EPOCH_DAYS_BEFORE_UNIX,
            created: u32_at(body, j),
            names: name_refs(&body[j + 16..end]),
        });
    }
}

/// Name references `[0x80][kind 0..=3][00][len][name]` in a row.
fn name_refs(row: &[u8]) -> Vec<(u8, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 5 <= row.len() {
        if row[i] == 0x80 && row[i + 1] <= 3 && row[i + 2] == 0 {
            let len = row[i + 3] as usize;
            if (2..=64).contains(&len)
                && i + 4 + len <= row.len()
                && let Some(name) = clean(&row[i + 4..i + 4 + len])
            {
                out.push((row[i + 1], name));
                i += 4 + len;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// One line of a transaction. Line 0 is the bank, card or register side
/// and carries the transaction total and its reference (check) number;
/// lines 1.. are the splits.
#[derive(Debug, Clone, PartialEq)]
pub struct TxnLine {
    pub line_no: u32,
    pub line_id: u32,
    pub account: Option<String>,
    pub amount: Option<f64>,
    pub ref_no: Option<String>,
}

/// Find the lines of known transactions: `[key_a][key_b][line no][line id]`,
/// then the account, (on line 0) the reference number, the line's name
/// reference and its amount. (The rate after the quantity is not used: it
/// is stored at a different scale.)
fn txn_lines(body: &[u8], keys: &HashSet<(u32, u32)>, out: &mut HashMap<(u32, u32), Vec<TxnLine>>) {
    let mut j = 0;
    while j + 16 <= body.len() {
        let key = (u32_at(body, j), u32_at(body, j + 4));
        let line_no = u32_at(body, j + 8);
        let line_id = u32_at(body, j + 12);
        if line_no > 500 || !(key.0 < line_id && line_id < key.0.saturating_add(5000)) || !keys.contains(&key) {
            j += 1;
            continue;
        }
        let end = (j + 420).min(body.len());
        let row = &body[j + 16..end];
        let mut account = None;
        let mut ref_no = None;
        if let Some(p) = row.windows(3).take(32).position(|w| w == [0, 0, 0x80]) {
            let len = row.get(p + 3).copied().unwrap_or(0) as usize;
            if (2..=64).contains(&len) && p + 4 + len <= row.len() {
                account = clean(&row[p + 4..p + 4 + len]);
                // `[len][digits]` right after the account: the check or ref number.
                let r = p + 4 + len;
                if line_no == 0
                    && let Some(&rlen) = row.get(r)
                    && (1..=12).contains(&rlen)
                    && let Some(digits) = row.get(r + 1..r + 1 + rlen as usize)
                    && digits.iter().all(|b| b.is_ascii_alphanumeric() || b"-/#".contains(b))
                    && digits.iter().any(|b| b.is_ascii_digit())
                {
                    ref_no = Some(String::from_utf8_lossy(digits).into_owned());
                }
            }
        }
        let mut amount = None;
        let mut i = 0;
        while i + 5 <= row.len() {
            if row[i] == 0x80 && row[i + 1] <= 3 && row[i + 2] == 0 {
                let len = row[i + 3] as usize;
                if (2..=64).contains(&len) && i + 4 + len < row.len() {
                    amount = sa_decimal(&row[i + 4 + len..]);
                    break;
                }
            }
            i += 1;
        }
        let amount = amount.map(|v| (v * 100.0).round() / 100.0);
        out.entry(key).or_default().push(TxnLine { line_no, line_id, account, amount, ref_no });
        j += 16;
    }
}

/// SQL Anywhere NUMERIC: `[digit count][sign bit | exponent][base-100
/// digits]`. The digits form an integer whose last pair is the
/// 100^(exponent - 0x40) place (trailing zero pairs are dropped), so $60 is
/// `01 40 3c` and $98.04 is `02 3f 62 04`. The high bit marks a negative.
fn sa_decimal(b: &[u8]) -> Option<f64> {
    let (&count, rest) = b.split_first()?;
    let (&exp, digits) = rest.split_first()?;
    let count = count as usize;
    if !(1..=8).contains(&count) || !(0x3a..0x46).contains(&(exp & 0x7f)) || digits.len() < count {
        return None;
    }
    let digits = &digits[..count];
    if digits.iter().any(|&d| d > 99) {
        return None;
    }
    let n = digits.iter().fold(0f64, |n, &d| n * 100.0 + d as f64);
    let v = n * 100f64.powi((exp & 0x7f) as i32 - 0x40);
    let v = (v * 10_000.0).round() / 10_000.0;
    Some(if exp & 0x80 != 0 { -v } else { v })
}

/// Find list records on a decoded page by scanning for their timestamp
/// blocks. (The slot directory is not used: on newer files it slices rows
/// at the wrong boundaries.)
fn list_entries(pn: u64, plain: &[u8], out: &mut Vec<ListEntry>) {
    let body = &plain[..PAGE_BODY_END];
    let mut starts = Vec::new();
    let mut i = 0;
    while let Some(k) = find_timestamps(body, i, body.len()) {
        starts.push(k);
        i = k + 24;
    }
    for (j, &k) in starts.iter().enumerate() {
        let end = starts.get(j + 1).map_or(body.len(), |&s| s.saturating_sub(16));
        if let Some(e) = entry_at(pn, body, k, end.max(k + 25)) {
            out.push(e);
        }
    }
}

/// Position of the next `[a 8][a 8][b 4][b 4]` timestamp block whose
/// length-prefixed name follows, in `buf[from..to]`.
fn find_timestamps(buf: &[u8], from: usize, to: usize) -> Option<usize> {
    let to = to.min(buf.len());
    let mut k = from;
    while k + 27 <= to {
        let w = &buf[k..];
        if w[..8] == w[8..16]
            && w[16..20] == w[20..24]
            && w[..8] != [0u8; 8]
            && (2..=64).contains(&w[24])
            && k + 25 + w[24] as usize <= buf.len()
            && looks_like_name(&w[25..25 + w[24] as usize])
        {
            return Some(k);
        }
        k += 1;
    }
    None
}

fn looks_like_name(s: &[u8]) -> bool {
    s.iter().all(|&b| printable(b))
        && s.iter().filter(|b| b.is_ascii()).count() * 10 >= s.len() * 9
        && s.windows(2).any(|w| w[0].is_ascii_alphabetic() && w[1].is_ascii_alphabetic())
}

/// Parse the record whose timestamp block starts at `k`.
fn entry_at(pn: u64, buf: &[u8], k: usize, end: usize) -> Option<ListEntry> {
    let len = buf[k + 24] as usize;
    let name = clean(&buf[k + 25..k + 25 + len])?;
    let group = column_bitmap(&buf[k.saturating_sub(18)..k]);
    let created = u32::from_le_bytes([buf[k + 16], buf[k + 17], buf[k + 18], buf[k + 19]]);
    let tail = &buf[(k + 25 + len).min(end)..end];
    let mut list_id = None;
    let mut details = Vec::new();
    for s in extract_strings(tail) {
        if is_list_id(&s) {
            list_id.get_or_insert(s);
        } else if is_clean_text(&s) && s != name && !details.contains(&s) {
            details.push(s);
        }
    }
    Some(ListEntry { name, list_id, group, details, created, page: pn })
}

/// The column bitmap of a row, from the bytes just before its timestamp
/// block: `[row_len u16][0x40][n][bitmap ...][seq u32]`. The flags byte is
/// found by searching back from the sequence number; the bitmap is
/// normalised (count byte and trailing zeros dropped) so rows of the same
/// list compare equal.
fn column_bitmap(before: &[u8]) -> String {
    let n = before.len();
    if n < 8 {
        return String::new();
    }
    let seq_start = n - 4;
    let mut b = match (seq_start.saturating_sub(12)..seq_start - 1).rev().find(|&i| before[i] == 0x40) {
        Some(i) => &before[i + 1..seq_start],
        None => &before[seq_start - 4..seq_start],
    };
    if let [c, rest @ ..] = b
        && *c < 0x20
    {
        b = rest;
    }
    while let [rest @ .., 0] = b {
        b = rest;
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn is_list_id(s: &str) -> bool {
    s.len() == ID_LEN && s.bytes().all(|b| b.is_ascii_alphanumeric()) && s.bytes().any(|b| b.is_ascii_digit())
}

/// Text worth showing: plain ASCII with at least two letters, and not an ID,
/// GUID or row-boundary fragment.
fn is_clean_text(s: &str) -> bool {
    // Short tokens mixing case, digits or symbols ("uT8", "?BZ", "00DZt")
    // are bytes of binary fields that happen to be printable.
    let short_junk = s.len() <= 5
        && !(s.bytes().all(|b| b.is_ascii_alphabetic() || b == b' ' || b == b'&' || b == b'.')
            || s.bytes().all(|b| b.is_ascii_digit()));
    !short_junk
        && s.bytes().all(|b| (0x20..0x7F).contains(&b))
        && s.bytes().filter(|b| b.is_ascii_alphabetic()).count() >= 2
        && !is_list_id(s)
        && !is_guid_like(s)
        && !s.starts_with("0000000")
}

/// GUIDs, including ones whose bytes were split by a page boundary.
fn is_guid_like(s: &str) -> bool {
    let s = s.trim_start_matches('$');
    s.len() >= 8 && s.contains('-') && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// QuickBooks keeps older copies of rows around; keep one per list ID (or per
/// name when the ID was not found), preferring the copy with more details.
fn dedupe(entries: &mut Vec<ListEntry>) {
    let mut best: HashMap<(String, String), usize> = HashMap::new();
    let mut keep = vec![false; entries.len()];
    for (i, e) in entries.iter().enumerate() {
        let key = (e.group.clone(), e.list_id.clone().unwrap_or_else(|| e.name.to_lowercase()));
        match best.get(&key) {
            Some(&j) if entries[j].details.len() >= e.details.len() => {}
            Some(&j) => {
                keep[j] = false;
                keep[i] = true;
                best.insert(key, i);
            }
            None => {
                keep[i] = true;
                best.insert(key, i);
            }
        }
    }
    let mut i = 0;
    entries.retain(|_| {
        i += 1;
        keep[i - 1]
    });
}

/// Split a decoded page into rows and keep the readable strings of each.
fn page_text_rows(pn: u64, plain: &[u8]) -> Vec<Vec<String>> {
    let slotted = SlottedPage::parse(Page::from_bytes(pn, plain));
    let mut chunks: Vec<&[u8]> = slotted.row_bytes().into_iter().map(|(_, r)| r).collect();
    if chunks.is_empty() {
        chunks = split_on_zero_runs(&plain[..PAGE_BODY_END]);
    }
    chunks
        .into_iter()
        .map(extract_strings)
        .map(|v| {
            let mut out: Vec<String> = Vec::new();
            for s in v.into_iter().filter(|s| is_clean_text(s)) {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
            out
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn split_on_zero_runs(body: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut zeros = 0;
    for (i, &b) in body.iter().enumerate() {
        if b == 0 {
            zeros += 1;
            if zeros == 4 && i + 1 - zeros > start {
                out.push(&body[start..i + 1 - zeros]);
            }
        } else {
            if zeros >= 4 {
                start = i;
            }
            zeros = 0;
        }
    }
    if zeros < 4 && start < body.len() {
        out.push(&body[start..]);
    }
    out
}

fn printable(b: u8) -> bool {
    (0x20..0x7F).contains(&b) || b >= 0x80
}

fn clean(bytes: &[u8]) -> Option<String> {
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(), // Latin-1
    };
    let s = s.trim();
    let letters = s.chars().filter(|c| c.is_alphabetic()).count();
    let alnum = s.chars().filter(|c| c.is_alphanumeric()).count();
    let weird = s.chars().filter(|c| c.is_control() || ('\u{80}'..'\u{a0}').contains(c)).count();
    if s.chars().count() < 2 || alnum < 2 || weird > 0 {
        return None;
    }
    // Drop short symbol soup like "@A#" that shows up between binary fields.
    if letters == 0 && !s.chars().all(|c| c.is_ascii_digit() || " -().,/".contains(c)) {
        return None;
    }
    Some(s.to_owned())
}

/// Pull readable strings out of a row. SQL Anywhere stores short strings
/// with a one-byte length prefix; prefer that when it fits exactly, else
/// fall back to maximal printable runs of at least three bytes.
fn extract_strings(row: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < row.len() {
        let len = row[i] as usize;
        if (2..=250).contains(&len) && i + 1 + len <= row.len() {
            let body = &row[i + 1..i + 1 + len];
            let ends_clean = row.get(i + 1 + len).is_none_or(|&b| !printable(b));
            // A printable byte right after the string is usually the next
            // field's length or flag byte, so a name-like string is still
            // taken at its declared length.
            if body.iter().all(|&b| printable(b)) && (ends_clean || (len >= 3 && looks_like_name(body))) {
                if let Some(s) = clean(body) {
                    out.push(s);
                }
                i += 1 + len;
                continue;
            }
        }
        if printable(row[i]) {
            let start = i;
            while i < row.len() && printable(row[i]) {
                i += 1;
            }
            if i - start >= 3
                && let Some(s) = clean(&row[start..i])
            {
                out.push(s);
            }
            continue;
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefixed_before_flag_byte() {
        let mut row = vec![0x0f];
        row.extend_from_slice(b"Office Supplies5");
        assert_eq!(extract_strings(&row), vec!["Office Supplies"]);
    }

    #[test]
    fn length_prefixed_and_runs() {
        let mut row = vec![0x00, 0x05];
        row.extend_from_slice(b"Acme ");
        row.extend_from_slice(&[0x00, 0x01, 0x10]);
        row.extend_from_slice(b"0000000000001QBm");
        row.extend_from_slice(&[0x00, 0x00, 0xFF]);
        row.extend_from_slice(b"\x00Main Street\x00");
        assert_eq!(extract_strings(&row), vec!["Acme", "0000000000001QBm", "Main Street"]);
    }

    #[test]
    fn rejects_noise() {
        assert!(extract_strings(b"\x00@A#\x00\x01ab\x00").is_empty());
    }

    #[test]
    fn rejects_non_qbw() {
        assert!(analyze_bytes(vec![0u8; 100]).is_err());
    }

    /// A list row laid out as observed in a 2024-era company file.
    fn list_row(bitmap: &[u8], name: &str, id: &str) -> Vec<u8> {
        let mut r = vec![0xac, 0x00, 0x40, 0x08];
        r.extend_from_slice(bitmap);
        r.extend_from_slice(&[0x28, 0x01, 0x00, 0x00]);
        let modified = [0x3e, 0xb4, 0x38, 0x0d, 0xa8, 0x00, 0x9c, 0x01];
        r.extend_from_slice(&modified);
        r.extend_from_slice(&modified);
        r.extend_from_slice(&[0x22, 0xf1, 0xa4, 0x61, 0x22, 0xf1, 0xa4, 0x61]);
        r.push(name.len() as u8);
        r.extend_from_slice(name.as_bytes());
        r.extend_from_slice(&[0; 20]);
        r.push(0x10);
        r.extend_from_slice(id.as_bytes());
        r.extend_from_slice(&[0; 8]);
        r.push(0x24);
        r.extend_from_slice(b"fa20223f-b1e5-444b-aae1-73b7d065b825");
        r.extend_from_slice(&[0; 4]);
        r.push(9);
        r.extend_from_slice(b"Brooklyn ");
        r
    }

    #[test]
    fn parses_list_row() {
        let row = list_row(&[0xef, 0xc4, 0x38, 0x80], "Tri State Lumber", "0000000000000AWd");
        let k = find_timestamps(&row, 0, row.len()).unwrap();
        let e = entry_at(7, &row, k, row.len()).unwrap();
        assert_eq!(e.name, "Tri State Lumber");
        assert_eq!(e.list_id.as_deref(), Some("0000000000000AWd"));
        assert_eq!(e.group, "efc43880");
        assert_eq!(e.details, vec!["Brooklyn"]);
        assert_eq!(e.created, 0x61a4f122); // 2021-11-29
    }

    #[test]
    fn bitmap_normalisation() {
        // Headers seen in one file; the first two are the same list.
        let vendor = [0, 0, 0, 0, 0, 0, 0xac, 0, 0x40, 0x08, 0xef, 0xc4, 0x38, 0x80, 0x28, 0x01, 0, 0];
        let vendor_long = [0, 0, 0, 0, 0xaa, 0, 0x40, 0x40, 0x09, 0xef, 0xc4, 0x38, 0x80, 0, 0x25, 0x05, 0, 0];
        let account = [0, 0, 0, 0, 0, 0, 0xb2, 0, 0x40, 0x01, 0xff, 0xfb, 0x91, 0, 0x15, 0, 0, 0];
        assert_eq!(column_bitmap(&vendor), "efc43880");
        assert_eq!(column_bitmap(&vendor_long), "efc43880");
        assert_eq!(column_bitmap(&account), "fffb91");
    }

    #[test]
    fn sa_decimals() {
        assert_eq!(sa_decimal(&[0x02, 0xbf, 0x62, 0x04]), Some(-98.04));
        assert_eq!(sa_decimal(&[0x02, 0x3f, 0x54, 0x21]), Some(84.33));
        assert_eq!(sa_decimal(&[0x01, 0xc0, 0x3c]), Some(-60.0));
        assert_eq!(sa_decimal(&[0x01, 0x41, 0x08]), Some(800.0));
        assert_eq!(sa_decimal(&[0x02, 0xbf, 0x03, 0x35]), Some(-3.53));
        assert_eq!(sa_decimal(&[0x02, 0x3f, 0x99, 0x01]), None);
    }

    #[test]
    fn finds_transaction_and_lines() {
        // Header: bitmap, key, created x2, date 2021-12-19, then the payee.
        let mut page = vec![0u8; 40];
        page.extend_from_slice(&[0x77, 0x00, 0x00, 0xf0, 0x0b, 0x89, 0x80]);
        page.extend_from_slice(&7696u32.to_le_bytes());
        page.extend_from_slice(&1021u32.to_le_bytes());
        page.extend_from_slice(&0x61c2_5f2fu32.to_le_bytes());
        page.extend_from_slice(&0x61c2_5f2fu32.to_le_bytes());
        page.extend_from_slice(&0x0d39_2120u32.to_le_bytes());
        page.extend_from_slice(&[0; 24]);
        page.extend_from_slice(&[0x80, 0x01, 0x00, 0x08]);
        page.extend_from_slice(b"Speedway");
        page.extend_from_slice(&[0; 70]);
        // Line: key, line 1, line id, account, payee, amount.
        page.extend_from_slice(&7696u32.to_le_bytes());
        page.extend_from_slice(&1021u32.to_le_bytes());
        page.extend_from_slice(&1u32.to_le_bytes());
        page.extend_from_slice(&7698u32.to_le_bytes());
        page.extend_from_slice(&[0x33, 0x00, 0x00, 0x80, 0x16]);
        page.extend_from_slice(b"Automobile Expense:Gas");
        page.extend_from_slice(&[0x17, 0x00, 0x00, 0x80, 0x01, 0x00, 0x08]);
        page.extend_from_slice(b"Speedway");
        page.extend_from_slice(&[0x02, 0xbf, 0x62, 0x04, 0x00]);
        page.extend_from_slice(&[0; 40]);

        let mut headers = Vec::new();
        txn_headers(1, &page, &mut headers);
        assert_eq!(headers.len(), 1);
        let h = &headers[0];
        assert_eq!(h.key, (7696, 1021));
        assert_eq!(h.bitmap, "f00b8980");
        assert_eq!(h.date_days, 18980); // 2021-12-19
        assert_eq!(h.names, vec![(1, "Speedway".to_string())]);

        let keys = HashSet::from([h.key]);
        let mut lines = HashMap::new();
        txn_lines(&page, &keys, &mut lines);
        assert_eq!(
            lines[&h.key],
            vec![TxnLine { line_no: 1, line_id: 7698, account: Some("Automobile Expense:Gas".into()), amount: Some(-98.04), ref_no: None }]
        );
    }

    #[test]
    fn guid_like() {
        assert!(is_guid_like("$3e709ea9-ff44-42e54bc21-e3d2-4f0c"));
        assert!(!is_guid_like("Main St"));
        assert!(!is_guid_like("2% 10 Net 30"));
        assert!(is_clean_text("Chase 4171"));
        assert!(!is_clean_text("0000000000000BH3\u{ce}@9"));
        assert!(!is_clean_text("0000000000000AWd"));
        assert!(!is_clean_text("uT8") && !is_clean_text("?BZ") && !is_clean_text("00DZt"));
        assert!(is_clean_text("Admin") && is_clean_text("BP") && is_clean_text("AT&T"));
    }

    #[test]
    fn dedupe_prefers_detailed_copy() {
        let mk = |d: &[&str]| ListEntry {
            name: "A".into(),
            list_id: Some("0000000000000AAA".into()),
            group: "g".into(),
            details: d.iter().map(|s| s.to_string()).collect(),
            created: 0,
            page: 1,
        };
        let mut v = vec![mk(&[]), mk(&["x"]), mk(&[])];
        dedupe(&mut v);
        assert_eq!(v, vec![mk(&["x"])]);
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    /// QBW_BENCH=in.qbw [QBW_OUT=out.json] cargo test --release bench -- --ignored --nocapture
    #[test]
    #[ignore]
    fn analyze_file() {
        let path = std::env::var("QBW_BENCH").expect("set QBW_BENCH");
        let t = Instant::now();
        let deep = std::env::var("QBW_DEEP").is_ok();
        let r = analyze_bytes_with(std::fs::read(path).unwrap(), deep);
        eprintln!("analyze {:?} ok={}", t.elapsed(), r.is_ok());
        if let (Ok(v), Ok(out)) = (r, std::env::var("QBW_OUT")) {
            std::fs::write(out, serde_json::to_vec(&v).unwrap()).unwrap();
        }
    }
}
