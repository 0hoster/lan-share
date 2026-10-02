#!/usr/bin/env python3
"""lan-share 吞吐量基准：对比单连接与多连接并行上传/下载。

用法:
    python3 scripts/bench.py [base_url] [size_mib]
"""
import concurrent.futures
import os
import sys
import time
import urllib.error
import urllib.request
import json

BASE = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18080").rstrip("/")
SIZE_MIB = int(sys.argv[2] if len(sys.argv) > 2 else 256)
LANES = [1, 4, 8]

for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, ValueError):
        pass


def request(path, method="GET", data=None, headers=None):
    req = urllib.request.Request(BASE + path, data=data, method=method)
    for key, value in (headers or {}).items():
        req.add_header(key, value)
    with urllib.request.urlopen(req, timeout=300) as resp:
        return resp.status, dict(resp.headers), resp.read()


def json_request(path, payload=None, method="POST"):
    data = json.dumps(payload).encode() if payload is not None else None
    headers = {"Content-Type": "application/json"} if payload is not None else None
    status, resp_headers, body = request(path, method=method, data=data, headers=headers)
    return status, resp_headers, json.loads(body) if body else None


def benchmark_upload(payload, lanes):
    status, _, init = json_request("/api/upload/init", {"name": f"bench-{lanes}.bin", "size": len(payload)})
    upload_id, chunk_size, count = init["upload_id"], init["chunk_size"], init["chunk_count"]

    def put(index):
        start = index * chunk_size
        end = min(len(payload), start + chunk_size)
        request(f"/api/upload/{upload_id}/chunk/{index}", "PUT", payload[start:end])

    started = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=lanes) as pool:
        list(pool.map(put, range(count)))
    status, _, meta = json_request(f"/api/upload/{upload_id}/complete")
    elapsed = time.perf_counter() - started
    assert status == 200, meta
    request(f"/api/files/{meta['id']}", "DELETE")
    return elapsed, len(payload) / elapsed / 1024 / 1024


def benchmark_download(file_id, total, lanes, chunk=4 * 1024 * 1024):
    count = max(1, (total + chunk - 1) // chunk)
    ranges = [(i * chunk, min(total, (i + 1) * chunk) - 1) for i in range(count)]
    started = time.perf_counter()

    def get(bounds):
        start, end = bounds
        return request(
            f"/api/files/{file_id}/download",
            headers={"Range": f"bytes={start}-{end}"},
        )[2]

    with concurrent.futures.ThreadPoolExecutor(max_workers=lanes) as pool:
        parts = list(pool.map(get, ranges))
    elapsed = time.perf_counter() - started
    return elapsed, sum(len(p) for p in parts) / elapsed / 1024 / 1024


def main():
    total = SIZE_MIB * 1024 * 1024
    print(f"\nlan-share 基准测试 · 数据量 {SIZE_MIB} MiB · 目标 {BASE}\n")
    payload = os.urandom(total)

    print("上传（分片并行）")
    upload_results = {}
    for lanes in LANES:
        elapsed, speed = benchmark_upload(payload, lanes)
        upload_results[lanes] = speed
        print(f"  并发 {lanes:>2}: {elapsed:6.2f}s  {speed:8.1f} MiB/s")

    # 重新上传一个文件用于下载测试
    status, _, init = json_request("/api/upload/init", {"name": "bench-download.bin", "size": total})
    upload_id, chunk_size, count = init["upload_id"], init["chunk_size"], init["chunk_count"]

    def put(index):
        start = index * chunk_size
        end = min(total, start + chunk_size)
        request(f"/api/upload/{upload_id}/chunk/{index}", "PUT", payload[start:end])

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        list(pool.map(put, range(count)))
    status, _, meta = json_request(f"/api/upload/{upload_id}/complete")
    file_id = meta["id"]

    print("\n下载（Range 并行）")
    for lanes in LANES:
        elapsed, speed = benchmark_download(file_id, total, lanes)
        print(f"  并发 {lanes:>2}: {elapsed:6.2f}s  {speed:8.1f} MiB/s")

    request(f"/api/files/{file_id}", "DELETE")
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
