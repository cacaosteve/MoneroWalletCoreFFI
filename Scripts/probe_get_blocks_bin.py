#!/usr/bin/env python3
"""
probe_get_blocks_bin.py

Fetch and probe Monero daemon /get_blocks.bin responses (binary EPEE portable-storage).

Goal: give you quick visibility into whether the daemon is returning substantial binary payloads
and help you bisect/sanity-check around a restore height without needing to know exact tx heights.

This script intentionally does NOT attempt to fully decode EPEE. It:
  - builds a /get_blocks.bin request body matching what monero-daemon-rpc uses:
      { prune: true, start_height: <u64>, max_block_count: <u64> }
  - downloads the raw binary response
  - prints sizes and timing
  - searches for a few common ASCII field names inside the binary payload (heuristic):
      "blocks", "txs", "blob", "block", "output_indices", "status", "OK"
    This can quickly confirm you're getting real get_blocks.bin-like responses and not errors.

Usage examples:
  # Probe 25 blocks starting at restore height
  python3 probe_get_blocks_bin.py --node http://mini.nexatrode.com:18089 --start 3519450 --count 25

  # Probe multiple windows stepping forward
  python3 probe_get_blocks_bin.py --node http://mini.nexatrode.com:18089 --start 3519450 --count 25 --step 250 --windows 10

  # Save response to a file for later offline inspection
  python3 probe_get_blocks_bin.py --node http://mini.nexatrode.com:18089 --start 3519450 --count 25 --out /tmp/get_blocks_resp.bin

Exit codes:
  0 on success
  2 on argument errors
  3 on network/protocol errors
"""

from __future__ import annotations

import argparse
import struct
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Dict, List, Optional, Tuple

# NOTE:
# Monero's /get_blocks.bin does NOT accept the simple EPEE portable-storage encoding
# we originally attempted here. Walletcore's upstream BIN request framing begins with:
#
#   01 11 01 01 01 01 02 01 01 0c ...
#
# So for this probe we match walletcore by hardcoding the exact request prefix/suffix
# around the two u64 values we need to vary (start_height and max_block_count).
#
# Source (captured from walletcore telemetry):
# 🛰️  daemon_bin_call req_hex_prefix route=get_blocks.bin prefix_len=65 hex=
# 01 11 01 01 01 01 02 01 01 0c
# 05 70 72 75 6e 65 0b 01
# 0c 73 74 61 72 74 5f 68 65 69 67 68 74 05 <u64 LE>
# 0f 6d 61 78 5f 62 6c 6f 63 6b 5f 63 6f 75 6e 74 05 <u64 LE>
GET_BLOCKS_BIN_PREFIX = bytes.fromhex(
    "01 11 01 01 01 01 02 01 01 0c "
    "05 70 72 75 6e 65 0b 01 "
    "0c 73 74 61 72 74 5f 68 65 69 67 68 74 05"
)
GET_BLOCKS_BIN_MID = bytes.fromhex("0f 6d 61 78 5f 62 6c 6f 63 6b 5f 63 6f 75 6e 74 05")

# Heuristic markers to search in the response bytes
NEEDLE_KEYS = [
    b"status",
    b"OK",
    b"blocks",
    b"block",
    b"txs",
    b"blob",
    b"output_indices",
    b"indices",
    b"start_height",
    b"current_height",
    b"error",
]


def epee_key(key: str) -> bytes:
    kb = key.encode("utf-8")
    if len(kb) > 255:
        raise ValueError(f"key too long for epee (len={len(kb)}): {key}")
    return bytes([len(kb)]) + kb


def build_get_blocks_bin_request(
    start_height: int, max_block_count: int, prune: bool = True
) -> bytes:
    """
    Build request body for /get_blocks.bin matching walletcore's request framing.

    We only support prune=True here because the captured walletcore framing hardcodes prune=true.
    """
    if start_height < 0:
        raise ValueError("start_height must be >= 0")
    if max_block_count <= 0:
        raise ValueError("max_block_count must be > 0")
    if start_height > 2**64 - 1:
        raise ValueError("start_height must fit in u64")
    if max_block_count > 2**64 - 1:
        raise ValueError("max_block_count must fit in u64")
    if prune is not True:
        raise ValueError(
            "this probe currently only supports prune=true (walletcore framing)"
        )

    # Exact framing:
    #   PREFIX + <start_height:u64le> + MID + <max_block_count:u64le>
    return (
        GET_BLOCKS_BIN_PREFIX
        + struct.pack("<Q", start_height)
        + GET_BLOCKS_BIN_MID
        + struct.pack("<Q", max_block_count)
    )


@dataclass
class FetchResult:
    start: int
    count: int
    http_ms: int
    status: int
    resp_bytes: int
    saved_path: Optional[str]
    needles_found: Dict[bytes, int]


def http_post_bytes(
    url: str, body: bytes, timeout_s: float = 30.0
) -> Tuple[int, bytes]:
    req = urllib.request.Request(
        url=url,
        data=body,
        method="POST",
        headers={
            "Content-Type": "application/octet-stream",
            "Accept": "*/*",
            "User-Agent": "probe_get_blocks_bin.py",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            status = getattr(resp, "status", 200)
            data = resp.read()
            return int(status), data
    except urllib.error.HTTPError as e:
        # HTTPError is also a response; read its body for debugging
        try:
            data = e.read()
        except Exception:
            data = b""
        return int(getattr(e, "code", 0) or 0), data
    except Exception as e:
        raise RuntimeError(f"network error: {e}") from e


def find_needles(data: bytes, needles: List[bytes]) -> Dict[bytes, int]:
    found: Dict[bytes, int] = {}
    for n in needles:
        idx = data.find(n)
        if idx >= 0:
            found[n] = idx
    return found


def fmt_bytes(n: int) -> str:
    if n < 1024:
        return f"{n} B"
    if n < 1024 * 1024:
        return f"{n / 1024.0:.1f} KiB"
    return f"{n / (1024.0 * 1024.0):.2f} MiB"


def do_probe(
    node: str, start: int, count: int, timeout_s: float, out_path: Optional[str]
) -> FetchResult:
    url = node.rstrip("/") + "/get_blocks.bin"
    body = build_get_blocks_bin_request(
        start_height=start, max_block_count=count, prune=True
    )

    t0 = time.time()
    status, resp = http_post_bytes(url, body, timeout_s=timeout_s)
    http_ms = int((time.time() - t0) * 1000)

    needles = find_needles(resp, NEEDLE_KEYS)

    saved = None
    if out_path:
        # If multiple windows are used, caller should pass a format string with {start}
        path = out_path.format(start=start, count=count)
        with open(path, "wb") as f:
            f.write(resp)
        saved = path

    return FetchResult(
        start=start,
        count=count,
        http_ms=http_ms,
        status=status,
        resp_bytes=len(resp),
        saved_path=saved,
        needles_found=needles,
    )


def parse_args(argv: List[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Probe monerod /get_blocks.bin responses (binary)."
    )
    p.add_argument(
        "--node",
        required=True,
        help="Monero daemon base URL, e.g. http://mini.nexatrode.com:18089",
    )
    p.add_argument("--start", type=int, required=True, help="Start height (u64)")
    p.add_argument(
        "--count", type=int, default=25, help="Max block count to request (default: 25)"
    )
    p.add_argument(
        "--timeout", type=float, default=30.0, help="HTTP timeout seconds (default: 30)"
    )
    p.add_argument(
        "--windows", type=int, default=1, help="How many probes to run (default: 1)"
    )
    p.add_argument(
        "--step",
        type=int,
        default=0,
        help="Advance start height by this per window (default: 0)",
    )
    p.add_argument(
        "--out",
        default=None,
        help=(
            "Optional output path to save responses. If --windows>1 you can use "
            "placeholders like /tmp/get_blocks_{start}_{count}.bin"
        ),
    )
    return p.parse_args(argv)


def main(argv: List[str]) -> int:
    args = parse_args(argv)

    if args.windows <= 0:
        print("error: --windows must be > 0", file=sys.stderr)
        return 2
    if args.count <= 0:
        print("error: --count must be > 0", file=sys.stderr)
        return 2
    if args.start < 0:
        print("error: --start must be >= 0", file=sys.stderr)
        return 2
    if args.step < 0:
        print("error: --step must be >= 0", file=sys.stderr)
        return 2

    print(f"node={args.node}")
    print(f"endpoint={args.node.rstrip('/')}/get_blocks.bin")
    print(
        f"count={args.count} windows={args.windows} step={args.step} timeout_s={args.timeout}"
    )
    if args.out:
        print(f"out={args.out} (supports {{start}} and {{count}} placeholders)")

    start = args.start
    for i in range(args.windows):
        try:
            r = do_probe(
                node=args.node,
                start=start,
                count=args.count,
                timeout_s=args.timeout,
                out_path=args.out,
            )
        except Exception as e:
            print(f"[{i + 1}/{args.windows}] start={start} ERROR: {e}", file=sys.stderr)
            return 3

        needles_summary = ", ".join(
            [
                f"{k.decode('utf-8', 'replace')}@{v}"
                for k, v in sorted(r.needles_found.items(), key=lambda kv: kv[1])
            ]
        )
        if not needles_summary:
            needles_summary = "(none found)"

        print(
            f"[{i + 1}/{args.windows}] start={r.start} count={r.count} "
            f"status={r.status} http_ms={r.http_ms} resp={fmt_bytes(r.resp_bytes)} "
            f"needles={needles_summary}"
            + (f" saved={r.saved_path}" if r.saved_path else "")
        )

        start += args.step

    print("done")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
