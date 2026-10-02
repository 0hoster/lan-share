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
import base64
import struct
import urllib.error
import urllib.request
from urllib.parse import urlparse

BASE = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18080").rstrip("/")
TOKEN = sys.argv[2] if len(sys.argv) > 2 else ""
PARALLEL = 4

# Windows 上 Python 默认用 cp1252 输出，打印中文检查名会直接抛
# UnicodeEncodeError 把测试打死，这里强制 UTF-8。
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, ValueError):
        pass

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


def json_request(path, method="GET", payload=None, headers=None):
    data = json.dumps(payload).encode() if payload is not None else None
    extra = dict(headers or {})
    if payload is not None:
        extra["Content-Type"] = "application/json"
    status, resp_headers, body = request(path, method=method, data=data, headers=extra)
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


class WsClient:
    """够用的 WebSocket 客户端：文本 + 二进制帧，用于验证实时转发。"""

    def __init__(self, path, timeout=15):
        parsed = urlparse(BASE)
        host = parsed.hostname or "127.0.0.1"
        port = parsed.port or 80
        if TOKEN:
            path += ("&" if "?" in path else "?") + "token=" + TOKEN
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.buf = b""
        key = base64.b64encode(os.urandom(16)).decode()
        handshake = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(handshake.encode())
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise RuntimeError("websocket 握手失败")
            head += chunk
        self.status_line = head.split(b"\r\n")[0].decode("latin1")
        self.buf = head.split(b"\r\n\r\n", 1)[1]

    def _read(self, size):
        while len(self.buf) < size:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("连接已关闭")
            self.buf += chunk
        data, self.buf = self.buf[:size], self.buf[size:]
        return data

    def recv_frame(self):
        """返回 (opcode, payload)；仅处理服务端发来的未掩码帧。"""
        first, second = self._read(2)
        opcode = first & 0x0F
        length = second & 0x7F
        if length == 126:
            length = struct.unpack(">H", self._read(2))[0]
        elif length == 127:
            length = struct.unpack(">Q", self._read(8))[0]
        if second & 0x80:
            mask = self._read(4)
            payload = bytes(b ^ mask[i % 4] for i, b in enumerate(self._read(length)))
        else:
            payload = self._read(length)
        return opcode, payload

    def recv_text(self):
        while True:
            opcode, payload = self.recv_frame()
            if opcode == 0x1:
                return payload.decode("utf-8", "ignore")
            if opcode == 0x8:
                raise RuntimeError("连接被关闭")

    def recv_binary(self):
        while True:
            opcode, payload = self.recv_frame()
            if opcode == 0x2:
                return payload
            if opcode == 0x8:
                raise RuntimeError("连接被关闭")

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def test_websocket():
    try:
        client = WsClient("/ws")
    except OSError as err:
        check("websocket upgrade succeeds", False, err)
        return
    check("websocket upgrade succeeds", "101" in client.status_line, client.status_line)
    client.close()


def test_live_relay():
    """直播链路：推流密钥、房间列表、中途加入的快照、实时转发、结束与录像。"""
    mime = "video/webm;codecs=vp8,opus"
    status, _, start = json_request(
        "/api/live/start", "POST", {"title": "自动化测试直播", "mime": mime, "record": True}
    )
    check("live: 创建直播间成功", status == 200 and start.get("room_id"), start)
    if status != 200:
        return
    room_id, key = start["room_id"], start["key"]

    status, _, rooms = json_request("/api/live/rooms")
    room = next((r for r in rooms if r["id"] == room_id), None)
    check("live: 房间出现在列表中", room is not None, rooms)
    check("live: 房间记录了编码格式", room and room["mime"] == mime, room)
    check("live: 标记了正在录像", room and room["recording"] is True, room)

    status, _, _ = request(
        f"/api/live/{room_id}/chunk", "PUT", b"x" * 64, headers={"x-live-key": "wrong-key"}
    )
    check("live: 错误密钥被拒绝", status == 403, status)

    # 第一个分片当作 WebM 初始化段，其后是媒体分片
    init_segment = b"\x1a\x45\xdf\xa3" + b"init-segment" * 8
    chunks = [init_segment] + [bytes([i]) * 512 for i in range(1, 5)]
    for index, chunk in enumerate(chunks):
        status, _, _ = request(
            f"/api/live/{room_id}/chunk", "PUT", chunk, headers={"x-live-key": key}
        )
        assert status == 200, f"推流分片 {index} 失败: {status}"
    check("live: 连续推流 5 个分片", True)

    # 中途加入的观众应当拿到 info + 初始化分片 + 最近分片
    viewer = WsClient(f"/api/live/{room_id}/ws")
    info = json.loads(viewer.recv_text())
    check("live: 观众收到房间信息", info.get("type") == "info" and info["room"]["id"] == room_id, info)
    first = viewer.recv_binary()
    check("live: 新观众先拿到初始化分片", first == init_segment, len(first))
    tail = [viewer.recv_binary() for _ in range(4)]
    check("live: 新观众补齐最近分片", tail == chunks[1:], [len(t) for t in tail])

    # 实时转发
    live_chunk = b"\x00live-tail-chunk"
    request(f"/api/live/{room_id}/chunk", "PUT", live_chunk, headers={"x-live-key": key})
    pushed = viewer.recv_binary()
    check("live: 新分片实时转发给观众", pushed == live_chunk, len(pushed))

    # 结束直播并校验录像
    status, _, stop = json_request(f"/api/live/{room_id}/stop", "POST", headers={"x-live-key": key})
    check("live: 结束直播成功", status == 200, f"{status} {stop}")
    saved = stop.get("saved") if stop else None
    expected_size = sum(len(c) for c in chunks) + len(live_chunk)
    check("live: 生成了录像文件", bool(saved) and saved["size"] == expected_size, saved)
    check("live: 录像 MIME 正确", saved and saved["mime"] == "video/webm", saved)
    check("live: 录像文件名带时间戳", saved and saved["name"].startswith("录屏-自动化测试直播-"), saved)

    status, _, files = json_request("/api/files")
    check("live: 录像进入文件列表", any(f["id"] == saved["id"] for f in files), files)
    if saved:
        status, _, body = request(f"/api/files/{saved['id']}/download")
        check(
            "live: 录像内容与推流数据一致",
            body == b"".join(chunks) + live_chunk,
            len(body),
        )
        request(f"/api/files/{saved['id']}", "DELETE")

    status, _, rooms = json_request("/api/live/rooms")
    check("live: 结束后房间消失", all(r["id"] != room_id for r in rooms), rooms)
    try:
        viewer.close()
    except OSError:
        pass


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
    test_live_relay()
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
