#!/usr/bin/env python3
"""真实浏览器端到端测试（无头 Chrome + CDP）。

通过模拟拖拽事件触发页面上真实的并行分片上传流程，然后点击下载按钮
验证浏览器侧的 Range 并行下载与重组，最后可输出页面截图。

用法:
    python3 scripts/browser_test.py [base_url] [screenshot_path]
"""
import base64
import hashlib
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import time
import urllib.request
from urllib.parse import urlparse

BASE = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18080").rstrip("/")
SHOT = sys.argv[2] if len(sys.argv) > 2 else ""
DEBUG_PORT = int(os.environ.get("LAN_SHARE_DEBUG_PORT", "9333"))
PAYLOAD_MIB = 12

# 同 e2e_test.py：Windows 控制台默认 cp1252，中文输出会抛 UnicodeEncodeError
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


class WebSocket:
    """极简 WebSocket 客户端，仅支持文本帧（CDP 所需）。"""

    def __init__(self, url):
        parsed = urlparse(url)
        # 直播播放等待可能较久，超时放宽
        self.sock = socket.create_connection((parsed.hostname, parsed.port or 80), timeout=120)
        key = base64.b64encode(os.urandom(16)).decode()
        path = parsed.path + (("?" + parsed.query) if parsed.query else "")
        handshake = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {parsed.hostname}:{parsed.port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(handshake.encode())
        self.buf = b""
        while b"\r\n\r\n" not in self.buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise RuntimeError("WebSocket 握手失败")
            self.buf += chunk
        head, _, rest = self.buf.partition(b"\r\n\r\n")
        if b"101" not in head.split(b"\r\n")[0]:
            raise RuntimeError(f"WebSocket 握手被拒绝: {head.split(chr(13).encode())[0]!r}")
        self.buf = rest

    def _read(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("连接已关闭")
            self.buf += chunk
        data, self.buf = self.buf[:n], self.buf[n:]
        return data

    def send_text(self, text):
        payload = text.encode()
        mask = os.urandom(4)
        header = bytearray([0x81])
        size = len(payload)
        if size < 126:
            header.append(0x80 | size)
        elif size < 65536:
            header.append(0x80 | 126)
            header += struct.pack(">H", size)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", size)
        header += mask
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        self.sock.sendall(bytes(header) + masked)

    def recv_text(self):
        while True:
            first, second = self._read(2)
            opcode = first & 0x0F
            length = second & 0x7F
            if length == 126:
                length = struct.unpack(">H", self._read(2))[0]
            elif length == 127:
                length = struct.unpack(">Q", self._read(8))[0]
            if second & 0x80:
                mask = self._read(4)
                data = bytes(b ^ mask[i % 4] for i, b in enumerate(self._read(length)))
            else:
                data = self._read(length)
            if opcode == 0x8:
                raise RuntimeError("对端关闭连接")
            if opcode == 0x9:
                continue
            if opcode in (0x1, 0x2):
                return data.decode("utf-8", "ignore")


class CDP:
    def __init__(self, url):
        self.ws = WebSocket(url)
        self.next_id = 0

    def call(self, method, params=None, timeout=120):
        self.next_id += 1
        msg_id = self.next_id
        self.ws.send_text(json.dumps({"id": msg_id, "method": method, "params": params or {}}))
        deadline = time.time() + timeout
        while time.time() < deadline:
            message = json.loads(self.ws.recv_text())
            if message.get("id") == msg_id:
                if "error" in message:
                    raise RuntimeError(f"{method} 失败: {message['error']}")
                return message.get("result", {})
        raise TimeoutError(method)

    def evaluate(self, expression, await_promise=True):
        result = self.call("Runtime.evaluate", {
            "expression": expression,
            "awaitPromise": await_promise,
            "returnByValue": True,
        })
        if result.get("exceptionDetails"):
            details = result["exceptionDetails"]
            raise RuntimeError(details.get("exception", {}).get("description", str(details)))
        return result["result"].get("value")


def find_page_target():
    for _ in range(60):
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{DEBUG_PORT}/json/list", timeout=2) as resp:
                targets = json.loads(resp.read())
            for target in targets:
                if target.get("type") == "page" and target.get("webSocketDebuggerUrl"):
                    return target["webSocketDebuggerUrl"]
        except Exception:
            time.sleep(0.5)
    raise RuntimeError("未找到可用的 Chrome 页面目标")


UPLOAD_JS = """
(async () => {
  const size = %d * 1024 * 1024 + 12345;
  const buffer = new Uint8Array(size);
  for (let i = 0; i < size; i += 1) buffer[i] = (i * 31 + 7) & 255;
  window.__expectedSha = null;
  const digest = await crypto.subtle.digest('SHA-256', buffer);
  window.__expectedSha = Array.from(new Uint8Array(digest)).map(b => b.toString(16).padStart(2, '0')).join('');
  window.__expectedSize = size;

  const file = new File([buffer], '浏览器 拖拽测试.bin', { type: 'application/octet-stream' });
  const dt = new DataTransfer();
  dt.items.add(file);
  const dropzone = document.getElementById('dropzone');
  dropzone.dispatchEvent(new DragEvent('drop', { dataTransfer: dt, bubbles: true, cancelable: true }));

  const started = Date.now();
  while (Date.now() - started < 90000) {
    const text = document.getElementById('task-list').innerText || '';
    if (text.includes('完成')) return { ok: true, text: text.trim(), ms: Date.now() - started };
    if (text.includes('失败')) return { ok: false, text: text.trim() };
    await new Promise(r => setTimeout(r, 250));
  }
  return { ok: false, text: 'timeout' };
})()
"""


def main():
    print(f"\n\033[1m浏览器端到端测试\033[0m -> {BASE}\n")
    chrome = shutil.which("google-chrome-stable") or shutil.which("google-chrome") or shutil.which("chromium")
    if not chrome:
        print("未找到 Chrome，跳过浏览器测试")
        return 0

    profile = "/tmp/lan-share-chrome-profile"
    shutil.rmtree(profile, ignore_errors=True)
    proc = subprocess.Popen(
        [
            chrome, "--headless=new", "--no-sandbox", "--disable-gpu",
            "--disable-dev-shm-usage", "--no-first-run", "--disable-extensions",
            # 让 getUserMedia 直接返回合成音视频，便于无人值守地跑通「开播」链路
            "--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream",
            "--autoplay-policy=no-user-gesture-required",
            "--no-proxy-server",
            # 自签证书场景（--tls）需要用 https 访问，这里忽略证书错误
            "--ignore-certificate-errors",
            f"--remote-debugging-port={DEBUG_PORT}", f"--user-data-dir={profile}",
            "--window-size=1280,900", "about:blank",
        ],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )

    try:
        cdp = CDP(find_page_target())
        cdp.call("Page.enable")
        cdp.call("Runtime.enable")
        cdp.call("Page.addScriptToEvaluateOnNewDocument", {"source": """
            window.__lanShareErrors = [];
            window.addEventListener('error', (e) => window.__lanShareErrors.push(String(e.message)));
            window.addEventListener('unhandledrejection', (e) => window.__lanShareErrors.push('unhandledrejection: ' + String(e.reason)));
        """})
        cdp.call("Page.navigate", {"url": BASE + "/"})
        time.sleep(2.5)

        console_errors = cdp.evaluate("JSON.stringify(window.__lanShareErrors || [])")
        check("页面加载无脚本错误", console_errors == "[]", console_errors)

        title = cdp.evaluate("document.title")
        check("页面标题正确", "局域网互传" in (title or ""), title)

        ws_state = cdp.evaluate("document.getElementById('ws-text').textContent")
        check("WebSocket 已连接", ws_state == "实时同步", ws_state)

        stats = cdp.evaluate("document.getElementById('stats-text').textContent")
        check("统计信息已渲染", "个文件" in (stats or ""), stats)

        result = cdp.evaluate(UPLOAD_JS % PAYLOAD_MIB)
        check("拖拽触发的并行分片上传完成", bool(result and result.get("ok")), result)
        if result and result.get("ok"):
            print(f"      上传耗时 {result['ms']} ms")

        listed = cdp.evaluate("""
        (async () => {
          const resp = await fetch('/api/files');
          const files = await resp.json();
          const f = files.find(x => x.name.includes('拖拽测试'));
          if (!f) return { found: false };
          const buf = await (await fetch('/api/files/' + f.id + '/download')).arrayBuffer();
          const digest = await crypto.subtle.digest('SHA-256', buf);
          const sha = Array.from(new Uint8Array(digest)).map(b => b.toString(16).padStart(2, '0')).join('');
          const expected = window.__expectedSha;
          window.__testFileId = f.id;
          return { found: true, size: f.size, shaMatches: sha === expected, delivered: buf.byteLength };
        })()
        """)
        check("服务端保存了浏览器上传的文件", bool(listed and listed.get("found")), listed)
        check("服务端数据与浏览器原始数据一致", bool(listed and listed.get("shaMatches")), listed)

        # ------------------------------------------------------ 顶部实时统计条
        statbar = cdp.evaluate("""
        (() => {
          const pick = (id) => (document.getElementById(id) || {}).textContent || '';
          return {
            signal: pick('signal-text').trim(),
            signalClass: document.getElementById('signal-bars').className,
            active: pick('stat-active').trim(),
            remaining: pick('stat-remaining').trim(),
            remainingBytes: pick('stat-remaining-bytes').trim(),
            chunks: pick('stat-chunks').trim(),
            live: pick('stat-live').trim(),
            server: pick('stat-server').trim(),
          };
        })()
        """)
        check("统计条：显示到服务端的延迟", "延迟" in statbar.get("signal", ""), statbar)
        check("统计条：信号等级已判定", "lvl-" in statbar.get("signalClass", ""), statbar.get("signalClass"))
        check("统计条：分段统计可用", "/" in statbar.get("chunks", ""), statbar.get("chunks"))
        check("统计条：显示服务端文件统计", "服务端" in statbar.get("server", ""), statbar.get("server"))

        # 点击下载按钮，验证浏览器侧 Range 并行下载与重组
        download_js = """
        (async () => {
          const rows = Array.from(document.querySelectorAll('#file-list .row'));
          const row = rows.find(r => r.querySelector('.name').textContent.includes('拖拽测试'));
          if (!row) return { ok: false, reason: 'row not found' };
          window.__downloadDone = false;
          row.querySelector('[data-download]').click();
          const started = Date.now();
          while (Date.now() - started < 60000) {
            const toasts = document.getElementById('toasts').innerText || '';
            if (toasts.includes('下载完成')) { window.__downloadDone = true; return { ok: true, ms: Date.now() - started }; }
            if (toasts.includes('下载失败')) return { ok: false, reason: toasts.trim() };
            await new Promise(r => setTimeout(r, 200));
          }
          return { ok: false, reason: 'timeout: ' + (document.getElementById('toasts').innerText || '') };
        })()
        """
        download = cdp.evaluate(download_js)
        check("浏览器并行 Range 下载完成", bool(download and download.get("ok")), download)
        if download and download.get("ok"):
            print(f"      下载耗时 {download['ms']} ms")

        # ---------------------------------------------------------- 直播录屏链路
        live_start_js = """
        (async () => {
          const live = window.__lanShareLive;
          if (!live) return { ok: false, reason: 'live.js 未加载' };
          document.querySelector('#tabs .tab[data-tab="live"]').click();
          document.getElementById('live-title').value = '浏览器自动化直播';
          document.getElementById('live-record').checked = true;
          document.getElementById('live-start-camera').click();

          // 房间一出现在列表里就立刻点「观看」——这时通常还没有任何分片，
          // 正好覆盖「开播瞬间加入」这条曾经出错的路径
          const started = Date.now();
          while (Date.now() - started < 30000) {
            const watch = document.querySelector('#live-list .row [data-watch]');
            if (watch) {
              const beforeChunks = live.cast ? live.cast.chunks : 0;
              watch.click();
              return { ok: true, roomId: live.cast.roomId, chunksBeforeWatch: beforeChunks };
            }
            await new Promise(r => setTimeout(r, 100));
          }
          return { ok: false, reason: '开播后房间未出现在列表' };
        })()
        """
        live = cdp.evaluate(live_start_js)
        check("浏览器开播（真实 MediaRecorder 采集）", bool(live and live.get("ok")), live)
        if live and live.get("ok"):
            print(f"      房间 {live['roomId']}，点观看时分片数={live['chunksBeforeWatch']}")

        live_watch_js = """
        (async () => {
          const live = window.__lanShareLive;
          const video = document.getElementById('live-player');
          const started = Date.now();
          while (Date.now() - started < 20000) {
            if (video.readyState >= 2 && video.currentTime > 0.1) {
              return {
                ok: true,
                currentTime: video.currentTime,
                readyState: video.readyState,
                buffered: video.buffered.length ? video.buffered.end(video.buffered.length - 1) : 0,
                status: document.getElementById('live-player-status').innerText.trim(),
              };
            }
            if ((document.getElementById('live-player-status').innerText || '').includes('已结束')) {
              return { ok: false, reason: '直播提前结束' };
            }
            await new Promise(r => setTimeout(r, 300));
          }
          return {
            ok: false,
            reason: '播放超时',
            status: document.getElementById('live-player-status').innerText.trim(),
            readyState: video.readyState,
            currentTime: video.currentTime,
            wsState: live.player && live.player.ws ? live.player.ws.readyState : -1,
            buffered: video.buffered.length,
            error: video.error ? video.error.message || video.error.code : null,
            received: live.player ? live.player.bytes : 0,
          };
        })()
        """
        playing = cdp.evaluate(live_watch_js)
        check("观看端 MSE 实时播放成功", bool(playing and playing.get("ok")), playing)
        if playing and playing.get("ok"):
            print(f"      已播放到 {playing['currentTime']:.1f}s，缓冲 {playing['buffered']:.1f}s")

        # 手机场景的核心操作：切换前后摄像头后观众应自动重新同步并继续播放
        flip_js = """
        (async () => {
          const live = window.__lanShareLive;
          if (!live.cast) return { ok: false, reason: '没有正在进行的推流' };
          const roomBefore = live.cast.roomId;
          document.getElementById('live-flip').click();
          const video = document.getElementById('live-player');
          const started = Date.now();
          while (Date.now() - started < 30000) {
            const cast = live.cast;
            if (cast && !cast.flipping && cast.chunks > 0 && video.currentTime > 0.1) {
              return { ok: true, sameRoom: cast.roomId === roomBefore, currentTime: video.currentTime };
            }
            await new Promise(r => setTimeout(r, 300));
          }
          return { ok: false, reason: document.getElementById('live-player-status').innerText.trim() };
        })()
        """
        flipped = cdp.evaluate(flip_js)
        check("切换摄像头后观众自动恢复播放", bool(flipped and flipped.get("ok")), flipped)
        if flipped and flipped.get("ok"):
            print(f"      切换后房间号保持不变: {flipped['sameRoom']}，播放到 {flipped['currentTime']:.1f}s")

        if SHOT:
            shot = cdp.call("Page.captureScreenshot", {"format": "png", "captureBeyondViewport": True})
            with open(SHOT.rsplit(".", 1)[0] + ".live.png", "wb") as handle:
                handle.write(base64.b64decode(shot["data"]))
            print(f"      直播页截图: {SHOT.rsplit('.', 1)[0]}.live.png")

        live_stop_js = """
        (async () => {
          const live = window.__lanShareLive;
          document.getElementById('live-stop').click();
          const started = Date.now();
          while (Date.now() - started < 30000) {
            if (!live.cast) break;
            await new Promise(r => setTimeout(r, 300));
          }
          await new Promise(r => setTimeout(r, 1500));
          const toasts = document.getElementById('toasts').innerText || '';
          document.querySelector('#tabs .tab[data-tab="files"]').click();
          await new Promise(r => setTimeout(r, 800));
          const names = Array.from(document.querySelectorAll('#file-list .name')).map(el => el.textContent);
          return { ok: true, toasts: toasts.trim(), recording: names.some(n => n.includes('录屏-')) };
        })()
        """
        stopped = cdp.evaluate(live_stop_js)
        check("结束直播后生成录像文件", bool(stopped and stopped.get("recording")), stopped)
        if stopped:
            print(f"      提示: {stopped['toasts'][:80]}")

        if SHOT:
            shot = cdp.call("Page.captureScreenshot", {"format": "png", "captureBeyondViewport": True})
            with open(SHOT, "wb") as handle:
                handle.write(base64.b64decode(shot["data"]))
            print(f"      截图已保存: {SHOT}")
            cdp.call("Emulation.setEmulatedMedia", {
                "features": [{"name": "prefers-color-scheme", "value": "light"}],
            })
            time.sleep(0.5)
            light = cdp.call("Page.captureScreenshot", {"format": "png", "captureBeyondViewport": True})
            light_path = SHOT.rsplit(".", 1)[0] + ".light.png"
            with open(light_path, "wb") as handle:
                handle.write(base64.b64decode(light["data"]))
            print(f"      浅色截图已保存: {light_path}")

        # 清理测试文件
        cdp.evaluate("""
        (async () => {
          window.confirm = () => true;
          const rows = Array.from(document.querySelectorAll('#file-list .row'));
          const row = rows.find(r => r.querySelector('.name').textContent.includes('拖拽测试'));
          if (row) row.querySelector('[data-delete]').click();
          await new Promise(r => setTimeout(r, 800));
        })()
        """)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()

    print()
    if failed:
        print(f"\033[31m{len(failed)} 项失败\033[0m，{passed} 项通过")
        return 1
    print(f"\033[32m全部 {passed} 项浏览器检查通过\033[0m")
    return 0


if __name__ == "__main__":
    sys.exit(main())
