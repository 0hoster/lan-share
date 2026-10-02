/* lan-share 前端：分片并行上传 + Range 并行下载 */
(() => {
  'use strict';

  const $ = (sel) => document.querySelector(sel);

  const state = {
    token: new URLSearchParams(location.search).get('token') || localStorage.getItem('lan-share-token') || '',
    files: [],
    jobs: new Map(),
    ws: null,
    wsRetry: 0,
    serverChunkSize: 8 * 1024 * 1024,
    search: '',
  };

  if (state.token) localStorage.setItem('lan-share-token', state.token);

  // ---------------------------------------------------------------- 工具

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  function withToken(url) {
    if (!state.token) return url;
    return url + (url.includes('?') ? '&' : '?') + 'token=' + encodeURIComponent(state.token);
  }

  function authHeaders(extra) {
    const headers = Object.assign({}, extra);
    if (state.token) headers['Authorization'] = 'Bearer ' + state.token;
    return headers;
  }

  function fmtBytes(n) {
    if (!Number.isFinite(n) || n < 0) return '—';
    if (n < 1024) return n + ' B';
    const units = ['KB', 'MB', 'GB', 'TB'];
    let value = n / 1024;
    let i = 0;
    while (value >= 1024 && i < units.length - 1) { value /= 1024; i++; }
    return value.toFixed(value >= 100 ? 0 : 1) + ' ' + units[i];
  }

  const fmtSpeed = (n) => fmtBytes(n) + '/s';

  function fmtDuration(sec) {
    if (!Number.isFinite(sec) || sec < 0) return '—';
    if (sec < 60) return Math.ceil(sec) + ' 秒';
    if (sec < 3600) return Math.floor(sec / 60) + ' 分 ' + Math.round(sec % 60) + ' 秒';
    return Math.floor(sec / 3600) + ' 小时 ' + Math.round((sec % 3600) / 60) + ' 分';
  }

  function fmtTime(unixSecs) {
    const date = new Date(unixSecs * 1000);
    const diff = (Date.now() - date.getTime()) / 1000;
    if (diff < 60) return '刚刚';
    if (diff < 3600) return Math.floor(diff / 60) + ' 分钟前';
    if (diff < 86400) return Math.floor(diff / 3600) + ' 小时前';
    return date.toLocaleString('zh-CN', { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' });
  }

  function escapeHtml(str) {
    return String(str).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  }

  function fileIcon(mime, name) {
    const m = (mime || '') + name;
    if (/video\//.test(mime)) return '🎬';
    if (/audio\//.test(mime)) return '🎵';
    if (/^image\//.test(mime)) return '🖼';
    if (/zip|rar|7z|tar|gz/i.test(m)) return '🗜';
    if (/pdf/i.test(m)) return '📕';
    if (/text|json|xml|javascript|rust|python/i.test(m)) return '📄';
    if (/sheet|excel/i.test(m)) return '📊';
    return '📦';
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
    }, 3600);
  }

  async function parseError(resp) {
    try {
      const data = await resp.json();
      if (data && data.error) return data.error;
    } catch (_) { /* ignore */ }
    return `HTTP ${resp.status}`;
  }

  // ---------------------------------------------------------------- 状态栏

  function setWsStatus(status) {
    const dot = $('#ws-dot');
    dot.classList.remove('on', 'off');
    if (status === 'on') { dot.classList.add('on'); $('#ws-text').textContent = '实时同步'; }
    else if (status === 'off') { dot.classList.add('off'); $('#ws-text').textContent = '已断开，重连中…'; }
    else { $('#ws-text').textContent = '连接中…'; }
  }

  async function refreshStats() {
    try {
      const resp = await fetch(withToken('/api/stats'), { headers: authHeaders() });
      if (!resp.ok) return;
      const stats = await resp.json();
      $('#stats-text').textContent = `${stats.files} 个文件 · ${fmtBytes(stats.bytes)}`;
      state.serverChunkSize = stats.chunk_size || state.serverChunkSize;
      $('#chunk-hint').textContent = '分片 ' + fmtBytes(state.serverChunkSize);
      $('#foot-info').textContent =
        `服务端分片 ${fmtBytes(stats.chunk_size)} · 已运行 ${fmtDuration(stats.uptime_secs)} · lan-share`;
    } catch (_) { /* 忽略 */ }
  }

  // ---------------------------------------------------------------- WebSocket

  function connectWs() {
    if (!window.WebSocket) { setWsStatus('off'); return; }
    const proto = location.protocol === 'https:' ? 'wss://' : 'ws://';
    const url = proto + location.host + '/ws' + (state.token ? '?token=' + encodeURIComponent(state.token) : '');

    let socket;
    try {
      socket = new WebSocket(url);
    } catch (_) {
      setWsStatus('off');
      return;
    }
    state.ws = socket;

    socket.onopen = () => { state.wsRetry = 0; setWsStatus('on'); };
    socket.onmessage = (event) => {
      let data;
      try { data = JSON.parse(event.data); } catch (_) { return; }
      switch (data.type) {
        case 'file_added':
        case 'upload_finished':
          loadFiles();
          refreshStats();
          break;
        case 'file_removed':
        case 'upload_aborted':
          loadFiles();
          refreshStats();
          break;
        default:
          break;
      }
    };
    socket.onclose = () => {
      setWsStatus('off');
      state.wsRetry = Math.min(state.wsRetry + 1, 6);
      setTimeout(connectWs, 1000 * state.wsRetry);
    };
    socket.onerror = () => { try { socket.close(); } catch (_) { /* ignore */ } };
  }

  // ---------------------------------------------------------------- 文件列表

  async function loadFiles() {
    try {
      const resp = await fetch(withToken('/api/files'), { headers: authHeaders() });
      if (!resp.ok) throw new Error(await parseError(resp));
      state.files = await resp.json();
      renderFiles();
    } catch (err) {
      toast('读取文件列表失败：' + err.message, 'err');
    }
  }

  function renderFiles() {
    const keyword = state.search.trim().toLowerCase();
    const list = state.files.filter((f) => !keyword || f.name.toLowerCase().includes(keyword));
    const ul = $('#file-list');
    ul.innerHTML = '';
    $('#file-count').textContent = state.files.length ? `(${state.files.length})` : '';
    $('#empty-state').hidden = list.length > 0;
    if (!list.length && state.files.length) {
      $('#empty-state').hidden = false;
      $('#empty-state').textContent = '没有匹配的文件。';
      return;
    }
    if (!list.length) {
      $('#empty-state').textContent = '还没有文件，拖一个上来试试。';
    }

    for (const file of list) {
      const li = document.createElement('li');
      li.className = 'row';
      li.innerHTML = `
        <span class="icon">${fileIcon(file.mime, file.name)}</span>
        <div class="grow">
          <div class="name" title="${escapeHtml(file.name)}">${escapeHtml(file.name)}</div>
          <div class="meta"><span>${fmtBytes(file.size)}</span><span>${fmtTime(file.created_at)}</span></div>
          <div class="bar" hidden><i></i></div>
        </div>
        <div class="actions">
          <button class="act primary" data-download>下载</button>
          <button class="act" data-open>直链</button>
          <button class="act" data-copy>复制</button>
          <button class="act danger" data-delete>删除</button>
        </div>`;

      const bar = li.querySelector('.bar');
      const barFill = li.querySelector('.bar > i');
      const meta = li.querySelector('.meta');
      const baseMeta = meta.innerHTML;

      li.querySelector('[data-download]').onclick = (e) => startDownload(file, e.currentTarget, { bar, barFill, meta, baseMeta });
      li.querySelector('[data-open]').onclick = () => window.open(withToken(`/api/files/${file.id}/download?inline=1`), '_blank');
      li.querySelector('[data-copy]').onclick = async () => {
        const url = location.origin + withToken(`/api/files/${file.id}/download`);
        try {
          await navigator.clipboard.writeText(url);
          toast('下载链接已复制', 'ok');
        } catch (_) {
          window.prompt('复制下载链接：', url);
        }
      };
      li.querySelector('[data-delete]').onclick = () => deleteFile(file);
      ul.appendChild(li);
    }
  }

  async function deleteFile(file) {
    if (!confirm(`确定删除「${file.name}」？此操作不可恢复。`)) return;
    try {
      const resp = await fetch(withToken('/api/files/' + file.id), { method: 'DELETE', headers: authHeaders() });
      if (!resp.ok) throw new Error(await parseError(resp));
      toast('已删除 ' + file.name, 'ok');
      loadFiles();
      refreshStats();
    } catch (err) {
      toast('删除失败：' + err.message, 'err');
    }
  }

  // ---------------------------------------------------------------- 上传

  async function addFiles(fileList) {
    const files = Array.from(fileList || []);
    if (!files.length) return;
    $('#tasks-card').hidden = false;

    const concurrency = Number($('#upload-concurrency').value) || 4;
    // 多个文件时依次排队，每个文件内部使用并发分片
    for (const file of files) {
      const job = createUploadJob(file);
      uploadJob(job, concurrency).catch(() => { /* 错误已在 UI 中体现 */ });
    }
  }

  function createUploadJob(file) {
    const job = {
      kind: 'upload',
      key: 'u-' + Math.random().toString(36).slice(2),
      file,
      name: file.name,
      size: file.size,
      status: 'queued',
      loaded: 0,
      speed: 0,
      lastLoaded: 0,
      lastTs: performance.now(),
      chunkLoaded: null,
      aborted: false,
      xhrs: new Set(),
      uploadId: null,
    };
    state.jobs.set(job.key, job);

    const li = document.createElement('li');
    li.className = 'row';
    li.innerHTML = `
      <span class="icon">⬆</span>
      <div class="grow">
        <div class="name" title="${escapeHtml(job.name)}">${escapeHtml(job.name)}</div>
        <div class="meta"><span class="status-text">排队中…</span></div>
        <div class="bar"><i></i></div>
      </div>
      <div class="actions"><button class="act danger" data-cancel>取消</button></div>`;
    li.querySelector('[data-cancel]').onclick = () => cancelUpload(job);
    $('#task-list').prepend(li);

    job.el = li;
    job.statusText = li.querySelector('.status-text');
    job.barFill = li.querySelector('.bar > i');
    updateJobRow(job);
    return job;
  }

  async function uploadJob(job, concurrency) {
    try {
      const initResp = await fetch(withToken('/api/upload/init'), {
        method: 'POST',
        headers: authHeaders({ 'Content-Type': 'application/json' }),
        body: JSON.stringify({ name: job.name, size: job.size }),
      });
      if (!initResp.ok) throw new Error(await parseError(initResp));
      const init = await initResp.json();

      job.uploadId = init.upload_id;
      job.chunkSize = init.chunk_size;
      job.chunkCount = init.chunk_count;
      job.chunkLoaded = new Float64Array(init.chunk_count);
      const alreadyReceived = new Set(init.received || []);
      for (const idx of alreadyReceived) job.chunkLoaded[idx] = chunkExpectedLen(job, idx);
      recalcLoaded(job);
      job.status = 'uploading';
      updateJobRow(job);

      const pending = [];
      for (let i = 0; i < init.chunk_count; i++) {
        if (alreadyReceived.has(i)) continue;
        pending.push(i);
      }

      let cursor = 0;
      const worker = async () => {
        while (!job.aborted) {
          const index = cursor++;
          if (index >= pending.length) return;
          await uploadChunk(job, pending[index]);
        }
      };
      const lanes = Math.max(1, Math.min(concurrency, pending.length));
      await Promise.all(Array.from({ length: lanes }, worker));

      if (job.aborted) return;

      const completeResp = await fetch(withToken(`/api/upload/${job.uploadId}/complete`), {
        method: 'POST',
        headers: authHeaders(),
      });
      if (!completeResp.ok) throw new Error(await parseError(completeResp));

      job.status = 'done';
      job.loaded = job.size;
      updateJobRow(job);
      toast(`上传完成：${job.name}`, 'ok');
      loadFiles();
      refreshStats();
    } catch (err) {
      if (job.aborted) return;
      job.status = 'error';
      job.error = err.message;
      updateJobRow(job);
      toast(`上传失败（${job.name}）：${err.message}`, 'err');
    }
  }

  function chunkExpectedLen(job, index) {
    const start = index * job.chunkSize;
    if (start >= job.size) return 0;
    return Math.min(job.chunkSize, job.size - start);
  }

  async function uploadChunk(job, index) {
    const start = index * job.chunkSize;
    const end = Math.min(job.size, start + job.chunkSize);
    const blob = job.file.slice(start, end);
    const url = withToken(`/api/upload/${job.uploadId}/chunk/${index}`);

    let attempt = 0;
    for (;;) {
      if (job.aborted) return;
      try {
        await putChunk(job, url, blob);
        setChunkLoaded(job, index, blob.size);
        return;
      } catch (err) {
        if (job.aborted) return;
        attempt++;
        if (attempt >= 3) throw new Error(`分片 ${index + 1} 失败：${err.message}`);
        await sleep(400 * attempt);
      }
    }
  }

  function recalcLoaded(job) {
    let total = 0;
    for (let i = 0; i < job.chunkLoaded.length; i++) total += job.chunkLoaded[i];
    job.loaded = total;
  }

  /** 更新某个分片已上传字节数，并同步总进度（兼容重试导致的回退）。 */
  function setChunkLoaded(job, index, value) {
    const prev = job.chunkLoaded[index] || 0;
    job.chunkLoaded[index] = value;
    job.loaded = Math.max(0, job.loaded + value - prev);
  }

  function putChunk(job, url, blob) {
    return new Promise((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      job.xhrs.add(xhr);
      xhr.open('PUT', url, true);
      xhr.setRequestHeader('Content-Type', 'application/octet-stream');
      if (state.token) xhr.setRequestHeader('Authorization', 'Bearer ' + state.token);

      const index = Number(url.slice(url.lastIndexOf('/') + 1).split('?')[0]);
      xhr.upload.addEventListener('progress', (event) => {
        if (!job.chunkLoaded) return;
        setChunkLoaded(job, index, Math.min(event.loaded, blob.size));
      });

      const finish = (fn) => (arg) => {
        job.xhrs.delete(xhr);
        fn(arg);
      };
      xhr.addEventListener('load', finish(() => {
        if (xhr.status >= 200 && xhr.status < 300) resolve();
        else reject(new Error(safeErrorMessage(xhr)));
      }));
      xhr.addEventListener('error', finish(() => reject(new Error('网络中断'))));
      xhr.addEventListener('abort', finish(() => reject(new Error('已取消'))));
      xhr.addEventListener('timeout', finish(() => reject(new Error('超时'))));
      xhr.send(blob);
    });
  }

  function safeErrorMessage(xhr) {
    try {
      const data = JSON.parse(xhr.responseText);
      if (data && data.error) return data.error;
    } catch (_) { /* ignore */ }
    return 'HTTP ' + xhr.status;
  }

  async function cancelUpload(job) {
    job.aborted = true;
    job.status = 'error';
    job.error = '已取消';
    for (const xhr of job.xhrs) { try { xhr.abort(); } catch (_) { /* ignore */ } }
    job.xhrs.clear();
    if (job.uploadId) {
      try {
        await fetch(withToken('/api/upload/' + job.uploadId), { method: 'DELETE', headers: authHeaders() });
      } catch (_) { /* ignore */ }
    }
    updateJobRow(job);
  }

  function updateJobRow(job) {
    if (!job.el) return;
    const percent = job.size ? Math.min(100, (job.loaded / job.size) * 100) : 100;
    job.barFill.style.width = percent.toFixed(1) + '%';
    job.el.classList.toggle('done', job.status === 'done');
    job.el.classList.toggle('error', job.status === 'error');

    if (job.status === 'queued') {
      job.statusText.textContent = '排队中…';
    } else if (job.status === 'uploading') {
      job.statusText.textContent = `${fmtBytes(job.loaded)} / ${fmtBytes(job.size)} · ${percent.toFixed(0)}% · ${fmtSpeed(job.speed)}`;
    } else if (job.status === 'done') {
      job.statusText.textContent = `完成 · ${fmtBytes(job.size)}`;
      job.el.querySelector('[data-cancel]')?.remove();
    } else if (job.status === 'error') {
      job.statusText.textContent = '失败：' + (job.error || '未知错误');
    }
  }

  // ---------------------------------------------------------------- 下载

  const PARALLEL_LIMIT = 1.5 * 1024 * 1024 * 1024; // 超过 1.5GB 走浏览器原生下载

  async function startDownload(file, button, ui) {
    const concurrency = Number($('#download-concurrency').value) || 4;
    const url = withToken(`/api/files/${file.id}/download`);

    if (file.size > PARALLEL_LIMIT) {
      toast('文件较大，改用浏览器原生下载（支持断点续传）', 'ok');
      const a = document.createElement('a');
      a.href = url;
      a.download = file.name;
      document.body.appendChild(a);
      a.click();
      a.remove();
      return;
    }

    const original = button.textContent;
    button.disabled = true;
    button.textContent = '下载中';
    ui.bar.hidden = false;

    const job = { loaded: 0, speed: 0, lastLoaded: 0, lastTs: performance.now() };
    const timer = setInterval(() => {
      const now = performance.now();
      const dt = (now - job.lastTs) / 1000;
      if (dt > 0.2) {
        job.speed = (job.loaded - job.lastLoaded) / dt;
        job.lastLoaded = job.loaded;
        job.lastTs = now;
        updateDownloadMeta(file, job, ui);
      }
    }, 500);

    try {
      await fetchParallel(file, url, concurrency, job, ui);
      toast('下载完成：' + file.name, 'ok');
    } catch (err) {
      toast('下载失败：' + err.message, 'err');
      // 回退到原生下载
      const a = document.createElement('a');
      a.href = url;
      a.download = file.name;
      document.body.appendChild(a);
      a.click();
      a.remove();
    } finally {
      clearInterval(timer);
      ui.bar.hidden = true;
      ui.meta.innerHTML = ui.baseMeta;
      button.disabled = false;
      button.textContent = original;
    }
  }

  function updateDownloadMeta(file, job, ui) {
    const percent = file.size ? (job.loaded / file.size) * 100 : 100;
    const eta = job.speed > 0 ? (file.size - job.loaded) / job.speed : Infinity;
    ui.barFill.style.width = percent.toFixed(1) + '%';
    ui.meta.innerHTML =
      `<span>${fmtBytes(job.loaded)} / ${fmtBytes(file.size)}</span>` +
      `<span>${percent.toFixed(0)}%</span>` +
      `<span>${fmtSpeed(job.speed)}</span>` +
      `<span>剩余 ${fmtDuration(eta)}</span>`;
  }

  async function fetchParallel(file, url, concurrency, job, ui) {
    const head = await fetch(url, { method: 'HEAD', headers: authHeaders() });
    if (!head.ok) throw new Error(await parseError(head));

    const total = Number(head.headers.get('Content-Length')) || file.size;
    const rangesSupported = (head.headers.get('Accept-Ranges') || '').includes('bytes');
    if (!rangesSupported || total === 0) {
      return nativeDownload(url, file);
    }

    const chunkSize = Math.max(1024 * 1024, Math.min(state.serverChunkSize, 32 * 1024 * 1024));
    const count = Math.ceil(total / chunkSize);
    const buffer = new Uint8Array(total);

    let cursor = 0;
    const worker = async () => {
      for (;;) {
        const index = cursor++;
        if (index >= count) return;
        const start = index * chunkSize;
        const end = Math.min(total, start + chunkSize) - 1;

        const resp = await fetch(url, {
          headers: authHeaders({ 'Range': `bytes=${start}-${end}` }),
        });
        if (!resp.ok) throw new Error(await parseError(resp));

        const reader = resp.body && resp.body.getReader ? resp.body.getReader() : null;
        let offset = start;
        if (reader) {
          for (;;) {
            const { done, value } = await reader.read();
            if (done) break;
            buffer.set(value, offset);
            offset += value.length;
            job.loaded += value.length;
          }
        } else {
          const bytes = new Uint8Array(await resp.arrayBuffer());
          buffer.set(bytes, start);
          job.loaded += bytes.length;
        }
      }
    };

    const lanes = Math.max(1, Math.min(concurrency, count));
    await Promise.all(Array.from({ length: lanes }, worker));

    const blob = new Blob([buffer], { type: file.mime || 'application/octet-stream' });
    triggerBlobDownload(blob, file.name);
  }

  async function nativeDownload(url, file) {
    const a = document.createElement('a');
    a.href = url;
    a.download = file.name;
    document.body.appendChild(a);
    a.click();
    a.remove();
  }

  function triggerBlobDownload(blob, filename) {
    const href = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = href;
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(href), 60_000);
  }

  // ---------------------------------------------------------------- 事件绑定

  function bind() {
    const dz = $('#dropzone');
    const input = $('#file-input');

    dz.addEventListener('click', () => input.click());
    dz.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); input.click(); }
    });
    input.addEventListener('change', () => { addFiles(input.files); input.value = ''; });

    let depth = 0;
    dz.addEventListener('dragenter', (e) => { e.preventDefault(); depth++; dz.classList.add('dragover'); });
    dz.addEventListener('dragover', (e) => e.preventDefault());
    dz.addEventListener('dragleave', () => { if (--depth <= 0) { depth = 0; dz.classList.remove('dragover'); } });
    dz.addEventListener('drop', (e) => {
      e.preventDefault();
      depth = 0;
      dz.classList.remove('dragover');
      addFiles(e.dataTransfer && e.dataTransfer.files);
    });
    window.addEventListener('dragover', (e) => e.preventDefault());
    window.addEventListener('drop', (e) => e.preventDefault());

    $('#search').addEventListener('input', (e) => { state.search = e.target.value; renderFiles(); });
    $('#refresh').addEventListener('click', () => { loadFiles(); refreshStats(); });
    $('#clear-finished').addEventListener('click', () => {
      for (const [key, job] of state.jobs) {
        if (job.status === 'done' || job.status === 'error') {
          job.el?.remove();
          state.jobs.delete(key);
        }
      }
      if (!state.jobs.size) $('#tasks-card').hidden = true;
    });

    // 定期刷新速度显示
    setInterval(() => {
      const now = performance.now();
      for (const job of state.jobs.values()) {
        if (job.status !== 'uploading') continue;
        const dt = (now - job.lastTs) / 1000;
        if (dt < 0.4) continue;
        job.speed = Math.max(0, (job.loaded - job.lastLoaded) / dt);
        job.lastLoaded = job.loaded;
        job.lastTs = now;
        updateJobRow(job);
      }
    }, 600);
  }

  // ---------------------------------------------------------------- 启动

  bind();
  setWsStatus('connecting');
  loadFiles();
  refreshStats();
  connectWs();
  setInterval(refreshStats, 30_000);
})();
