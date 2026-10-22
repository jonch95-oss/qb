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
//! group names into lists. OpenQBW's own table map and transaction decoding
//! do not match newer company files, so they are not used here.

use std::collections::HashMap;


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
    let file_size = bytes.len();
    let store = PageStore::from_bytes(bytes)
        .map_err(|e| format!("This does not look like a QuickBooks company file ({e})."))?;
    if store.superblock().is_err() {
        return Err("This is not a QuickBooks Desktop company file (.qbw), or it is from a \
                    version older than 2018 that this reader does not support."
            .into());
    }
    let n = store.page_count() as usize;

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
        }
    }

    // Step 2: read lists and collect readable text. Transactions store
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
    }))
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
        let r = analyze_bytes(std::fs::read(path).unwrap());
        eprintln!("analyze {:?} ok={}", t.elapsed(), r.is_ok());
        if let (Ok(v), Ok(out)) = (r, std::env::var("QBW_OUT")) {
            std::fs::write(out, serde_json::to_vec(&v).unwrap()).unwrap();
        }
    }
}
