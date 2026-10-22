Vendored copy of OpenQBW crates/openqbw at
https://github.com/Sigilweaver/OpenQBW commit e11017e (Apache-2.0, see LICENSE/NOTICE).

Local changes for the QBW viewer:
- standalone Cargo.toml (no workspace inheritance)
- per-page scanners made public and re-exported as `openqbw::scan`
- `bv_recovery.rs`: faster `recover_step` and one-pass anchor search in
  `recover_bv_qb_data` (same results, see unit tests)
- `bv_recovery.rs`: `sector_steps` + `*_with_steps` variants so a page's AP steps
  are computed once (same results, see `viewer_patch_equivalence` tests)
