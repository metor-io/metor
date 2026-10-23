"""Resolves a built catalog by name to a local file checked by sha256. Stdlib only."""

from __future__ import annotations

import hashlib
import os
import tomllib
import urllib.parse
import urllib.request
from pathlib import Path

CATALOGS = Path(__file__).with_name("catalogs.toml")
PLACEHOLDER_TLD = ".invalid"


class EnsureError(Exception):
    pass


def cache_root() -> Path:
    base = os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache"
    return Path(base) / "metor" / "starcat"


def load(catalogs: Path = CATALOGS) -> dict[str, dict]:
    with open(catalogs, "rb") as f:
        return tomllib.load(f)


def ensure(name: str, catalogs: Path = CATALOGS, cache: Path | None = None) -> Path:
    """The checked local path of catalog `name`. Downloads it if it is not cached.

    It never builds a catalog.
    """
    entries = load(catalogs)
    if name not in entries:
        raise EnsureError(f"unknown catalog {name!r}; known: {sorted(entries)}")
    entry = entries[name]
    if "path" in entry:
        path = (catalogs.parent / entry["path"]).resolve()
    else:
        path = (cache or cache_root()) / f"starcat-{name}-{entry['sha256'][:16]}.bin"
        if not path.exists():
            download(entry, path)
    check(path, entry)
    return path


def download(entry: dict, path: Path) -> None:
    url = entry["url"]
    host = urllib.parse.urlparse(url).hostname or ""
    if host.endswith(PLACEHOLDER_TLD):
        raise EnsureError(f"{url} is a placeholder: the file is not hosted yet; place it at {path}")
    path.parent.mkdir(parents=True, exist_ok=True)
    partial = path.with_suffix(".partial")
    urllib.request.urlretrieve(url, partial)
    try:
        check(partial, entry)
    except EnsureError:
        partial.unlink()
        raise
    partial.rename(path)


def check(path: Path, entry: dict) -> None:
    if not path.is_file():
        raise EnsureError(f"{path}: missing")
    digest = sha256_file(path)
    if digest != entry["sha256"]:
        raise EnsureError(f"{path}: sha256 {digest} does not match expected {entry['sha256']}")
    size = path.stat().st_size
    if size != entry["bytes"]:
        raise EnsureError(f"{path}: {size} B, expected {entry['bytes']} B")


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()
