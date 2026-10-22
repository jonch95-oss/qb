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
    LineItem, PageAttribution, TransactionHeader, deobfuscate_with_bv,
    iter_lineitems_with_attribution, iter_transaction_headers, recover_bv_qb_data,
};
use opensqlany::{ApModel, Page, PageStore, PageType, SlottedPage};
use serde_json::{Value, json};

const PAGE_BODY_END: usize = 0xFF0;
const MAX_TEXT_ROWS: usize = 400_000;

static mut RESULT: Vec<u8> = Vec::new();

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
    let model = ApModel::learn(&store);
    let tables = openqbw::collect_unique(&store, &model);
    if tables.is_empty() {
        return Err("The file opened, but no QuickBooks tables could be found in it. It may be \
                    damaged or use a format this reader does not support yet."
            .into());
    }
    let attribution = PageAttribution::build(&store, &model);

    let mut headers: Vec<TransactionHeader> =
        iter_transaction_headers(&store, &model, &attribution).collect();
    headers.sort_by_key(|h| (h.page_number, h.page_offset));
    let mut items: Vec<LineItem> =
        iter_lineitems_with_attribution(&store, &model, &attribution).collect();
    items.sort_by_key(|li| (li.page_number, li.page_offset));

    Ok(json!({
        "file_size": file_size,
        "pages": store.page_count(),
        "tables": tables.iter().map(|t| json!({
            "id": t.table_id,
            "name": t.name,
            "cols": t.col_count,
            "system": is_system_name(&t.name),
        })).collect::<Vec<_>>(),
        "transactions": transactions_json(&headers, &items),
        "lines": lines_json(&items),
        "records": text_records(&store, &model, &attribution),
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

/// For every data page owned by a QuickBooks table, split it into rows and
/// keep the readable strings of each row. Returns `{table: [[str, ...], ...]}`.
fn text_records(
    store: &PageStore,
    model: &ApModel,
    attribution: &PageAttribution,
) -> BTreeMap<String, Vec<Vec<String>>> {
    let mut out: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
    let mut total = 0usize;
    for pn in 0..store.page_count() {
        let Ok(page) = store.page(pn) else { continue };
        if page.trailer().page_type() != PageType::Extent {
            continue;
        }
        let Some(entry) = attribution.attribute(pn) else { continue };
        if is_system_name(&entry.name) {
            continue;
        }
        let raw = page.bytes();
        let plain = match recover_bv_qb_data(pn, raw) {
            Some(bv) => deobfuscate_with_bv(raw, pn, bv),
            None => model.deobfuscate_with_store(raw, pn, store),
        };
        let slotted = SlottedPage::parse(Page::from_bytes(pn, &plain));
        let mut chunks: Vec<&[u8]> = slotted.row_bytes().into_iter().map(|(_, r)| r).collect();
        if chunks.is_empty() {
            chunks = split_on_zero_runs(&plain[..PAGE_BODY_END]);
        }
        let bucket = out.entry(entry.name.clone()).or_default();
        for chunk in chunks {
            let strings = extract_strings(chunk);
            if strings.is_empty() {
                continue;
            }
            bucket.push(strings);
            total += 1;
            if total >= MAX_TEXT_ROWS {
                return out;
            }
        }
    }
    out.retain(|_, rows| !rows.is_empty());
    out
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
