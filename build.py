#!/usr/bin/env python3
"""Rebuild "QBW Viewer.html": compile the reader to WebAssembly and inline it.

Needs Rust with the wasm32 target:  rustup target add wasm32-unknown-unknown
"""
import base64
import subprocess
from pathlib import Path

HERE = Path(__file__).resolve().parent
subprocess.run(
    ["cargo", "build", "--release", "--target", "wasm32-unknown-unknown"],
    cwd=HERE / "wasm",
    check=True,
)
wasm = HERE / "wasm/target/wasm32-unknown-unknown/release/qbw_wasm.wasm"
template = (HERE / "viewer/app.html").read_text(encoding="utf-8")
out = HERE / "QBW Viewer.html"
out.write_text(
    template.replace("__WASM_BASE64__", base64.b64encode(wasm.read_bytes()).decode()),
    encoding="utf-8",
)
print(f"Wrote {out} ({out.stat().st_size // 1024} KB)")
