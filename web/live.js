/* lan-share 直播录屏
 * 推送端：getDisplayMedia / getUserMedia → MediaRecorder 分片 → PUT /api/live/{id}/chunk
 * 观看端：WebSocket 收分片 → MediaSource 播放（延迟约 1~3 秒）
 */
(() => {
  'use strict';

  const $ = (sel) => document.querySelector(sel);

  const state = {
    token: new URLSearchParams(location.search).get('token') || localStorage.getItem('lan-share-token') || '',
    rooms: [],
    cast: null,   // 推流中的会话
    player: null, // 观看中的会话
  };

  /** 推流端最多容忍多少个分片积压（约 1 分钟），超过就判定网络跟不上 */
  const MAX_PENDING_CHUNKS = 60;
  /** 观看端还没喂给播放器的分片上限，超过就重连取快照 */
  const MAX_BUFFERED_CHUNKS = 300;

  // ---------------------------------------------------------------- 工具

  const withToken = (url) =>
    state.token ? url + (url.includes('?') ? '&' : '?') + 'token=' + encodeURIComponent(state.token) : url;

  const authHeaders = (extra) => {
    const headers = Object.assign({}, extra || {});
    if (state.token) headers['Authorization'] = 'Bearer ' + state.token;
    return headers;
  };

  const wsUrl = (path) => {
    const base = (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + path;
    return state.token ? base + (path.includes('?') ? '&' : '?') + 'token=' + encodeURIComponent(state.token) : base;
  };

  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  function fmtBytes(n) {
    if (!Number.isFinite(n) || n < 0) return '—';
    if (n < 1024) return n + ' B';
    const units = ['KB', 'MB', 'GB'];
    let value = n / 1024;
    let i = 0;
    while (value >= 1024 && i < units.length - 1) { value /= 1024; i++; }
    return value.toFixed(value >= 100 ? 0 : 1) + ' ' + units[i];
  }

  function fmtDuration(seconds) {
    if (!Number.isFinite(seconds) || seconds < 0) return '—';
    const s = Math.floor(seconds % 60);
    const m = Math.floor((seconds / 60) % 60);
    const h = Math.floor(seconds / 3600);
    const mm = String(m).padStart(2, '0');
    const ss = String(s).padStart(2, '0');
    return h > 0 ? `${h}:${mm}:${ss}` : `${m}:${ss}`;
  }

  function escapeHtml(str) {
    return String(str).replace(/[&<>"']/g, (c) =>
      ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  }

  function toast(message, kind = '') {
    const el = document.createElement('div');
    el.className = 'toast ' + kind;
    el.textContent = message;
    $('#toasts').appendChild(el);
    setTimeout(() => {
      el.style.opacity = '0';
      el.style.transition = 'opacity .3s';
      setTimeout(() => el.remove(), 320);
    }, 4200);
  }

  async function parseError(resp) {
    try {
      const data = await resp.json();
      if (data && data.error) return data.error;
    } catch (_) { /* ignore */ }
    return 'HTTP ' + resp.status;
  }

  // ---------------------------------------------------------------- 标签页

  function bindTabs() {
    const tabs = Array.from(document.querySelectorAll('#tabs .tab'));
    tabs.forEach((tab) => {
      tab.addEventListener('click', () => activate(tab.dataset.tab));
    });
    if (location.hash === '#live') activate('live');
  }

  function activate(name) {
    document.querySelectorAll('#tabs .tab').forEach((tab) => {
      tab.classList.toggle('active', tab.dataset.tab === name);
    });
    document.querySelectorAll('.panel').forEach((panel) => {
      panel.hidden = panel.dataset.panel !== name;
    });
    if (name === 'live') {
      describeSupport();
      loadRooms();
    }
    history.replaceState(null, '', name === 'live' ? '#live' : '#files');
  }

  function describeSupport() {
    const hint = $('#live-support');
    if (!window.MediaRecorder || !navigator.mediaDevices) {
      // 局域网 IP + http 时浏览器会直接隐藏 mediaDevices（安全上下文限制）
      hint.innerHTML = isSecure() 
        ? '当前浏览器不支持录制推送，请用 Chrome / Edge / Firefox'
        : '当前是 <b>http</b> 访问，浏览器禁用了采集能力：请用 <code>--tls</code> 启动后以 <b>https</b> 打开本页';
      $('#live-start-screen').disabled = true;
      $('#live-start-camera').disabled = true;
      return;
    }
    const mime = pickMime();
    hint.textContent = mime ? '编码：' + mime.replace('video/webm;codecs=', '') : '当前浏览器不支持 WebM 录制';
    if (!mime) {
      $('#live-start-screen').disabled = true;
      $('#live-start-camera').disabled = true;
    }
  }

  /** getUserMedia / getDisplayMedia 只在安全上下文可用 */
  function isSecure() {
    if (window.isSecureContext) return true;
    const host = location.hostname;
    return host === 'localhost' || host === '127.0.0.1' || host === '[::1]';
  }

  function isMobile() {
    return /Android|iPhone|iPad|iPod|Mobile/i.test(navigator.userAgent);
  }

  /** 摄像头画质档位：手机默认用标清，省电省流量 */
  const QUALITY = {
    low: { width: 854, height: 480, bps: 1_200_000 },
    medium: { width: 1280, height: 720, bps: 2_800_000 },
    high: { width: 1920, height: 1080, bps: 6_000_000 },
  };

  function currentQuality() {
    const select = $('#live-quality');
    return QUALITY[select ? select.value : 'medium'] || QUALITY.medium;
  }

  /** 手机默认后置摄像头，桌面默认前置（笔记本更常见） */
  function preferredFacing() {
    const select = $('#live-facing');
    if (select && select.value) return select.value;
    return isMobile() ? 'environment' : 'user';
  }

  function cameraConstraints(facing, deviceId) {
    const quality = currentQuality();
    const video = {
      width: { ideal: quality.width },
      height: { ideal: quality.height },
      frameRate: { ideal: 24, max: 30 },
    };
    if (deviceId) {
      video.deviceId = { exact: deviceId };
    } else if (facing) {
      video.facingMode = { ideal: facing };
    }
    return { video };
  }

  function cameraStream(facing, deviceId) {
    return navigator.mediaDevices.getUserMedia(cameraConstraints(facing, deviceId));
  }

  /** 列出摄像头，手机上有前置/后置两个 */
  async function refreshCameras() {
    const select = $('#live-device');
    if (!select || !navigator.mediaDevices || !navigator.mediaDevices.enumerateDevices) return;
    try {
      const devices = (await navigator.mediaDevices.enumerateDevices())
        .filter((device) => device.kind === 'videoinput');
      if (devices.length === 0) return;
      const current = select.value;
      select.innerHTML = '<option value="">默认摄像头</option>';
      devices.forEach((device, index) => {
        const option = document.createElement('option');
        option.value = device.deviceId;
        option.textContent = device.label || `摄像头 ${index + 1}`;
        select.appendChild(option);
      });
      if (current && devices.some((device) => device.deviceId === current)) {
        select.value = current;
      }
    } catch (_) { /* 未授权时拿不到标签，忽略 */ }
  }

  /** 分片编码必须和实际音轨匹配：没有音轨却声明 opus，播放端会初始化失败 */
  function pickMime(wantAudio) {
    if (!window.MediaRecorder || !MediaRecorder.isTypeSupported) return '';
    const candidates = wantAudio
      ? [
          'video/webm;codecs=vp8,opus',
          'video/webm;codecs=vp9,opus',
          'video/webm',
        ]
      : [
          'video/webm;codecs=vp8',
          'video/webm;codecs=vp9',
          'video/webm',
        ];
    return candidates.find((m) => MediaRecorder.isTypeSupported(m)) || '';
  }

  // ---------------------------------------------------------------- 房间列表

  async function loadRooms() {
    try {
      const resp = await fetch(withToken('/api/live/rooms'), { headers: authHeaders() });
      if (!resp.ok) throw new Error(await parseError(resp));
      state.rooms = await resp.json();
      renderRooms();
    } catch (err) {
      toast('读取直播列表失败：' + err.message, 'err');
    }
  }

  function renderRooms() {
    const list = $('#live-list');
    list.innerHTML = '';
    $('#live-count').textContent = state.rooms.length ? `(${state.rooms.length})` : '';
    $('#live-empty').hidden = state.rooms.length > 0;

    for (const room of state.rooms) {
      const mine = state.cast && state.cast.roomId === room.id;
      const seconds = Math.max(0, Math.floor(Date.now() / 1000) - room.started_at);
      const li = document.createElement('li');
      li.className = 'row';
      li.innerHTML = `
        <span class="icon">${room.recording ? '⏺' : '📡'}</span>
        <div class="grow">
          <div class="name" title="${escapeHtml(room.title)}">${escapeHtml(room.title)}${mine ? ' <span class="tag">我的直播</span>' : ''}</div>
          <div class="meta">
            <span>已直播 ${fmtDuration(seconds)}</span>
            <span>${room.viewers} 人观看</span>
            <span>${fmtBytes(room.bytes)}</span>
            ${room.recording ? '<span>正在录像</span>' : ''}
          </div>
        </div>
        <div class="actions">
          <button class="act primary" data-watch type="button">观看</button>
          ${mine ? '<button class="act danger" data-stop type="button">结束</button>' : ''}
        </div>`;
      li.querySelector('[data-watch]').onclick = () => watchRoom(room);
      const stopButton = li.querySelector('[data-stop]');
      if (stopButton) stopButton.onclick = () => stopBroadcast();
      list.appendChild(li);
    }
  }

  // ---------------------------------------------------------------- 开播

  async function startBroadcast(source) {
    if (state.cast) {
      toast('已经在直播中', 'err');
      return;
    }
    if (!isSecure()) {
      toast('当前是 http 访问，浏览器不允许调用摄像头/屏幕；请用 https 打开', 'err');
      return;
    }
    let captured;
    try {
      captured = source === 'screen'
        ? await navigator.mediaDevices.getDisplayMedia({ video: { frameRate: 24 }, audio: true })
        : await cameraStream(preferredFacing());
    } catch (err) {
      toast('没有拿到采集源：' + (err && err.message ? err.message : err), 'err');
      return;
    }

    let stream = captured;
    if ($('#live-mic').checked) {
      try {
        const mic = await navigator.mediaDevices.getUserMedia({ audio: true });
        stream = mixAudio(captured, mic);
      } catch (err) {
        toast('麦克风不可用，仅使用原有声音轨', 'err');
      }
    }

    // 编码声明必须与实际音轨一致，否则观看端 MSE 初始化会失败
    const mime = pickMime(stream.getAudioTracks().length > 0);
    if (!mime) {
      captured.getTracks().forEach((track) => track.stop());
      toast('当前浏览器不支持 WebM 录制，无法开播', 'err');
      return;
    }

    const title = $('#live-title').value.trim() || (source === 'screen' ? '屏幕直播' : '摄像头直播');
    const record = $('#live-record').checked;

    let session;
    try {
      const resp = await fetch(withToken('/api/live/start'), {
        method: 'POST',
        headers: authHeaders({ 'Content-Type': 'application/json' }),
        body: JSON.stringify({ title, mime, record }),
      });
      if (!resp.ok) throw new Error(await parseError(resp));
      session = await resp.json();
    } catch (err) {
      captured.getTracks().forEach((track) => track.stop());
      toast('开播失败：' + err.message, 'err');
      return;
    }

    const cast = {
      roomId: session.room_id,
      key: session.key,
      title,
      mime,
      source,
      facing: source === 'camera' ? preferredFacing() : null,
      deviceId: $('#live-device') ? $('#live-device').value : '',
      bps: currentQuality().bps,
      stream,
      captured,
      micStream: stream.__keepAlive ? stream.__keepAlive.micStream : null,
      queue: [],
      sending: false,
      stopping: false,
      settled: false,
      flipping: false,
      bytes: 0,
      chunks: 0,
      viewers: 0,
      speed: 0,
      startedAt: performance.now(),
      lastBytes: 0,
      lastAt: performance.now(),
      failed: false,
    };
    state.cast = cast;

    makeRecorder(cast);
    bindTrackEnded(cast);

    $('#live-preview').srcObject = stream;
    $('#live-preview').play().catch(() => { /* 预览失败不影响推流 */ });
    $('#live-self').hidden = false;
    $('#live-flip').disabled = source !== 'camera';
    refreshCameras();
    $('#live-title').disabled = true;
    $('#live-start-screen').disabled = true;
    $('#live-start-camera').disabled = true;
    loadRooms();
    toast('已开播：' + title, 'ok');
  }

  /** 把系统声音与麦克风混成一条音轨 */
  function mixAudio(videoStream, micStream) {
    const ctx = new (window.AudioContext || window.webkitAudioContext)();
    const destination = ctx.createMediaStreamDestination();
    if (videoStream.getAudioTracks().length > 0) {
      ctx.createMediaStreamSource(new MediaStream(videoStream.getAudioTracks())).connect(destination);
    }
    ctx.createMediaStreamSource(micStream).connect(destination);
    const mixed = new MediaStream([
      ...videoStream.getVideoTracks(),
      ...destination.stream.getAudioTracks(),
    ]);
    mixed.__keepAlive = { ctx, micStream };
    return mixed;
  }

  /** 创建并启动录制器：每次切换采集源都会新建一个（所以要 reset 房间） */
  function makeRecorder(cast) {
    const mime = pickMime(cast.stream.getAudioTracks().length > 0);
    if (!mime) throw new Error('当前浏览器不支持 WebM 录制');
    cast.mime = mime;
    const recorder = new MediaRecorder(cast.stream, {
      mimeType: mime,
      videoBitsPerSecond: cast.bps,
      audioBitsPerSecond: 128_000,
    });
    cast.recorder = recorder;
    recorder.ondataavailable = (event) => {
      if (!event.data || event.data.size === 0) return;
      if (cast.settled) return; // 已经收尾完毕，丢弃迟到的分片
      cast.queue.push(event.data);
      pumpQueue();
    };
    recorder.onerror = (event) => fail('录制出错：' + (event.error && event.error.name));
    recorder.start(1000);
    return recorder;
  }

  /** 停止录制器，并等已产生的分片推完 */
  async function stopRecorder(cast) {
    const recorder = cast.recorder;
    if (!recorder || recorder.state === 'inactive') return;
    await new Promise((resolve) => {
      let done = false;
      const finish = () => {
        if (!done) {
          done = true;
          resolve();
        }
      };
      recorder.addEventListener('stop', finish, { once: true });
      try {
        recorder.stop();
      } catch (_) {
        finish();
      }
      setTimeout(finish, 3000);
    });
    const deadline = Date.now() + 5000;
    while ((cast.queue.length > 0 || cast.sending) && Date.now() < deadline) {
      await sleep(150);
    }
  }

  function releaseStream(stream) {
    if (!stream) return;
    try {
      stream.getTracks().forEach((track) => track.stop());
    } catch (_) { /* ignore */ }
    if (stream.__keepAlive) {
      try {
        stream.__keepAlive.micStream.getTracks().forEach((track) => track.stop());
      } catch (_) { /* ignore */ }
      try {
        stream.__keepAlive.ctx.close();
      } catch (_) { /* ignore */ }
    }
  }

  function bindTrackEnded(cast) {
    cast.captured.getVideoTracks().forEach((track) => {
      track.addEventListener('ended', () => {
        if (!cast.stopping) stopBroadcast();
      });
    });
  }

  /** 切换采集源（摄像头前后切换 / 换设备），房间号不变，观众自动重新同步 */
  async function switchCamera(options) {
    const cast = state.cast;
    if (!cast || cast.flipping) return;
    if (cast.source !== 'camera') {
      toast('只有摄像头直播可以切换镜头', 'err');
      return;
    }
    cast.flipping = true;
    const flipButton = $('#live-flip');
    if (flipButton) flipButton.disabled = true;
    try {
      const nextFacing = options.facing || cast.facing;
      const nextDevice = options.deviceId !== undefined ? options.deviceId : cast.deviceId;
      cast.bps = currentQuality().bps;
      const captured = await cameraStream(nextFacing, nextDevice);

      await stopRecorder(cast);
      releaseStream(cast.stream);
      cast.captured.getTracks().forEach((track) => track.stop());
      cast.captured = captured;

      let stream = captured;
      if (cast.micStream) {
        stream = mixAudio(captured, cast.micStream);
      }
      cast.stream = stream;
      cast.facing = nextFacing;
      cast.deviceId = nextDevice;

      // 新的 MediaRecorder 会重新生成 WebM 初始化分片，让观众重连取快照
      await fetch(withToken(`/api/live/${cast.roomId}/reset`), {
        method: 'POST',
        headers: authHeaders({ 'x-live-key': cast.key }),
      });
      cast.settled = false;
      makeRecorder(cast);
      bindTrackEnded(cast);

      const preview = $('#live-preview');
      preview.srcObject = stream;
      preview.play().catch(() => { /* ignore */ });
      toast(nextFacing === 'environment' ? '已切换到后置摄像头' : '已切换到前置摄像头', 'ok');
    } catch (err) {
      toast('切换摄像头失败：' + (err && err.message ? err.message : err), 'err');
      if (!cast.recorder || cast.recorder.state === 'inactive') {
        try {
          makeRecorder(cast);
        } catch (inner) {
          fail('切换后无法继续推流：' + inner.message);
        }
      }
    } finally {
      cast.flipping = false;
      if (flipButton) flipButton.disabled = false;
      refreshCameras();
    }
  }

  function flipCamera() {
    const cast = state.cast;
    if (!cast) return;
    const next = cast.facing === 'user' ? 'environment' : 'user';
    return switchCamera({ facing: next });
  }

  /** 顺序把分片推给服务端（顺序发送可以保证时间戳单调） */
  async function pumpQueue() {
    const cast = state.cast;
    if (!cast || cast.sending) return;
    cast.sending = true;
    while (cast.queue.length && !cast.failed) {
      const blob = cast.queue.shift();
      try {
        const resp = await fetch(withToken(`/api/live/${cast.roomId}/chunk`), {
          method: 'PUT',
          headers: authHeaders({
            'Content-Type': 'application/octet-stream',
            'x-live-key': cast.key,
          }),
          body: blob,
        });
        if (!resp.ok) throw new Error(await parseError(resp));
        const info = await resp.json();
        cast.bytes = info.bytes;
        cast.chunks = info.seq + 1;
        cast.viewers = info.viewers;
        // 网络长期跟不上采集速度时，积压会让内存无限增长，不如直接停下来
        if (cast.queue.length > MAX_PENDING_CHUNKS) {
          fail('网络过慢，已排队 ' + cast.queue.length + ' 个分片，直播已停止');
          break;
        }
      } catch (err) {
        fail('推流中断：' + err.message);
        break;
      }
    }
    cast.sending = false;
    updateCastStats();
  }

  function fail(message) {
    const cast = state.cast;
    if (!cast) return;
    cast.failed = true;
    toast(message, 'err');
    stopBroadcast();
  }

  async function stopBroadcast() {
    const cast = state.cast;
    if (!cast || cast.stopping) return;
    cast.stopping = true;

    // 先等录制器把最后一个分片吐出来并推完，否则那一片会在房间结束后才发出
    await stopRecorder(cast);
    cast.settled = true;

    try {
      const resp = await fetch(withToken(`/api/live/${cast.roomId}/stop`), {
        method: 'POST',
        headers: authHeaders({ 'x-live-key': cast.key }),
      });
      if (resp.ok) {
        const data = await resp.json();
        toast(data.saved ? '直播已结束，录像已保存：' + data.saved.name : '直播已结束', 'ok');
      } else {
        toast('结束直播失败：' + await parseError(resp), 'err');
      }
    } catch (err) {
      toast('结束直播失败：' + err.message, 'err');
    }
    cleanupCast(cast);
  }

  function cleanupCast(cast) {
    releaseStream(cast.stream);
    try {
      cast.captured.getTracks().forEach((track) => track.stop());
    } catch (_) { /* ignore */ }
    if (state.cast === cast) state.cast = null;
    $('#live-preview').srcObject = null;
    $('#live-self').hidden = true;
    $('#live-flip').disabled = true;
    $('#live-title').disabled = false;
    $('#live-start-screen').disabled = false;
    $('#live-start-camera').disabled = false;
    loadRooms();
  }

  function updateCastStats() {
    const cast = state.cast;
    if (!cast) return;
    const now = performance.now();
    const dt = (now - cast.lastAt) / 1000;
    if (dt > 0.5) {
      cast.speed = Math.max(0, (cast.bytes - cast.lastBytes) / dt);
      cast.lastBytes = cast.bytes;
      cast.lastAt = now;
    }
    const elapsed = (now - cast.startedAt) / 1000;
    $('#live-stats').innerHTML =
      `<span>已推流 ${fmtBytes(cast.bytes)}</span>` +
      `<span>${fmtDuration(elapsed)}</span>` +
      `<span>${fmtBytes(cast.speed)}/s</span>` +
      `<span>${cast.viewers} 人观看</span>` +
      (cast.failed ? '<span class="err-text">已中断</span>' : '');
  }

  // ---------------------------------------------------------------- 观看

  function watchRoom(room) {
    closePlayer();
    const card = $('#live-player-card');
    card.hidden = false;
    $('#live-player-title').textContent = room.title;
    setStatus('正在连接…');
    card.scrollIntoView({ behavior: 'smooth', block: 'nearest' });

    const video = $('#live-player');
    const source = new MediaSource();
    const objectUrl = URL.createObjectURL(source);
    const player = {
      room, source, objectUrl, video,
      sb: null, queue: [], current: null, appending: false,
      bytes: 0, ws: null, closed: false, info: null,
    };
    state.player = player;
    video.src = objectUrl;
    video.play().catch(() => { /* 需要用户手势时由控件接管 */ });

    source.addEventListener('sourceopen', () => {
      try {
        player.sb = source.addSourceBuffer(normalizeMime(room.mime));
      } catch (_) {
        try {
          player.sb = source.addSourceBuffer('video/webm');
        } catch (_) {
          setStatus('当前浏览器无法播放该编码，建议用 Chrome / Edge 观看');
          return;
        }
      }
      player.sb.mode = 'segments';
      player.sb.addEventListener('updateend', onUpdateEnd);
      connect(player);
      pump();
    });
  }

  /** MSE 要求 codecs 参数加引号，例如 video/webm;codecs="vp8,opus" */
  function normalizeMime(mime) {
    const raw = (mime || 'video/webm').trim();
    const match = raw.match(/^(.*?codecs\s*=\s*)([^;"]+)(.*)$/i);
    if (!match) return raw;
    const list = match[2].trim().replace(/^"|"$/g, '');
    return `${match[1]}"${list}"${match[3]}`;
  }

  function connect(player) {
    const ws = new WebSocket(wsUrl(`/api/live/${player.room.id}/ws`));
    player.ws = ws;
    ws.binaryType = 'arraybuffer';
    ws.onmessage = (event) => {
      if (typeof event.data === 'string') {
        let msg;
        try { msg = JSON.parse(event.data); } catch (_) { return; }
        if (msg.type === 'ended') {
          player.closed = true;
          setStatus('直播已结束');
          try { ws.close(); } catch (_) { /* ignore */ }
        } else if (msg.type === 'lagged') {
          setStatus('网络较慢，正在重新同步…');
          reconnect();
        } else if (msg.type === 'reset') {
          // 主播换了采集源（例如切换摄像头），重新取快照
          setStatus('主播切换了画面来源，正在重新同步…');
          reconnect();
        } else if (msg.type === 'info') {
          player.info = msg.room;
        }
        return;
      }
      player.bytes += event.data.byteLength;
      player.queue.push(new Uint8Array(event.data));
      // 播放器严重落后时，与其越堆越多，不如重连拿一份新的快照
      if (player.queue.length > MAX_BUFFERED_CHUNKS) {
        setStatus('播放落后过多，正在重新同步…');
        reconnect();
        return;
      }
      pump();
    };
    ws.onclose = () => {
      if (!player.closed && state.player === player) {
        player.closed = true;
        setStatus('连接已断开');
      }
    };
  }

  function reconnect() {
    const player = state.player;
    if (!player) return;
    const room = player.room;
    closePlayer();
    setTimeout(() => watchRoom(room), 300);
  }

  function onUpdateEnd() {
    const player = state.player;
    if (!player || !player.sb) return;
    if (player.appending) {
      player.appending = false;
      player.current = null;
    }
    pump();
    followLiveEdge();
  }

  function pump() {
    const player = state.player;
    if (!player || !player.sb || player.closed || player.sb.updating) return;
    if (!player.current) {
      if (!player.queue.length) {
        followLiveEdge();
        return;
      }
      player.current = player.queue.shift();
    }
    try {
      player.sb.appendBuffer(player.current);
      player.appending = true;
    } catch (err) {
      if (err && err.name === 'QuotaExceededError') {
        evictOld();
        return;
      }
      player.current = null;
      setStatus('播放出错：' + (err && err.message ? err.message : err));
    }
  }

  /** 缓冲区过大时丢掉最旧的一段，给新数据腾地方 */
  function evictOld() {
    const player = state.player;
    if (!player || !player.sb) return;
    const buffered = player.video.buffered;
    if (!buffered.length || buffered.end(buffered.length - 1) - buffered.start(0) < 12) {
      player.current = null;
      pump();
      return;
    }
    const start = buffered.start(0);
    const end = buffered.end(buffered.length - 1);
    try {
      player.sb.remove(start, Math.max(start + 1, end - 8));
    } catch (_) {
      player.current = null;
      pump();
    }
  }

  /** 尽量贴着直播点播放，落后太多就跳一下 */
  function followLiveEdge() {
    const player = state.player;
    if (!player) return;
    const video = player.video;
    if (!video.buffered.length) return;
    const edge = video.buffered.end(video.buffered.length - 1);
    if (video.paused) video.play().catch(() => { /* ignore */ });
    const lag = edge - video.currentTime;
    if (lag > 4) video.currentTime = Math.max(0, edge - 1);
    updatePlayerStatus(Math.max(0, lag));
  }

  function updatePlayerStatus(lag) {
    const player = state.player;
    if (!player) return;
    const started = player.info ? player.info.started_at : 0;
    const elapsed = started ? Math.max(0, Math.floor(Date.now() / 1000) - started) : 0;
    const viewers = player.info ? player.info.viewers : 0;
    const recording = player.info && player.info.recording ? '<span>主播正在录像</span>' : '';
    $('#live-player-status').innerHTML =
      `<span>已直播 ${fmtDuration(elapsed)}</span>` +
      `<span>落后直播点 ${lag.toFixed(1)} 秒</span>` +
      `<span>已接收 ${fmtBytes(player.bytes)}</span>` +
      `<span>${viewers} 人观看</span>` + recording;
  }

  function setStatus(text) {
    $('#live-player-status').innerHTML = `<span>${escapeHtml(text)}</span>`;
  }

  function closePlayer() {
    const player = state.player;
    if (!player) return;
    player.closed = true;
    try { player.ws && player.ws.close(); } catch (_) { /* ignore */ }
    try { player.video.pause(); } catch (_) { /* ignore */ }
    player.video.removeAttribute('src');
    try { player.video.load(); } catch (_) { /* ignore */ }
    URL.revokeObjectURL(player.objectUrl);
    state.player = null;
    $('#live-player-card').hidden = true;
  }

  // ---------------------------------------------------------------- 初始化

  function bind() {
    $('#live-start-screen').addEventListener('click', () => startBroadcast('screen'));
    $('#live-start-camera').addEventListener('click', () => startBroadcast('camera'));
    $('#live-stop').addEventListener('click', () => stopBroadcast());
    $('#live-refresh').addEventListener('click', () => loadRooms());
    $('#live-leave').addEventListener('click', () => closePlayer());
    $('#live-flip').addEventListener('click', () => flipCamera());
    $('#live-device').addEventListener('change', (event) => {
      if (state.cast && state.cast.source === 'camera') {
        switchCamera({ deviceId: event.target.value, facing: null });
      }
    });
    $('#live-quality').addEventListener('change', () => {
      if (state.cast && state.cast.source === 'camera' && !state.cast.flipping) {
        toast('画质已切换，正在重连画面…', 'ok');
        switchCamera({ deviceId: state.cast.deviceId, facing: state.cast.facing });
      }
    });

    if (isMobile()) {
      $('#live-facing').value = 'environment';
    }
    refreshCameras();

    setInterval(updateCastStats, 500);
    // 直播列表定时刷新（只在直播页可见时发请求）
    setInterval(() => {
      const panel = document.querySelector('.panel[data-panel="live"]');
      if (panel && !panel.hidden) loadRooms();
    }, 5000);
  }

  bindTabs();
  describeSupport();
  bind();

  window.addEventListener('beforeunload', () => {
    if (state.player) closePlayer();
    // 页面被关掉/刷新时无法再带 x-live-key 头调用 stop 接口，
    // 交给服务端的空闲超时来收尾（会把已录部分保存成文件）。
    try { state.cast && state.cast.recorder.stop(); } catch (_) { /* ignore */ }
  });

  // 暴露给自动化测试使用的最小接口
  window.__lanShareLive = {
    startBroadcast,
    stopBroadcast,
    watchRoom,
    closePlayer,
    loadRooms,
    get rooms() { return state.rooms; },
    get cast() { return state.cast; },
    get player() { return state.player; },
  };
})();
