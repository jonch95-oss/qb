#!/usr/bin/env python3
"""Open and browse a QuickBooks Desktop .qbw file without QuickBooks.

Usage:
    python qbw_viewer.py "C:\\path\\to\\Company.qbw"
    python qbw_viewer.py Company.qbw --no-browser      # just write the .sqlite
    python qbw_viewer.py --view existing.sqlite         # re-open a past export

What it does:
  1. Copies the .qbw to a temp file (the original is never opened for writing).
  2. Runs the open-source OpenQBW parser to export transactions and line
     items into a SQLite database next to this script (./exports/).
  3. Starts a local-only web server (127.0.0.1) and opens the viewer in
     your browser. Nothing leaves your machine.

OpenQBW (https://github.com/Sigilweaver/OpenQBW, Apache-2.0) is located via,
in order: --openqbw PATH, the OPENQBW env var, ./vendor/OpenQBW build output,
or `openqbw` on PATH. If none is found, the script offers to build it
(requires git and Rust/cargo: https://rustup.rs).

Standard library only; Python 3.9+.
"""

from __future__ import annotations

import argparse
import functools
import http.server
import os
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading
import urllib.parse
import webbrowser
from pathlib import Path

HERE = Path(__file__).resolve().parent
VIEWER_DIR = HERE / "viewer"
VENDOR_DIR = HERE / "vendor"
EXPORT_DIR = HERE / "exports"
EXE = "openqbw.exe" if os.name == "nt" else "openqbw"

REPOS = {
    "OpenQBW": "https://github.com/Sigilweaver/OpenQBW",
    # OpenQBW depends on this via a relative path, so it must be a sibling.
    "OpenSQLAnywhere": "https://github.com/Sigilweaver/OpenSQLAnywhere",
}


def find_openqbw(explicit: str | None) -> Path | None:
    candidates = []
    if explicit:
        candidates.append(Path(explicit))
    if os.environ.get("OPENQBW"):
        candidates.append(Path(os.environ["OPENQBW"]))
    candidates.append(VENDOR_DIR / "OpenQBW" / "target" / "release" / EXE)
    on_path = shutil.which("openqbw")
    if on_path:
        candidates.append(Path(on_path))
    for c in candidates:
        if c.is_file():
            return c
    return None


def build_openqbw() -> Path:
    for tool in ("git", "cargo"):
        if not shutil.which(tool):
            sys.exit(
                f"'{tool}' was not found. Install git and Rust (https://rustup.rs), "
                "then run this script again."
            )
    VENDOR_DIR.mkdir(exist_ok=True)
    for name, url in REPOS.items():
        dest = VENDOR_DIR / name
        if not dest.exists():
            print(f"Cloning {url} ...")
            subprocess.run(["git", "clone", "--depth", "1", url, str(dest)], check=True)
    print("Building OpenQBW (first run only, takes a minute or two) ...")
    subprocess.run(
        ["cargo", "build", "--release", "-p", "openqbw-cli"],
        cwd=VENDOR_DIR / "OpenQBW",
        check=True,
    )
    exe = VENDOR_DIR / "OpenQBW" / "target" / "release" / EXE
    if not exe.is_file():
        sys.exit(f"Build finished but {exe} was not produced.")
    return exe


def ensure_openqbw(explicit: str | None, assume_yes: bool) -> Path:
    exe = find_openqbw(explicit)
    if exe:
        return exe
    print("The OpenQBW parser is not installed yet.")
    if not assume_yes:
        answer = input("Download and build it into ./vendor now? [Y/n] ").strip().lower()
        if answer not in ("", "y", "yes"):
            sys.exit("Cannot read .qbw files without OpenQBW. Exiting.")
    return build_openqbw()


def export_qbw(exe: Path, qbw: Path, out: Path) -> None:
    out.parent.mkdir(parents=True, exist_ok=True)
    if out.exists():
        out.unlink()
    # Work on a copy so the original is never touched (and so a file that
    # QuickBooks currently has open can still be read).
    with tempfile.TemporaryDirectory() as tmp:
        copy = Path(tmp) / "company.qbw"
        print(f"Copying {qbw.name} to a temporary location ...")
        shutil.copyfile(qbw, copy)
        print("Decoding with OpenQBW ...")
        result = subprocess.run([str(exe), "export", str(copy), str(out)])
    if result.returncode != 0 or not out.exists():
        sys.exit(
            "OpenQBW could not export this file. Common causes: the file is from a "
            "QuickBooks version OpenQBW does not support yet (it is tested on 2018 and "
            "later), or the company file uses a password/strong encryption."
        )


def serve(db: Path, port: int, open_browser: bool) -> None:
    class Handler(http.server.SimpleHTTPRequestHandler):
        def translate_path(self, path):
            route = urllib.parse.urlparse(path).path
            if route == "/data.sqlite":
                return str(db)
            return super().translate_path(path)

        def log_message(self, *args):
            pass

    handler = functools.partial(Handler, directory=str(VIEWER_DIR))
    with socketserver.ThreadingTCPServer(("127.0.0.1", port), handler) as httpd:
        httpd.daemon_threads = True
        name = urllib.parse.quote(db.name)
        url = f"http://127.0.0.1:{httpd.server_address[1]}/?db=data.sqlite&name={name}"
        print(f"\nViewer running at {url}\nPress Ctrl+C to stop.")
        if open_browser:
            threading.Timer(0.5, webbrowser.open, args=(url,)).start()
        try:
            httpd.serve_forever()
        except KeyboardInterrupt:
            print("\nStopped.")


def main() -> None:
    p = argparse.ArgumentParser(description="View a QuickBooks .qbw file without QuickBooks.")
    p.add_argument("qbw", nargs="?", help="path to the .qbw company file")
    p.add_argument("--view", metavar="SQLITE", help="open an existing export instead")
    p.add_argument("--out", help="where to write the .sqlite export")
    p.add_argument("--openqbw", help="path to an openqbw executable")
    p.add_argument("--port", type=int, default=0, help="local port (default: any free port)")
    p.add_argument("--no-browser", action="store_true", help="export only; don't start the viewer")
    p.add_argument("-y", "--yes", action="store_true", help="build OpenQBW without asking")
    args = p.parse_args()

    if args.view:
        db = Path(args.view).expanduser().resolve()
        if not db.is_file():
            sys.exit(f"Not found: {db}")
    elif args.qbw:
        qbw = Path(args.qbw).expanduser().resolve()
        if not qbw.is_file():
            sys.exit(f"Not found: {qbw}")
        exe = ensure_openqbw(args.openqbw, args.yes)
        db = Path(args.out).resolve() if args.out else EXPORT_DIR / f"{qbw.stem}.sqlite"
        export_qbw(exe, qbw, db)
        print(f"Export written to {db}")
    else:
        p.print_help()
        sys.exit(1)

    if not args.no_browser:
        serve(db, args.port, open_browser=True)


if __name__ == "__main__":
    main()
