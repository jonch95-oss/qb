//! WebAssembly entry point for the drag-and-drop QBW viewer.
//!
//! Exposes a tiny C ABI (no wasm-bindgen) so the page can load the module
//! with plain `WebAssembly.instantiate`:
//!
//! * `alloc(len) -> ptr`      reserve `len` bytes for the .qbw contents
//! * `analyze(ptr, len) -> 0|1` decode the file (takes ownership of the buffer)
//! * `result_ptr()` / `result_len()` the UTF-8 JSON produced by `analyze`
//!
//! Decoding is done by OpenQBW / OpenSQLAnywhere. On top of their transaction
//! and line-item extraction this module pulls the readable text out of every
//! row of every QuickBooks table, which is where names (vendors, customers,
//! accounts, items), addresses and memos live. OpenQBW does not type those
//! columns yet, so the text is recovered heuristically per row.

use std::collections::{BTreeMap, HashMap};

use openqbw::{
    LineItem, PageAttribution, SysTableEntry, TransactionHeader, deobfuscate_with_steps,
    oracle_bv_e_page, recover_bv_brute_with_steps, recover_bv_qb_data_with_steps, scan, sector_steps,
};
use opensqlany::{ApModel, Page, PageStore, PageType, SlottedPage};
use serde_json::{Value, json};

const PAGE_BODY_END: usize = 0xFF0;
/// Window the OpenQBW line-item scanner expects (its `PAGE_DATA_END`).
const LINEITEM_BODY_END: usize = 0xFE0;
const MAX_TEXT_ROWS: usize = 400_000;

static mut RESULT: Vec<u8> = Vec::new();

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn progress(step: u32, done: u32, total: u32);
}

/// Tell the page how far along we are (step 1..=3, `done` of `total`).
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

fn is_system_name(name: &str) -> bool {
    name.starts_with("SYS")
        || name.starts_with("ISYS")
        || name.starts_with("RS_")
        || name.starts_with("dbo")
}

/// Cheap key recovery: the QuickBooks data-page anchor, then the E-page
/// oracle accepted when the result looks like real page content (at least
/// 5 % zero bytes). These are the first two stages of OpenQBW's
/// `recover_bv_any`; only its brute-force last stage is left out.
fn quick_plain(pn: u64, raw: &[u8], steps: &[u8; 8]) -> Option<Vec<u8>> {
    if let Some(bv) = recover_bv_qb_data_with_steps(pn, raw, steps) {
        return Some(deobfuscate_with_steps(raw, pn, bv, steps));
    }
    let plain = deobfuscate_with_steps(raw, pn, oracle_bv_e_page(pn, raw), steps);
    let zeros = plain[..PAGE_BODY_END].iter().filter(|&&b| b == 0).count();
    (zeros * 100 / PAGE_BODY_END >= 5).then_some(plain)
}

/// Expensive key recovery (brute-force search), as OpenQBW falls back to.
fn slow_plain(store: &PageStore, model: &ApModel, pn: u64, raw: &[u8]) -> Vec<u8> {
    let steps = sector_steps(raw);
    match recover_bv_brute_with_steps(pn, raw, &steps) {
        Some(bv) => deobfuscate_with_steps(raw, pn, bv, &steps),
        None => model.deobfuscate_with_store(raw, pn, store),
    }
}

/// Decodes every page at most once (OpenQBW's own iterators each re-decode
/// the whole file, re-running the brute-force key search every time):
///
/// 1. decode all data pages with the cheap key checks and read the catalog;
/// 2. brute-force only the leftover pages that belong to QuickBooks tables;
/// 3. run the header, line-item and text extractors on the decoded pages.
pub fn analyze_bytes(bytes: Vec<u8>) -> Result<Value, String> {
    let file_size = bytes.len();
    let store = PageStore::from_bytes(bytes).map_err(|e| {
        format!("This does not look like a QuickBooks company file ({e}).")
    })?;
    if store.superblock().is_err() {
        return Err("This is not a QuickBooks Desktop company file (.qbw), or it is from a \
                    version older than 2018 that this reader does not support."
            .into());
    }
    let n = store.page_count() as usize;
    let model = ApModel::learn(&store);

    // Step 1: cheap decode + catalog.
    let mut plain: Vec<Option<Vec<u8>>> = vec![None; n];
    let mut hard: Vec<u64> = Vec::new();
    let mut catalog: BTreeMap<(u32, String), SysTableEntry> = BTreeMap::new();
    for pn in 1..n as u64 {
        if pn % 128 == 0 {
            report(1, pn as usize, n);
        }
        let Ok(page) = store.page(pn) else { continue };
        if page.trailer().page_type() != PageType::Extent {
            continue;
        }
        match quick_plain(pn, page.bytes(), &sector_steps(page.bytes())) {
            Some(p) => {
                let mut found = Vec::new();
                scan::systable(&p, pn, &mut found);
                for e in found {
                    catalog.entry((e.table_id, e.name.clone())).or_insert(e);
                }
                plain[pn as usize] = Some(p);
            }
            None => hard.push(pn),
        }
    }
    let tables: Vec<SysTableEntry> = catalog.into_values().collect();
    if tables.is_empty() {
        return Err("The file opened, but no QuickBooks tables could be found in it. It may be \
                    damaged or use a format this reader does not support yet."
            .into());
    }
    let attribution = PageAttribution::from_catalog(tables.clone());

    // Step 2: brute-force only the pages we actually need.
    hard.retain(|&pn| attribution.attribute(pn).is_some_and(|e| !is_system_name(&e.name)));
    for (i, &pn) in hard.iter().enumerate() {
        if i % 8 == 0 {
            report(2, i, hard.len());
        }
        if let Ok(page) = store.page(pn) {
            plain[pn as usize] = Some(slow_plain(&store, &model, pn, page.bytes()));
        }
    }

    // Step 3: extract.
    let mut headers: Vec<TransactionHeader> = Vec::new();
    let mut items: Vec<LineItem> = Vec::new();
    let mut records: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
    let mut text_rows = 0usize;
    for (pn, p) in plain.iter().enumerate() {
        let Some(p) = p else { continue };
        let pn64 = pn as u64;
        if pn % 128 == 0 {
            report(3, pn, n);
        }
        let entry = attribution.attribute(pn64);

        if let Some(e) = entry
            && e.name.ends_with("_header")
            && scan::headers_slotted(p, pn64, &e.name, &mut headers).is_none()
        {
            scan::headers(&p[..PAGE_BODY_END], pn64, &e.name, &mut headers);
        }

        let before = items.len();
        scan::lineitems(&p[..LINEITEM_BODY_END], pn64, &mut items);
        if let Some(e) = entry {
            for li in &mut items[before..] {
                li.source_table = Some(e.name.clone());
            }
        }

        if let Some(e) = entry
            && !is_system_name(&e.name)
            && text_rows < MAX_TEXT_ROWS
        {
            let rows = page_text_rows(pn64, p);
            text_rows += rows.len();
            records.entry(e.name.clone()).or_default().extend(rows);
        }
    }
    records.retain(|_, rows| !rows.is_empty());
    headers.sort_by_key(|h| (h.page_number, h.page_offset));
    items.sort_by_key(|li| (li.page_number, li.page_offset));
    report(3, n, n);

    Ok(json!({
        "file_size": file_size,
        "pages": n,
        "tables": tables.iter().map(|t| json!({
            "id": t.table_id,
            "name": t.name,
            "cols": t.col_count,
            "system": is_system_name(&t.name),
        })).collect::<Vec<_>>(),
        "transactions": transactions_json(&headers, &items),
        "lines": lines_json(&items),
        "records": records,
    }))
}

/// Mirrors `openqbw export`: one row per header, with totals from its line
/// items; line items whose parent has no header get a synthesized row.
fn transactions_json(headers: &[TransactionHeader], items: &[LineItem]) -> Vec<Value> {
    let mut order: Vec<String> = Vec::new();
    let mut rows: HashMap<String, (String, String, Option<u32>, Option<u32>, i64, i64, bool)> =
        HashMap::new();
    for h in headers {
        if rows.contains_key(&h.qb_id) {
            continue;
        }
        order.push(h.qb_id.clone());
        rows.insert(
            h.qb_id.clone(),
            (h.txn_type().to_owned(), h.source_table.clone(), h.txn_date_raw, h.counter, 0, 0, false),
        );
    }
    for li in items {
        let row = rows.entry(li.invoice_id.clone()).or_insert_with(|| {
            order.push(li.invoice_id.clone());
            (
                "unknown".to_owned(),
                li.source_table.clone().unwrap_or_default(),
                li.txn_date_raw,
                li.counter,
                0,
                0,
                false,
            )
        });
        row.4 += 1;
        row.5 += li.amount_cents.map(|c| c as i64).unwrap_or(0);
        row.6 |= li.amount_type == openqbw::AmountType::Deferred;
    }
    order
        .into_iter()
        .map(|id| {
            let (ty, table, date, counter, lines, total, deferred) = &rows[&id];
            json!({
                "id": id, "type": ty, "table": table, "date_raw": date, "counter": counter,
                "lines": lines, "total": total, "deferred": deferred,
            })
        })
        .collect()
}

/// Compact line items: [parent_id, item_id, amount_type, cents, table].
fn lines_json(items: &[LineItem]) -> Vec<Value> {
    items
        .iter()
        .map(|li| {
            let cents = li
                .amount_cents_signed
                .map(|c| c as i64)
                .or(li.amount_cents.map(|c| c as i64));
            let kind = match li.amount_type {
                openqbw::AmountType::None => 0,
                openqbw::AmountType::OneByteOne => 1,
                openqbw::AmountType::Standard => 2,
                openqbw::AmountType::Deferred => 3,
                openqbw::AmountType::Other(b) => b as i64,
            };
            json!([li.invoice_id, li.item_qb_id, kind, cents, li.source_table])
        })
        .collect()
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
    if s.chars().count() < 3 || alnum < 2 || weird > 0 {
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
/// fall back to maximal printable runs.
fn extract_strings(row: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < row.len() {
        let len = row[i] as usize;
        if (3..=250).contains(&len) && i + 1 + len <= row.len() {
            let body = &row[i + 1..i + 1 + len];
            let ends_clean = row.get(i + 1 + len).is_none_or(|&b| !printable(b));
            if ends_clean && body.iter().all(|&b| printable(b)) {
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
            if let Some(s) = clean(&row[start..i]) {
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
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    /// QBW_BENCH=/path/file.qbw cargo test --release bench -- --ignored --nocapture
    #[test]
    #[ignore]
    fn phases() {
        let path = std::env::var("QBW_BENCH").expect("set QBW_BENCH");
        let t = Instant::now();
        let r = analyze_bytes(std::fs::read(path).unwrap());
        eprintln!("analyze      {:?} ok={}", t.elapsed(), r.is_ok());
        let store = PageStore::from_bytes(std::fs::read(std::env::var("QBW_BENCH").unwrap()).unwrap()).unwrap();
        let model = ApModel::learn(&store);
        let t = Instant::now();
        for pn in 1..201u64 {
            slow_plain(&store, &model, pn, store.page(pn).unwrap().bytes());
        }
        eprintln!("slow decode  {:?} per page", t.elapsed() / 200);
    }
}
