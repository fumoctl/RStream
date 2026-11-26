#!/usr/bin/env python3
"""Append a zlib-compressed bundle + RSTRSFX1 trailer onto rstream-portable-sfx.exe."""

from __future__ import annotations

import hashlib
import os
import struct
import sys
import zlib
from pathlib import Path

MAGIC = b"RSTRSFX1"
TRAILER_STRUCT = struct.Struct("<Q32s8s")


def pack(stub_path: Path, out_path: Path, files: list[Path]) -> None:
    if not stub_path.is_file():
        raise FileNotFoundError(f"Stub executable not found: {stub_path}")
    if not files:
        raise ValueError("At least one payload file is required")

    repo_root = Path(__file__).resolve().parent.parent
    existing_names = {p.name for p in files}
    files = list(files)
    for extra_name in ("LICENSE", "THIRD_PARTY_LICENSES.md"):
        extra_path = repo_root / extra_name
        if extra_path.is_file() and extra_name not in existing_names:
            files.append(extra_path)
            existing_names.add(extra_name)

    compressor = zlib.compressobj(level=6)
    hasher = hashlib.sha256()
    payload_len = 0

    out_path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = out_path.with_suffix(out_path.suffix + ".tmp")

    with stub_path.open("rb") as stub_f, tmp_path.open("wb") as out_f:
        while chunk := stub_f.read(1024 * 1024):
            out_f.write(chunk)

        def write_compressed(raw: bytes) -> None:
            nonlocal payload_len
            comp = compressor.compress(raw)
            if comp:
                out_f.write(comp)
                hasher.update(comp)
                payload_len += len(comp)

        write_compressed(struct.pack("<I", len(files)))
        for fpath in files:
            if not fpath.is_file():
                raise FileNotFoundError(f"Payload file missing: {fpath}")
            name_bytes = fpath.name.encode("utf-8")
            size = fpath.stat().st_size
            write_compressed(struct.pack("<I", len(name_bytes)) + name_bytes + struct.pack("<Q", size))
            with fpath.open("rb") as pf:
                while block := pf.read(1024 * 1024):
                    write_compressed(block)

        tail = compressor.flush()
        if tail:
            out_f.write(tail)
            hasher.update(tail)
            payload_len += len(tail)

        digest = hasher.digest()
        out_f.write(TRAILER_STRUCT.pack(payload_len, digest, MAGIC))

    os.chmod(tmp_path, 0o755)
    tmp_path.replace(out_path)

    # Self-check written trailer & digest.
    total = out_path.stat().st_size
    with out_path.open("rb") as vf:
        vf.seek(total - TRAILER_STRUCT.size)
        rec_len, rec_sha, rec_magic = TRAILER_STRUCT.unpack(vf.read(TRAILER_STRUCT.size))
        assert rec_magic == MAGIC, "trailer magic check failed"
        assert rec_len == payload_len, "trailer length check failed"
        vf.seek(total - TRAILER_STRUCT.size - rec_len)
        check_hasher = hashlib.sha256()
        rem = rec_len
        while rem > 0:
            buf = vf.read(min(1024 * 1024, rem))
            assert buf, "unexpected EOF verifying portable exe"
            check_hasher.update(buf)
            rem -= len(buf)
        assert check_hasher.digest() == rec_sha, "trailer SHA-256 verification failed"

    print(
        f"Packed {len(files)} files into {out_path} "
        f"({total / (1024 * 1024):.2f} MiB, sha256={digest.hex()[:16]})"
    )


def main(argv: list[str]) -> int:
    if len(argv) < 4:
        print(
            f"Usage: {argv[0]} <stub.exe> <out.exe> <rstream.exe> [dlls...]",
            file=sys.stderr,
        )
        return 1
    pack(Path(argv[1]), Path(argv[2]), [Path(p) for p in argv[3:]])
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
