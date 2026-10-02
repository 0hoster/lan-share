#!/usr/bin/env python3
"""lan-share 端到端测试。

覆盖：静态资源、鉴权、分片并行上传、完成校验、文件列表、Range 并行下载、
416 处理、边界分片校验、删除、0 字节文件、统计接口。

用法:
    python3 scripts/e2e_test.py [base_url] [token]
"""
import concurrent.futures
import hashlib
import json
import os
import socket
import sys
import urllib.error
import urllib.request
from urllib.parse import urlparse

BASE = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18080").rstrip("/")
TOKEN = sys.argv[2] if len(sys.argv) > 2 else ""
PARALLEL = 4

passed = 0
failed = []


def check(name, condition, detail=""):
    global passed
    if condition:
        passed += 1
        print(f"  \033[32mPASS\033[0m {name}")
    else:
        failed.append(name)
        print(f"  \033[31mFAIL\033[0m {name} -> {detail}")


def url(path):
    if not TOKEN:
        return BASE + path
    sep = "&" if "?" in path else "?"
    return f"{BASE}{path}{sep}token={TOKEN}"


def normalize_headers(message):
    """HTTP 头大小写不敏感，hyper 发出的就是全小写，统一规范化后再断言。"""
    return {key.lower(): value for key, value in message.items()}


def request(path, method="GET", data=None, headers=None):
    req = urllib.request.Request(url(path), data=data, method=method)
    if TOKEN:
        req.add_header("Authorization", f"Bearer {TOKEN}")
    for key, value in (headers or {}).items():
        req.add_header(key, value)
    try:
        with urllib.request.urlopen(req, timeout=90) as resp:
            return resp.status, normalize_headers(resp.headers), resp.read()
    except urllib.error.HTTPError as err:
        return err.code, normalize_headers(err.headers), err.read()


def json_request(path, method="GET", payload=None):
    data = json.dumps(payload).encode() if payload is not None else None
    headers = {"Content-Type": "application/json"} if payload is not None else None
    status, resp_headers, body = request(path, method=method, data=data, headers=headers)
    parsed = json.loads(body) if body else None
    return status, resp_headers, parsed


def upload_file(name, payload, concurrency=PARALLEL):
    status, _, init = json_request("/api/upload/init", "POST", {"name": name, "size": len(payload)})
    assert status == 200, f"init failed: {status} {init}"
    upload_id = init["upload_id"]
    chunk_size = init["chunk_size"]

    def put(index):
        start = index * chunk_size
        end = min(len(payload), start + chunk_size)
        return request(f"/api/upload/{upload_id}/chunk/{index}", "PUT", payload[start:end])

    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        results = list(pool.map(put, range(init["chunk_count"])))
    for status, _, body in results:
        assert status == 200, f"chunk failed: {status} {body!r}"

    status, _, meta = json_request(f"/api/upload/{upload_id}/complete", "POST")
    assert status == 200, f"complete failed: {status} {meta}"
    return upload_id, meta


def download_parallel(file_id, total, concurrency=PARALLEL, chunk=512 * 1024):
    count = max(1, (total + chunk - 1) // chunk)
    parts = [None] * count

    def get(index):
        start = index * chunk
        end = min(total, start + chunk) - 1
        status, headers, body = request(
            f"/api/files/{file_id}/download", headers={"Range": f"bytes={start}-{end}"}
        )
        assert status == 206, f"range failed: {status}"
        expected = f"bytes {start}-{end}/{total}"
        assert headers.get("content-range") == expected, headers.get("content-range")
        return index, body

    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        for index, body in pool.map(get, range(count)):
            parts[index] = body
    return b"".join(parts)


def test_static():
    status, headers, body = request("/")
    check("home page returns 200", status == 200, status)
    check("home page is html", "text/html" in headers.get("content-type", ""))
    check("home page renders Chinese title", "局域网互传" in body.decode("utf-8", "ignore"))
    status, _, body = request("/app.js")
    check("app.js is served", status == 200 and b"uploadChunk" in body, status)
    status, _, _ = request("/styles.css")
    check("styles.css is served", status == 200, status)


def test_auth_required():
    req = urllib.request.Request(BASE + "/api/files")
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            status = resp.status
    except urllib.error.HTTPError as err:
        status = err.code
    check("api without token returns 401", status == 401, status)


def test_websocket():
    parsed = urlparse(BASE)
    host = parsed.hostname or "127.0.0.1"
    port = parsed.port or 80
    path = "/ws" + (f"?token={TOKEN}" if TOKEN else "")
    key = base64_key()
    try:
        conn = socket.create_connection((host, port), timeout=15)
    except OSError as err:
        check("websocket upgrade succeeds", False, err)
        return
    handshake = (
        f"GET {path} HTTP/1.1\r\n"
        f"Host: {host}:{port}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n\r\n"
    )
    conn.sendall(handshake.encode())
    head = conn.recv(2048).decode("latin1")
    conn.close()
    status_line = head.split("\r\n")[0]
    check("websocket upgrade succeeds", "101" in status_line, status_line)


def base64_key():
    import base64
    return base64.b64encode(os.urandom(16)).decode()


def test_upload_download():
    payload = os.urandom(3 * 1024 * 1024 + 12345)
    digest = hashlib.sha256(payload).hexdigest()
    file_id, meta = upload_file("测试 文件.bin", payload)
    check("parallel chunk upload completed", meta["size"] == len(payload), meta)
    check("original file name preserved", meta["name"] == "测试 文件.bin", meta.get("name"))

    status, _, files = json_request("/api/files")
    check("file appears in listing", status == 200 and any(f["id"] == file_id for f in files))

    status, headers, _ = request(f"/api/files/{file_id}/download", method="HEAD")
    check("HEAD reports size", status == 200 and headers.get("content-length") == str(len(payload)))
    check("Accept-Ranges advertised", headers.get("accept-ranges") == "bytes")
    check("UTF-8 filename in header", "filename*=UTF-8''" in headers.get("content-disposition", ""))

    status, _, body = request(f"/api/files/{file_id}/download")
    check("full download matches", hashlib.sha256(body).hexdigest() == digest, len(body))

    merged = download_parallel(file_id, len(payload))
    check("parallel range download matches", hashlib.sha256(merged).hexdigest() == digest, len(merged))

    status, _, body = request(f"/api/files/{file_id}/download", headers={"Range": "bytes=-1000"})
    check("suffix range works", status == 206 and body == payload[-1000:], status)

    status, _, _ = request(
        f"/api/files/{file_id}/download", headers={"Range": f"bytes={len(payload)}-"}
    )
    check("out of range returns 416", status == 416, status)

    return file_id


def test_edge_cases():
    # 乱序分片：验证多连接乱序写入时偏移正确
    payload = os.urandom(20 * 1024 * 1024 + 7)
    digest = hashlib.sha256(payload).hexdigest()
    status, _, init = json_request(
        "/api/upload/init", "POST", {"name": "out-of-order.bin", "size": len(payload)}
    )
    upload_id, chunk_size, count = init["upload_id"], init["chunk_size"], init["chunk_count"]
    check("multi chunk session created", count >= 3, count)

    def put(index):
        start = index * chunk_size
        end = min(len(payload), start + chunk_size)
        return request(f"/api/upload/{upload_id}/chunk/{index}", "PUT", payload[start:end])[0]

    check("out of order chunk uploaded", put(count - 1) == 200 and put(0) == 200)
    status, _, _ = json_request(f"/api/upload/{upload_id}/complete", "POST")
    check("complete blocked while chunks missing", status == 409, status)
    status, _, uploads = json_request("/api/upload")
    progress = [u for u in uploads if u["upload_id"] == upload_id]
    check("resume info reports 2/{}".format(count), progress and progress[0]["received"] == 2, progress)

    for index in range(1, count - 1):
        assert put(index) == 200
    status, _, meta = json_request(f"/api/upload/{upload_id}/complete", "POST")
    check("out of order upload completes", status == 200, f"{status} {meta}")
    status, _, body = request(f"/api/files/{meta['id']}/download")
    check("out of order data is byte exact", hashlib.sha256(body).hexdigest() == digest)
    request(f"/api/files/{meta['id']}", "DELETE")

    status, _, init = json_request("/api/upload/init", "POST", {"name": "empty.txt", "size": 0})
    check("empty file session created", status == 200 and init["chunk_count"] == 1, init)
    status, _, meta = json_request(f"/api/upload/{init['upload_id']}/complete", "POST")
    check("empty file completes", status == 200 and meta["size"] == 0, meta)
    empty_id = init["upload_id"]

    status, _, init = json_request(
        "/api/upload/init", "POST", {"name": "partial.bin", "size": 3 * 1024 * 1024}
    )
    upload_id = init["upload_id"]
    request(f"/api/upload/{upload_id}/chunk/0", "PUT", b"x" * 1024)
    status, _, body = json_request(f"/api/upload/{upload_id}/complete", "POST")
    check("incomplete upload rejected", status == 409, f"{status} {body}")
    status, _, _ = request(f"/api/upload/{upload_id}/chunk/1", "PUT", b"x" * 10)
    check("wrong sized chunk rejected", status == 400, status)
    status, _, _ = request(f"/api/upload/{upload_id}", "DELETE")
    check("abort upload returns 204", status == 204, status)
    return empty_id


def test_delete(file_id, empty_id):
    status, _, _ = request(f"/api/files/{file_id}", "DELETE")
    check("delete file returns 204", status == 204, status)
    status, _, _ = request(f"/api/files/{empty_id}", "DELETE")
    check("delete empty file returns 204", status == 204, status)
    status, _, files = json_request("/api/files")
    check("file list is empty after delete", files == [], files)
    status, _, _ = request(f"/api/files/{file_id}", "DELETE")
    check("deleting missing file returns 404", status == 404, status)
    status, _, stats = json_request("/api/stats")
    check("stats endpoint works", status == 200 and "bytes" in stats, stats)


def main():
    label = BASE + (" (with token)" if TOKEN else "")
    print(f"\n\033[1mlan-share e2e test\033[0m -> {label}\n")
    test_static()
    if TOKEN:
        test_auth_required()
    test_websocket()
    file_id = test_upload_download()
    empty_id = test_edge_cases()
    test_delete(file_id, empty_id)

    print()
    if failed:
        print(f"\033[31m{len(failed)} failed\033[0m, {passed} passed")
        for name in failed:
            print("  - " + name)
        return 1
    print(f"\033[32mall {passed} checks passed\033[0m")
    return 0


if __name__ == "__main__":
    sys.exit(main())
