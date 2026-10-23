from __future__ import annotations

import hashlib
from pathlib import Path

import pytest
from starcat import fmt
from starcat.ensure import EnsureError, ensure, load


def write_catalogs(tmp_path: Path, body: str) -> Path:
    path = tmp_path / "catalogs.toml"
    path.write_text(body)
    return path


def hosted(tmp_path: Path, payload: bytes, sha: str | None = None) -> Path:
    src = tmp_path / "hosted.bin"
    src.write_bytes(payload)
    digest = sha or hashlib.sha256(payload).hexdigest()
    return write_catalogs(
        tmp_path,
        f'[x]\nmag_limit = 9.0\nurl = "{src.as_uri()}"\nsha256 = "{digest}"\nbytes = {len(payload)}\n',
    )


def test_g8_resolves_to_committed_file():
    path = ensure("g8")
    assert path.name == "starcat-g8.bin"
    cat = fmt.read(path)
    assert cat.mag_limit == 8.0 and cat.nside == 16


def test_known_catalogs():
    entries = load()
    assert {"g8", "g12"} <= entries.keys()
    assert entries["g12"]["sha256"] != entries["g8"]["sha256"]


def test_downloads_into_cache(tmp_path):
    catalogs = hosted(tmp_path, b"stars")
    cache = tmp_path / "cache"
    path = ensure("x", catalogs, cache)
    assert path.parent == cache and path.read_bytes() == b"stars"
    assert not list(cache.glob("*.partial"))


def test_uses_cached_file_without_download(tmp_path):
    catalogs = hosted(tmp_path, b"stars")
    cache = tmp_path / "cache"
    first = ensure("x", catalogs, cache)
    (tmp_path / "hosted.bin").unlink()
    assert ensure("x", catalogs, cache) == first


def test_rejects_sha_mismatch_and_names_both(tmp_path):
    catalogs = hosted(tmp_path, b"stars", sha="0" * 64)
    cache = tmp_path / "cache"
    actual = hashlib.sha256(b"stars").hexdigest()
    with pytest.raises(EnsureError, match=f"{actual}.*{'0' * 64}"):
        ensure("x", catalogs, cache)
    assert not list(cache.iterdir())


def test_rejects_corrupt_cached_file(tmp_path):
    catalogs = hosted(tmp_path, b"stars")
    cache = tmp_path / "cache"
    path = ensure("x", catalogs, cache)
    path.write_bytes(b"starz")
    with pytest.raises(EnsureError, match="sha256"):
        ensure("x", catalogs, cache)


def test_rejects_committed_file_mismatch(tmp_path):
    (tmp_path / "local.bin").write_bytes(b"abc")
    catalogs = write_catalogs(
        tmp_path, f'[y]\npath = "local.bin"\nsha256 = "{"1" * 64}"\nbytes = 3\n'
    )
    with pytest.raises(EnsureError, match="does not match"):
        ensure("y", catalogs)


def test_rejects_placeholder_url(tmp_path):
    catalogs = write_catalogs(
        tmp_path,
        '[z]\nurl = "https://hosting-tbd.invalid/z.bin"\nsha256 = "ab"\nbytes = 1\n',
    )
    with pytest.raises(EnsureError, match="placeholder"):
        ensure("z", catalogs, tmp_path / "cache")


def test_g12_placeholder_until_hosted(tmp_path):
    with pytest.raises(EnsureError, match="placeholder"):
        ensure("g12", cache=tmp_path)


def test_rejects_unknown_name():
    with pytest.raises(EnsureError, match="unknown catalog"):
        ensure("g99")
