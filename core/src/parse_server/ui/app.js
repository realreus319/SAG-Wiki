/* 文件解析服务 Web UI —— 纯静态单页，无构建、无外部依赖。
   API 契约：GET /api/settings 返回 {mineru, engines, docling, files,
   queue, asr, vision, logs}，字段名与后端一一对应；详见 docs-site
   的 parse-server 文档页。 */
'use strict';

const $ = (sel, root) => (root || document).querySelector(sel);
const $$ = (sel, root) => Array.from((root || document).querySelectorAll(sel));
const PAGE_SIZE = 20;
const POLL_MS = 2000;      // 任务进度轮询
const LOG_POLL_MS = 5000;  // 日志自动刷新

/* ---------- 小工具 ---------- */
function esc(s) {
  return String(s == null ? '' : s).replace(/[&<>"']/g, c => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));
}
function fmtBytes(n) {
  if (n == null || isNaN(n)) return '';
  if (n < 1024) return n + ' B';
  if (n < 1024 * 1024) return (n / 1024).toFixed(1) + ' KB';
  return (n / 1024 / 1024).toFixed(1) + ' MB';
}
function fmtDuration(ms) {
  if (ms == null || isNaN(ms) || ms < 0) return '—';
  const s = Math.round(ms / 1000);
  if (s < 60) return s + ' 秒';
  const m = Math.floor(s / 60);
  if (m < 60) return m + ' 分 ' + (s % 60) + ' 秒';
  return Math.floor(m / 60) + ' 小时 ' + (m % 60) + ' 分';
}
function fmtTime(iso) {
  if (!iso) return '—';
  const d = new Date(iso);
  if (isNaN(d)) return String(iso);
  return d.toLocaleString('zh-CN', { hour12: false });
}
function baseName(p) {
  if (!p) return '';
  const parts = String(p).split(/[\\/]/);
  return parts[parts.length - 1] || String(p);
}
function shortId(id) {
  const s = String(id || '');
  return s.length > 18 ? s.slice(0, 8) + '…' + s.slice(-6) : s;
}
function debounce(fn, ms) {
  let t = null;
  return (...args) => { clearTimeout(t); t = setTimeout(() => fn(...args), ms); };
}

let toastTimer = null;
function toast(msg) {
  const el = $('#toast');
  el.textContent = msg;
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { el.hidden = true; }, 4500);
}

/* ---------- API ---------- */
async function api(path, opts) {
  const res = await fetch(path, opts);
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch (e) { /* 非 JSON 响应 */ }
  if (!res.ok) {
    const msg = (data && (data.detail || data.error || data.message)) || text || ('HTTP ' + res.status);
    const err = new Error(String(msg));
    err.status = res.status;
    err.data = data;
    throw err;
  }
  return data;
}
function apiErrorText(err) {
  return err && err.message ? err.message : String(err);
}

/* ---------- 状态 ---------- */
const state = {
  route: 'tasks',
  detailId: null,
  detailTask: null,
  detailMarkdown: null,
  tasksPage: 1,
  tasksTotalPages: 1,
  tasksApiFallback: false,   // /api/tasks 不可用时改走参考实现 /ui/tasks
  settings: null,            // 归一化后的设置对象
  settingsRaw: null,
  keys: [],
  logs: [],
  pickedFiles: [],           // File[]
  uploading: false,
  detailView: 'rendered',
  inFlight: false,
};

/* ---------- 路由（hash） ---------- */
function navigate(hash) {
  if (location.hash === hash) { route(); return; }
  location.hash = hash;
}
function route() {
  const h = (location.hash || '#/tasks').replace(/^#\/?/, '');
  const parts = h.split('/');
  $$('.nav button').forEach(b => b.classList.toggle('active', b.dataset.nav === parts[0]));
  $$('.page').forEach(p => { p.hidden = true; });
  if (parts[0] === 'tasks' && parts[1]) {
    state.route = 'detail';
    state.detailId = decodeURIComponent(parts[1]);
    $('#page-detail').hidden = false;
    loadDetail(true);
  } else if (parts[0] === 'settings') {
    state.route = 'settings';
    $('#page-settings').hidden = false;
    if (!state.settings) loadSettings(); else renderSettings();
  } else if (parts[0] === 'logs') {
    state.route = 'logs';
    $('#page-logs').hidden = false;
    loadLogs();
  } else {
    state.route = 'tasks';
    state.detailId = null;
    $('#page-tasks').hidden = false;
    loadTasks();
  }
  window.scrollTo(0, 0);
}
window.addEventListener('hashchange', route);

/* ---------- 健康检查 ---------- */
async function checkHealth() {
  const dot = $('.health-dot');
  const text = $('#health-text');
  try {
    const h = await api('/health');
    dot.dataset.state = 'ok';
    const counts = h && h.tasks ? Object.entries(h.tasks).map(([k, v]) => k + ' ' + v).join(' / ') : '';
    text.textContent = '服务正常' + (counts ? ' · ' + counts : '') + (h && h.service ? ' · ' + h.service : '');
  } catch (e) {
    dot.dataset.state = 'down';
    text.textContent = '服务不可达：' + apiErrorText(e);
  }
}

/* ---------- 任务归一化 ---------- */
const STATUS_LABEL = { pending: '待处理', processing: '解析中', completed: '已完成', failed: '失败' };
function normStatus(s) {
  s = String(s || 'unknown').toLowerCase();
  if (s === 'done' || s === 'success') return 'completed';
  if (s === 'error') return 'failed';
  if (s === 'running') return 'processing';
  return ['pending', 'processing', 'completed', 'failed'].includes(s) ? s : 'unknown';
}
function normTask(t) {
  t = t || {};
  const created = t.created_at || t.submitted_at || t.createdAt || null;
  const finished = t.finished_at || t.completed_at || t.updated_at || null;
  let duration = t.duration_ms ?? t.elapsed_ms ?? t.duration ?? null;
  if (duration == null && created) {
    const end = finished ? new Date(finished) : new Date();
    duration = end - new Date(created);
    if (isNaN(duration) || duration < 0) duration = null;
  }
  return {
    id: t.task_id || t.id || '',
    status: normStatus(t.status),
    engine: t.engine || t.current_engine || '—',
    input: t.input_file || t.filename || t.input || '',
    output: t.output_file || t.output || '',
    clientId: t.client_id || t.clientId || '',
    createdAt: created,
    finishedAt: finished,
    durationMs: duration,
    error: t.error || t.error_message || '',
    prompt: t.prompt || '',
    progress: t.progress ?? t.extract_progress ?? null,   // 0..1 / 0..100 / {current,total}
    remoteState: t.remote_state || '',
    attempts: t.attempts || t.engine_chain || t.engine_attempts || t.steps || null,
    raw: t,
  };
}
function progressOf(task) {
  const p = task.progress;
  if (p == null || p === '') return null;
  if (typeof p === 'object') {
    if (p.total) return { pct: Math.round(100 * p.current / p.total), text: '第 ' + p.current + ' / ' + p.total + ' 段' };
    return null;
  }
  const n = Number(p);
  if (isNaN(n)) return null;
  if (n <= 1) return { pct: Math.round(n * 100), text: Math.round(n * 100) + '%' };
  return { pct: Math.min(100, Math.round(n)), text: Math.min(100, Math.round(n)) + '%' };
}

/* ---------- 任务总览 ---------- */
function taskFilters() {
  return {
    status: $('#filter-status').value,
    client_id: $('#filter-client').value.trim(),
    search: $('#filter-search').value.trim(),
  };
}
async function fetchTaskList(page) {
  const f = taskFilters();
  const params = new URLSearchParams({ page: String(page), page_size: String(PAGE_SIZE) });
  if (f.status) params.set('status', f.status);
  if (f.client_id) params.set('client_id', f.client_id);
  if (f.search) params.set('search', f.search);
  try {
    const data = await api('/api/tasks?' + params.toString());
    return data;
  } catch (e) {
    // 参考实现（mark-files-down）把列表挂在 /ui/tasks，且 status 用中文“全部”
    if ((e.status === 404 || e.status === 405) && !state.tasksApiFallback) {
      state.tasksApiFallback = true;
      const p2 = new URLSearchParams({ page: String(page), page_size: String(PAGE_SIZE), search: f.search });
      p2.set('status', f.status ? f.status : '全部');
      return await api('/ui/tasks?' + p2.toString());
    }
    throw e;
  }
}
async function loadTasks() {
  const listEl = $('#task-list');
  const loading = $('#tasks-loading'), empty = $('#tasks-empty'), errEl = $('#tasks-error');
  if (state.inFlight) return;
  state.inFlight = true;
  const firstLoad = listEl.children.length === 0;
  if (firstLoad) { loading.hidden = false; empty.hidden = true; errEl.hidden = true; }
  try {
    const data = await fetchTaskList(state.tasksPage);
    const arr = Array.isArray(data) ? data : (data.tasks || data.items || []);
    const total = data.total != null ? data.total : arr.length;
    state.tasksTotalPages = data.total_pages || Math.max(1, Math.ceil(total / PAGE_SIZE));
    state.tasksPage = data.page || state.tasksPage;
    loading.hidden = true; errEl.hidden = true;
    if (!arr.length) {
      empty.hidden = false; listEl.innerHTML = '';
    } else {
      empty.hidden = true;
      renderTaskList(arr.map(normTask));
    }
    const pager = $('#pager');
    pager.hidden = state.tasksTotalPages <= 1 && !arr.length;
    $('#page-info').textContent = '第 ' + state.tasksPage + ' / ' + state.tasksTotalPages + ' 页，共 ' + total + ' 个任务';
    $('#page-prev').disabled = state.tasksPage <= 1;
    $('#page-next').disabled = state.tasksPage >= state.tasksTotalPages;
  } catch (e) {
    loading.hidden = true;
    errEl.hidden = false;
    errEl.textContent = '加载任务列表失败：' + apiErrorText(e);
  } finally {
    state.inFlight = false;
  }
}
function renderTaskList(tasks) {
  const listEl = $('#task-list');
  listEl.innerHTML = '';
  for (const t of tasks) {
    const li = document.createElement('li');
    li.className = 'task-item';
    li.tabIndex = 0;
    li.setAttribute('role', 'button');
    const prog = progressOf(t);
    const pct = t.status === 'completed' ? 100 : (prog ? prog.pct : (t.status === 'processing' ? null : 0));
    const meta = [
      baseName(t.input) || t.id,
      '引擎 ' + t.engine,
      t.clientId ? 'client ' + t.clientId : null,
      fmtTime(t.createdAt),
      t.durationMs != null ? '耗时 ' + fmtDuration(t.durationMs) : null,
    ].filter(Boolean).map(x => '<span>' + esc(x) + '</span>').join('');
    li.innerHTML =
      '<div><div class="task-name">' + esc(baseName(t.input) || shortId(t.id)) + '</div>' +
      '<div class="task-meta">' + meta + '</div></div>' +
      '<div class="task-side"><span class="badge" data-status="' + esc(t.status) + '">' +
      esc(STATUS_LABEL[t.status] || t.status) + '</span>' +
      '<span class="task-actions">' +
      (t.status === 'failed' || t.status === 'completed'
        ? '<button type="button" class="outline" data-act="restart">重启</button>' : '') +
      (t.status === 'pending' || t.status === 'processing'
        ? '<button type="button" class="outline" data-act="cancel">取消</button>' : '') +
      '</span></div>' +
      (pct != null
        ? '<div class="progress-track" title="' + (prog ? esc(prog.text) : '') + '"><div class="progress-fill" style="width:' + pct + '%"></div></div>'
        : '<div class="progress-track" title="进行中，进度未知"><div class="progress-fill" style="width:30%"></div></div>') +
      (t.error ? '<div class="task-error">' + esc(t.error) + '</div>' : '');
    li.addEventListener('click', ev => {
      const actBtn = ev.target.closest('[data-act]');
      if (actBtn) {
        ev.stopPropagation();
        if (actBtn.dataset.act === 'restart') restartTask(t.id, li);
        if (actBtn.dataset.act === 'cancel') cancelTask(t.id, li);
        return;
      }
      navigate('#/tasks/' + encodeURIComponent(t.id));
    });
    li.addEventListener('keydown', ev => {
      if (ev.key === 'Enter') navigate('#/tasks/' + encodeURIComponent(t.id));
    });
    listEl.appendChild(li);
  }
}
async function restartTask(id, scopeEl) {
  try {
    await api('/tasks/' + encodeURIComponent(id) + '/restart', { method: 'POST' });
    toast('已提交重启：' + shortId(id));
    if (state.route === 'tasks') loadTasks(); else loadDetail(true);
  } catch (e) {
    toast('重启失败：' + apiErrorText(e));
  }
}
async function cancelTask(id, scopeEl) {
  // 契约里没有取消接口：先试 DELETE，失败则明确告知（见 frontend-notes.md 缺口）
  try {
    await api('/tasks/' + encodeURIComponent(id), { method: 'DELETE' });
    toast('已取消：' + shortId(id));
    if (state.route === 'tasks') loadTasks(); else loadDetail(true);
  } catch (e) {
    toast('取消失败：后端暂未提供取消接口（' + apiErrorText(e) + '），任务会继续跑完或失败。');
  }
}

/* ---------- 上传 ---------- */
function renderPicks() {
  const ul = $('#file-picks');
  ul.innerHTML = '';
  ul.hidden = state.pickedFiles.length === 0;
  $('#upload-btn').disabled = state.pickedFiles.length === 0 || state.uploading;
  state.pickedFiles.forEach((f, i) => {
    const li = document.createElement('li');
    const name = document.createElement('span');
    name.textContent = f.name;
    const meta = document.createElement('span');
    meta.textContent = fmtBytes(f.size);
    const rm = document.createElement('button');
    rm.type = 'button'; rm.className = 'pick-remove'; rm.textContent = '移除';
    rm.addEventListener('click', () => { state.pickedFiles.splice(i, 1); renderPicks(); });
    li.append(name, meta, rm);
    ul.appendChild(li);
  });
}
function addFiles(files) {
  for (const f of files) {
    if (!state.pickedFiles.some(x => x.name === f.name && x.size === f.size)) state.pickedFiles.push(f);
  }
  renderPicks();
}
async function submitUpload() {
  if (!state.pickedFiles.length || state.uploading) return;
  const statusEl = $('#upload-status');
  const fd = new FormData();
  for (const f of state.pickedFiles) fd.append('files', f, f.name);
  const clientId = $('#client-id').value.trim();
  if (clientId) fd.append('client_id', clientId);
  state.uploading = true;
  renderPicks();
  statusEl.textContent = '正在上传 ' + state.pickedFiles.length + ' 个文件…';
  try {
    const data = await api('/convert', { method: 'POST', body: fd });
    const results = (data && data.results) || [];
    const ok = results.filter(r => r.status === 'success' && r.task_id);
    const bad = results.filter(r => r.status !== 'success' || !r.task_id);
    if (ok.length) {
      statusEl.textContent = '已提交 ' + ok.length + ' 个任务' + (bad.length ? '，' + bad.length + ' 个失败' : '') + '。';
      state.pickedFiles = [];
      renderPicks();
      state.tasksPage = 1;
      loadTasks();
      if (ok.length === 1) navigate('#/tasks/' + encodeURIComponent(ok[0].task_id));
    } else {
      statusEl.textContent = bad.length ? '提交失败：' + (bad[0].message || bad[0].error || '未知错误') : '提交失败：后端未返回任务。';
    }
    if (bad.length) bad.forEach(b => toast('提交失败 ' + (b.filename || '') + '：' + (b.message || b.error || '未知错误')));
  } catch (e) {
    statusEl.textContent = '提交失败：' + apiErrorText(e);
  } finally {
    state.uploading = false;
    renderPicks();
  }
}

/* ---------- 任务详情 ---------- */
function attemptsOf(task) {
  const raw = task.attempts;
  if (Array.isArray(raw) && raw.length) {
    return raw.map(a => ({
      engine: a.engine || a.name || '—',
      status: normStatus(a.status || (a.error ? 'failed' : (a.ok === false ? 'failed' : 'completed'))),
      durationMs: a.duration_ms ?? a.elapsed_ms ?? a.duration ?? null,
      error: a.error || a.error_message || '',
      note: a.note || a.detail || '',
      startedAt: a.started_at || null,
    }));
  }
  // 后端若只给当前引擎：展示单步，避免凭空编造尝试链
  return [{
    engine: task.engine || '—',
    status: task.status,
    durationMs: task.durationMs,
    error: task.error || '',
    note: task.remoteState ? ('远端状态：' + task.remoteState) : '',
    startedAt: task.createdAt,
  }];
}
async function loadDetail(first) {
  const id = state.detailId;
  if (!id) return;
  const loading = $('#detail-loading'), errEl = $('#detail-error'), body = $('#detail-body');
  if (first) { loading.hidden = false; errEl.hidden = true; body.hidden = true; }
  try {
    const data = await api('/tasks/' + encodeURIComponent(id));
    const task = normTask(data && data.task ? data.task : data);
    state.detailTask = task;
    loading.hidden = true; errEl.hidden = true; body.hidden = false;
    renderDetail(task);
    if (task.status === 'completed' && state.detailMarkdown == null) {
      await loadResult(id);
    } else if (task.status !== 'completed') {
      state.detailMarkdown = null;
      renderResult();
    }
  } catch (e) {
    loading.hidden = true;
    errEl.hidden = false;
    errEl.textContent = '加载任务失败：' + apiErrorText(e) + (e.status === 404 ? '（任务不存在或已被清理）' : '');
  }
}
async function loadResult(id) {
  const errEl = $('#result-error');
  errEl.hidden = true;
  try {
    const data = await api('/tasks/' + encodeURIComponent(id) + '/result');
    if (data && data.status === 'failed') {
      state.detailMarkdown = null;
      errEl.hidden = false;
      errEl.textContent = '任务失败：' + (data.error || '未知错误');
    } else {
      state.detailMarkdown = (data && (data.markdown != null ? data.markdown : data.content)) || '';
    }
  } catch (e) {
    state.detailMarkdown = null;
    errEl.hidden = false;
    errEl.textContent = '读取结果失败：' + apiErrorText(e);
  }
  renderResult();
}
function renderDetail(t) {
  $('#detail-title').textContent = baseName(t.input) || shortId(t.id);
  $('#detail-sub').textContent = t.id;
  const badge = $('#detail-badge');
  badge.dataset.status = t.status;
  badge.textContent = STATUS_LABEL[t.status] || t.status;
  $('#detail-status-desc').textContent =
    t.status === 'completed' ? '解析完成，结果见下方。'
    : t.status === 'failed' ? '解析失败，错误原文见引擎尝试链。'
    : t.status === 'processing' ? '正在解析，每 2 秒自动刷新。'
    : '已提交，等待执行。';
  $('#detail-engine').textContent = t.engine;
  const prog = progressOf(t);
  $('#detail-progress-text').textContent =
    t.status === 'completed' ? '100%'
    : prog ? prog.text
    : t.status === 'processing' ? '进行中（进度未知）' : '—';
  $('#detail-progress-desc').textContent =
    prog && prog.text.includes('/') ? 'PDF 分段进度：' + prog.text
    : t.status === 'processing' ? '后端未返回分段进度时只显示状态。' : '';
  $('#detail-progress-fill').style.width =
    (t.status === 'completed' ? 100 : prog ? prog.pct : t.status === 'processing' ? 30 : 0) + '%';

  // 引擎尝试链
  const box = $('#attempts');
  box.innerHTML = '';
  const attempts = attemptsOf(t);
  $('#attempts-empty').hidden = attempts.length > 0;
  for (const a of attempts) {
    const div = document.createElement('div');
    div.className = 'attempt';
    const head = document.createElement('div');
    head.className = 'attempt-head';
    const name = document.createElement('strong');
    name.textContent = a.engine;
    const right = document.createElement('span');
    const b = document.createElement('span');
    b.className = 'badge'; b.dataset.status = a.status;
    b.textContent = STATUS_LABEL[a.status] || a.status;
    const tm = document.createElement('span');
    tm.className = 'attempt-time';
    tm.textContent = a.durationMs != null ? fmtDuration(a.durationMs) : '';
    right.append(b, document.createTextNode(' '), tm);
    head.append(name, right);
    div.appendChild(head);
    if (a.note) { const p = document.createElement('p'); p.className = 'attempt-note'; p.textContent = a.note; div.appendChild(p); }
    if (a.error) { const p = document.createElement('p'); p.className = 'attempt-error'; p.textContent = a.error; div.appendChild(p); }
    box.appendChild(div);
  }

  // 调用记录
  $('#detail-client').textContent = t.clientId || '—';
  $('#detail-created').textContent = fmtTime(t.createdAt);
  $('#detail-duration').textContent = t.durationMs != null ? fmtDuration(t.durationMs) : '—';
  $('#detail-input').textContent = t.input || '—';
  $('#detail-output').textContent = t.output || '—';
  $('#detail-taskid').textContent = t.id;
  $('#detail-restart').disabled = !(t.status === 'failed' || t.status === 'completed');
}
function renderResult() {
  const md = state.detailMarkdown;
  const has = md != null && md !== '';
  $('#result-empty').hidden = has;
  $('#md-rendered').hidden = !(has && state.detailView === 'rendered');
  $('#md-source').hidden = !(has && state.detailView === 'source');
  $('#copy-md').disabled = !has;
  $('#download-md').disabled = !has;
  if (has) {
    $('#md-rendered').innerHTML = renderMarkdown(md);
    $('#md-source').textContent = md;
  } else {
    $('#md-rendered').innerHTML = '';
    $('#md-source').textContent = '';
  }
  $('#view-rendered').classList.toggle('active', state.detailView === 'rendered');
  $('#view-source').classList.toggle('active', state.detailView === 'source');
  $('#view-rendered').setAttribute('aria-selected', String(state.detailView === 'rendered'));
  $('#view-source').setAttribute('aria-selected', String(state.detailView === 'source'));
}

/* ---------- 极简 Markdown 渲染（无外部库，先转义再解析） ---------- */
function mdInline(s) {
  let out = esc(s);
  out = out.replace(/`([^`\n]+)`/g, (m, c) => '<code>' + c + '</code>');
  out = out.replace(/!\[([^\]]*)\]\(([^)\s]+)\)/g, (m, alt, src) => '<img alt="' + alt + '" src="' + src + '">');
  out = out.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (m, txt, href) => '<a href="' + href + '" target="_blank" rel="noopener">' + txt + '</a>');
  out = out.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  out = out.replace(/(^|[^*])\*([^*\n]+)\*/g, '$1<em>$2</em>');
  return out;
}
function renderMarkdown(src) {
  const lines = String(src).replace(/\r\n?/g, '\n').split('\n');
  let html = '', i = 0, para = [];
  const flushPara = () => {
    if (para.length) { html += '<p>' + para.map(mdInline).join('<br>') + '</p>'; para = []; }
  };
  while (i < lines.length) {
    const line = lines[i];
    if (/^```/.test(line)) {
      flushPara();
      const lang = line.slice(3).trim();
      const buf = [];
      i++;
      while (i < lines.length && !/^```/.test(lines[i])) { buf.push(lines[i]); i++; }
      html += '<pre><code' + (lang ? ' data-lang="' + esc(lang) + '"' : '') + '>' + esc(buf.join('\n')) + '</code></pre>';
      i++;
      continue;
    }
    const h = line.match(/^(#{1,4})\s+(.*)$/);
    if (h) { flushPara(); html += '<h' + h[1].length + '>' + mdInline(h[2]) + '</h' + h[1].length + '>'; i++; continue; }
    if (/^\s*---+\s*$/.test(line) || /^\s*\*\*\*+\s*$/.test(line)) { flushPara(); html += '<hr>'; i++; continue; }
    if (/^\s*>\s?/.test(line)) {
      flushPara();
      const buf = [];
      while (i < lines.length && /^\s*>\s?/.test(lines[i])) { buf.push(lines[i].replace(/^\s*>\s?/, '')); i++; }
      html += '<blockquote>' + buf.map(mdInline).join('<br>') + '</blockquote>';
      continue;
    }
    if (/^\s*\|.*\|\s*$/.test(line) && i + 1 < lines.length && /^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1])) {
      flushPara();
      const head = line.split('|').slice(1, -1).map(c => '<th>' + mdInline(c.trim()) + '</th>').join('');
      i += 2;
      const rows = [];
      while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) {
        rows.push('<tr>' + lines[i].split('|').slice(1, -1).map(c => '<td>' + mdInline(c.trim()) + '</td>').join('') + '</tr>');
        i++;
      }
      html += '<table><thead><tr>' + head + '</tr></thead><tbody>' + rows.join('') + '</tbody></table>';
      continue;
    }
    const li = line.match(/^(\s*)([-*+]|\d+[.)])\s+(.*)$/);
    if (li) {
      flushPara();
      const ordered = /^\d/.test(li[2].trim());
      const items = [];
      while (i < lines.length) {
        const m = lines[i].match(/^(\s*)([-*+]|\d+[.)])\s+(.*)$/);
        if (!m) break;
        items.push('<li>' + mdInline(m[3]) + '</li>');
        i++;
      }
      html += (ordered ? '<ol>' : '<ul>') + items.join('') + (ordered ? '</ol>' : '</ul>');
      continue;
    }
    if (!line.trim()) { flushPara(); i++; continue; }
    para.push(line);
    i++;
  }
  flushPara();
  return html;
}

/* ---------- 设置 ---------- */
/* 后端契约：GET /api/settings 返回 {mineru, engines, docling, files,
   queue, asr, vision, logs}；PUT 接受同名分组（另有 engines 分组）。
   字段名与后端一一对应，不再按臆测的契约填默认值。*/
const DOC_ENGINE_OPTIONS = [
  ['auto', '自动'], ['mineru', 'MinerU'], ['anydoc', 'anydoc'], ['docling', 'Docling'],
];
const IMAGE_ENGINE_OPTIONS = [
  ['auto', '自动'], ['mineru', 'MinerU'], ['docling', 'Docling'],
];
const DOC_ENGINE_LABEL = { auto: '自动', mineru: 'MinerU', anydoc: 'anydoc', docling: 'Docling' };
const ASR_ENGINE_OPTIONS = [
  ['fun_asr', 'FunASR'], ['open_ai', 'OpenAI 兼容'], ['vibe_voice', 'VibeVoice'],
];
const ASR_ENGINE_LABEL = { fun_asr: 'FunASR', open_ai: 'OpenAI 兼容', vibe_voice: 'VibeVoice' };
const SETTING_DEFAULTS = {
  mineru: { enabled: false, mineru_enabled: null, provider: 'official', base_url: '', model_version: 'vlm', parse_method: 'auto', version: '2.5', ocr: true, table: true, formula: true, task_timeout_secs: 1800, request_timeout_secs: 60, poll_interval_secs: 2, concurrency: 2, key_cooldown_secs: 60 },
  engines: { pdf: 'auto', office: 'auto', image: 'auto', audio: 'docling', video: 'docling' },
  docling: { anydoc_timeout_secs: 120, docling_timeout_secs: 1800, conversion_input_mib: 512, conversion_output_mib: 16, docling_threads: 2, office_text_only: false, do_ocr: true, do_table_structure: true, max_document_pages: 10000 },
  files: { output_dir: '' },
  queue: { queue_concurrency: 4, mineru_concurrency: 2 },
  asr: { engine: 'fun_asr', base_url: '', model: 'whisper-v3:turbo', timeout_secs: 600, chunk_seconds: 600, poll_interval_secs: 5, max_upload_bytes: 26214400, max_clip_seconds: 1200 },
  vision: { base_url: '', model: 'default-vision-model', timeout_secs: 300, has_key: false, enabled: false },
  logs: { retention_days: 30 },
};
function normSettings(raw) {
  const src = (raw && raw.settings) || raw || {};
  const out = {};
  for (const g of Object.keys(SETTING_DEFAULTS)) {
    const grp = src[g] || {};
    out[g] = Object.assign({}, SETTING_DEFAULTS[g], grp);
  }
  // 后端 mineru.enabled 是生效值（显式开关或有 Key 自动开）；三态原始
  // 选择放在 mineru_enabled（null = 自动）。
  if (src.mineru && src.mineru.mineru_enabled !== undefined) {
    out.mineru.mineru_enabled = src.mineru.mineru_enabled;
  }
  return out;
}
async function loadSettings() {
  const loading = $('#settings-loading'), errEl = $('#settings-error'), body = $('#settings-body');
  loading.hidden = false; errEl.hidden = true; body.hidden = true;
  try {
    const data = await api('/api/settings');
    state.settingsRaw = data;
    state.settings = normSettings(data);
    state.settingsLoadedOutputDir = state.settings.files.output_dir;
    loading.hidden = true; body.hidden = false;
    renderSettings();
    await loadKeys();
  } catch (e) {
    loading.hidden = true;
    errEl.hidden = false;
    errEl.textContent = '加载设置失败：' + apiErrorText(e) + '（将显示默认值，保存会按当前默认值提交）';
    state.settings = normSettings(null);
    state.settingsLoadedOutputDir = state.settings.files.output_dir;
    body.hidden = false;
    renderSettings();
  }
}

/* ----- 设置页小组件 ----- */
function settingRow(label, desc, control) {
  const row = document.createElement('div');
  row.className = 'setting-row';
  const left = document.createElement('div');
  const strong = document.createElement('strong');
  strong.textContent = label;
  const p = document.createElement('p');
  p.textContent = desc;
  left.append(strong, p);
  row.append(left, control);
  return row;
}
function selectControl(value, options, onChange, ariaLabel) {
  const sel = document.createElement('select');
  sel.setAttribute('aria-label', ariaLabel || '');
  for (const [v, label] of options) {
    const o = document.createElement('option');
    o.value = v; o.textContent = label;
    if (String(value) === v) o.selected = true;
    sel.appendChild(o);
  }
  sel.addEventListener('change', () => onChange(sel.value));
  return sel;
}
function numberControl(value, onChange, ariaLabel, min, max) {
  const inp = document.createElement('input');
  inp.type = 'number';
  if (min != null) inp.min = min;
  if (max != null) inp.max = max;
  inp.value = value == null ? '' : String(value);
  inp.setAttribute('aria-label', ariaLabel || '');
  inp.addEventListener('input', () => { onChange(inp.value === '' ? null : Number(inp.value)); });
  return inp;
}
function textControl(value, onChange, ariaLabel, placeholder) {
  const inp = document.createElement('input');
  inp.type = 'text';
  inp.value = value == null ? '' : String(value);
  if (placeholder) inp.placeholder = placeholder;
  inp.setAttribute('aria-label', ariaLabel || '');
  inp.addEventListener('input', () => onChange(inp.value));
  return inp;
}
function checkControl(value, onChange, ariaLabel) {
  const cb = document.createElement('input');
  cb.type = 'checkbox';
  cb.checked = !!value;
  cb.setAttribute('aria-label', ariaLabel || '');
  cb.addEventListener('change', () => onChange(cb.checked));
  return cb;
}
function configButton(label, engine) {
  const wrap = document.createElement('span');
  wrap.className = 'config-actions';
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'outline';
  btn.textContent = label;
  btn.addEventListener('click', () => openEngineModal(engine));
  wrap.appendChild(btn);
  return wrap;
}
function engineSelectCell(kind, options) {
  const wrap = document.createElement('span');
  wrap.className = 'engine-select-cell';
  const sel = selectControl(state.settings.engines[kind], options, v => {
    state.settings.engines[kind] = v;
    renderSettings();
  }, kind + ' 首选引擎');
  wrap.appendChild(sel);
  const current = state.settings.engines[kind];
  const engine = current === 'auto' ? 'mineru' : current;
  wrap.appendChild(configButton('配置 ' + DOC_ENGINE_LABEL[engine], engine));
  return wrap;
}

function renderSettings() {
  if (!state.settings) return;
  const s = state.settings;

  // --- 文档解析：每种类型一个首选引擎 + 超时 + 高级参数（照 SAG 桌面文件设置）---
  const docBox = $('#group-doc-engines');
  docBox.innerHTML = '';
  docBox.appendChild(settingRow('PDF', 'PDF 与扫描版 PDF 的首选解析引擎。', engineSelectCell('pdf', DOC_ENGINE_OPTIONS)));
  docBox.appendChild(settingRow('Office 文档', 'DOCX / PPTX / XLSX / ODT / EPUB 的首选解析引擎。', engineSelectCell('office', DOC_ENGINE_OPTIONS)));
  docBox.appendChild(settingRow('图片', 'JPG / PNG / WebP 等图片的首选解析引擎。', engineSelectCell('image', IMAGE_ENGINE_OPTIONS)));
  docBox.appendChild(settingRow('转换结果存放位置', '解析出的 Markdown 保存目录，留空用默认位置。',
    textControl(s.files.output_dir, v => { s.files.output_dir = v; }, '转换结果存放位置', '留空用默认位置')));
  docBox.appendChild(settingRow('内置转换超时 · 秒', '新转换任务生效。',
    numberControl(s.docling.anydoc_timeout_secs, v => { s.docling.anydoc_timeout_secs = v; }, '内置转换超时', 5, 86400)));
  docBox.appendChild(settingRow('Docling 转换超时 · 秒', '外层自动额外预留 60 秒用于退出清理。',
    numberControl(s.docling.docling_timeout_secs, v => { s.docling.docling_timeout_secs = v; }, 'Docling 转换超时', 5, 86400)));
  const adv = document.createElement('details');
  adv.className = 'advanced-params';
  const advSum = document.createElement('summary');
  advSum.textContent = '高级运行参数';
  adv.appendChild(advSum);
  const advBox = document.createElement('div');
  advBox.className = 'settings-group settings-group-inner';
  advBox.appendChild(settingRow('输入大小上限 · MiB', '单个输入文件超过该大小会被拒绝。',
    numberControl(s.docling.conversion_input_mib, v => { s.docling.conversion_input_mib = v; }, '输入大小上限', 1, 4096)));
  advBox.appendChild(settingRow('输出大小上限 · MiB', '单个输出 Markdown 超过该大小会被拒绝。',
    numberControl(s.docling.conversion_output_mib, v => { s.docling.conversion_output_mib = v; }, '输出大小上限', 1, 4096)));
  advBox.appendChild(settingRow('Docling 线程数', 'Docling 工作线程数。',
    numberControl(s.docling.docling_threads, v => { s.docling.docling_threads = v; }, 'Docling 线程数', 1, 32)));
  advBox.appendChild(settingRow('文档最大页数', '超过该页数的文档会被拒绝。',
    numberControl(s.docling.max_document_pages, v => { s.docling.max_document_pages = v; }, '文档最大页数', 1, 100000)));
  advBox.appendChild(settingRow('Docling OCR', 'Docling 内置 OCR 开关。',
    checkControl(s.docling.do_ocr, v => { s.docling.do_ocr = v; }, 'Docling OCR')));
  advBox.appendChild(settingRow('Docling 表格结构', 'Docling 表格结构识别开关。',
    checkControl(s.docling.do_table_structure, v => { s.docling.do_table_structure = v; }, 'Docling 表格结构')));
  advBox.appendChild(settingRow('Office 仅提取文字', '仅适用于 DOCX/PPTX，跳过图片和内嵌对象。',
    checkControl(s.docling.office_text_only, v => { s.docling.office_text_only = v; }, 'Office 仅提取文字')));
  adv.appendChild(advBox);
  docBox.appendChild(adv);

  // --- 音视频：首选引擎 + 各自的后端配置 ---
  const mediaBox = $('#group-media');
  mediaBox.innerHTML = '';
  const audioCell = document.createElement('span');
  audioCell.className = 'engine-select-cell';
  audioCell.appendChild(selectControl(s.engines.audio, [['docling', '内置 Docling 媒体'], ['asr', 'ASR 直转']], v => {
    s.engines.audio = v;
    renderSettings();
  }, '音频首选引擎'));
  audioCell.appendChild(selectControl(s.asr.engine, ASR_ENGINE_OPTIONS, v => { s.asr.engine = v; }, 'ASR 引擎'));
  audioCell.appendChild(configButton('配置 ASR', 'asr'));
  mediaBox.appendChild(settingRow('音频', '首选 Docling 媒体；失败或选 ASR 直转时走下方 ASR 后端转写。', audioCell));
  const videoCell = document.createElement('span');
  videoCell.className = 'engine-select-cell';
  videoCell.appendChild(selectControl(s.engines.video, [['docling', '内置 Docling 媒体'], ['timeline', '时间线总结（ASR + 视觉）']], v => {
    s.engines.video = v;
    renderSettings();
  }, '视频首选引擎'));
  videoCell.appendChild(configButton('配置视觉模型', 'vision'));
  mediaBox.appendChild(settingRow('视频', '首选 Docling 媒体；选时间线总结时先 ASR 转写音频，再用视觉模型生成带时间戳的总结。', videoCell));

  // --- 队列与日志 ---
  const qBox = $('#group-queue-logs');
  qBox.innerHTML = '';
  qBox.appendChild(settingRow('解析队列并发', '同时跑的解析任务数，1–16。',
    numberControl(s.queue.queue_concurrency, v => { s.queue.queue_concurrency = v; }, '解析队列并发', 1, 16)));
  qBox.appendChild(settingRow('MinerU 并行数', '同时进行的 MinerU 任务数，1–16。',
    numberControl(s.mineru.concurrency, v => { s.mineru.concurrency = v; }, 'MinerU 并行数', 1, 16)));
  qBox.appendChild(settingRow('MinerU Key 冷却 · 秒', '触发 429 / 配额错误后该 Key 暂停轮换的时间，0–3600。',
    numberControl(s.mineru.key_cooldown_secs, v => { s.mineru.key_cooldown_secs = v; }, 'MinerU Key 冷却', 0, 3600)));
  qBox.appendChild(settingRow('日志保留天数', '日志保留天数，过期自动清理，1–365。',
    numberControl(s.logs.retention_days, v => { s.logs.retention_days = v; }, '日志保留天数', 1, 365)));
  renderKeys();
}

async function saveSettings() {
  const statusEl = $('#settings-save-status');
  const feedback = $('#settings-feedback');
  statusEl.textContent = '正在保存…';
  feedback.innerHTML = '';
  const s = state.settings;
  const payload = {
    engines: {
      pdf: s.engines.pdf,
      office: s.engines.office,
      image: s.engines.image,
      audio: s.engines.audio,
      video: s.engines.video,
    },
    mineru: {
      model_version: s.mineru.model_version,
      parse_method: s.mineru.parse_method,
      version: s.mineru.version,
      ocr: s.mineru.ocr,
      table: s.mineru.table,
      formula: s.mineru.formula,
      task_timeout_secs: s.mineru.task_timeout_secs,
      request_timeout_secs: s.mineru.request_timeout_secs,
      poll_interval_secs: s.mineru.poll_interval_secs,
      concurrency: s.mineru.concurrency,
      base_url: s.mineru.base_url,
    },
    runtime: {
      anydoc_timeout_secs: s.docling.anydoc_timeout_secs,
      docling_timeout_secs: s.docling.docling_timeout_secs,
      conversion_input_mib: s.docling.conversion_input_mib,
      conversion_output_mib: s.docling.conversion_output_mib,
      docling_threads: s.docling.docling_threads,
      max_document_pages: s.docling.max_document_pages,
      do_ocr: s.docling.do_ocr,
      do_table_structure: s.docling.do_table_structure,
      office_text_only: s.docling.office_text_only,
    },
    parse: { mineru_key_cooldown_secs: s.mineru.key_cooldown_secs },
    queue: {
      queue_concurrency: s.queue.queue_concurrency,
      mineru_concurrency: s.mineru.concurrency,
    },
    asr: {
      engine: s.asr.engine,
      base_url: s.asr.base_url,
      model: s.asr.model,
      timeout_secs: s.asr.timeout_secs,
      chunk_seconds: s.asr.chunk_seconds,
      poll_interval_secs: s.asr.poll_interval_secs,
      max_upload_bytes: s.asr.max_upload_bytes,
      max_clip_seconds: s.asr.max_clip_seconds,
    },
    vision: {
      base_url: s.vision.base_url,
      model: s.vision.model,
      timeout_secs: s.vision.timeout_secs,
    },
    logs: { retention_days: s.logs.retention_days },
  };
  // MinerU 开关是三态：自动（有 Key 即用）时不发 enabled，让后端保持 auto；
  // 开/关则发显式值。
  if (s.mineru.mineru_enabled === true) payload.mineru.enabled = true;
  if (s.mineru.mineru_enabled === false) payload.mineru.enabled = false;
  // 输出目录与视觉提示词只在改动 / 填写时才发，避免把后端算出的默认
  // 值或空串写回去覆盖原有配置。
  if (s.files.output_dir !== state.settingsLoadedOutputDir) {
    payload.files = { output_dir: s.files.output_dir };
  }
  const visionKey = (state.visionKeyInput || '').trim();
  if (visionKey) payload.vision.key = visionKey;
  const visionPrompt = (state.visionPromptInput || '').trim();
  if (visionPrompt) payload.vision.system_prompt = visionPrompt;
  try {
    await api('/api/settings', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });
    statusEl.textContent = '已保存 ' + new Date().toLocaleTimeString('zh-CN', { hour12: false });
    feedback.innerHTML = '<span class="ok">设置已保存并即时生效。</span>';
    state.visionKeyInput = '';
    state.visionPromptInput = '';
    await loadSettings();
  } catch (e) {
    statusEl.textContent = '';
    feedback.innerHTML = '<span class="err">保存失败：' + esc(apiErrorText(e)) + '</span>';
  }
}

/* ----- 引擎配置弹窗：每个引擎一张单独的配置表 ----- */
const ENGINE_MODAL_TITLES = {
  mineru: '配置 MinerU',
  anydoc: '配置 anydoc（内置解析）',
  docling: '配置 Docling',
  asr: '配置 ASR',
  vision: '配置视觉模型',
};
function openEngineModal(engine) {
  const s = state.settings;
  if (!s) return;
  state.modalEngine = engine;
  $('#engine-modal-title').textContent = ENGINE_MODAL_TITLES[engine] || '配置引擎';
  $('#engine-modal-status').textContent = '';
  const body = $('#engine-modal-body');
  body.innerHTML = '';
  const add = (label, desc, control) => body.appendChild(settingRow(label, desc, control));
  if (engine === 'mineru') {
    const enabledVal = s.mineru.mineru_enabled == null ? 'auto' : (s.mineru.mineru_enabled ? 'on' : 'off');
    add('MinerU 开关', '自动：有 Key 即用；关闭后只用本地引擎。',
      selectControl(enabledVal, [['auto', '自动（有 Key 即用）'], ['on', '开启'], ['off', '关闭']], v => {
        s.mineru.mineru_enabled = v === 'auto' ? null : v === 'on';
        s.mineru.enabled = v !== 'off';
      }, 'MinerU 开关'));
    const providerRow = document.createElement('div');
    providerRow.className = 'setting-row';
    const left = document.createElement('div');
    const strong = document.createElement('strong');
    strong.textContent = '服务商';
    const p = document.createElement('p');
    p.textContent = '解析服务只用 MinerU 官方（mineru.net）；302.AI 已移除。';
    left.append(strong, p);
    const val = document.createElement('span');
    val.className = 'setting-value';
    val.textContent = 'MinerU 官方';
    providerRow.append(left, val);
    body.appendChild(providerRow);
    add('接口地址', '留空使用官方默认地址。',
      textControl(s.mineru.base_url, v => { s.mineru.base_url = v; }, '接口地址', '留空用 https://mineru.net/api/v4'));
    add('解析模型', '官方服务商：VLM 或 Pipeline。',
      selectControl(s.mineru.model_version, [['vlm', 'VLM'], ['pipeline', 'Pipeline']], v => { s.mineru.model_version = v; }, '解析模型'));
    add('解析方式', 'auto 自动识别 / txt 文本提取 / ocr。',
      selectControl(s.mineru.parse_method, [['auto', '自动识别'], ['txt', '文本提取'], ['ocr', 'OCR']], v => { s.mineru.parse_method = v; }, '解析方式'));
    add('MinerU 版本', '解析服务版本标识。',
      selectControl(s.mineru.version, [['2.5', '2.5'], ['2.0', '2.0']], v => { s.mineru.version = v; }, 'MinerU 版本'));
    add('OCR 识别', '对图片型页面做文字识别。',
      checkControl(s.mineru.ocr, v => { s.mineru.ocr = v; }, 'OCR 识别'));
    add('表格识别', '识别并结构化表格。',
      checkControl(s.mineru.table, v => { s.mineru.table = v; }, '表格识别'));
    add('公式识别', '识别数学公式。',
      checkControl(s.mineru.formula, v => { s.mineru.formula = v; }, '公式识别'));
    add('任务时限 · 秒', '单个 MinerU 任务最长等待时间。',
      numberControl(s.mineru.task_timeout_secs, v => { s.mineru.task_timeout_secs = v; }, '任务时限', 10, 86400));
    add('请求超时 · 秒', '单次 HTTP 请求超时。',
      numberControl(s.mineru.request_timeout_secs, v => { s.mineru.request_timeout_secs = v; }, '请求超时', 5, 3600));
    add('轮询间隔 · 秒', '查询 MinerU 任务状态的间隔。',
      numberControl(s.mineru.poll_interval_secs, v => { s.mineru.poll_interval_secs = v; }, '轮询间隔', 1, 60));
    add('并行数', '同时进行的 MinerU 任务数，1–16。',
      numberControl(s.mineru.concurrency, v => { s.mineru.concurrency = v; }, '并行数', 1, 16));
  } else if (engine === 'anydoc') {
    add('Office 仅提取文字', '仅适用于 DOCX/PPTX，跳过图片和内嵌对象。',
      checkControl(s.docling.office_text_only, v => { s.docling.office_text_only = v; }, 'Office 仅提取文字'));
    add('输入大小上限 · MiB', '单个输入文件超过该大小会被拒绝。',
      numberControl(s.docling.conversion_input_mib, v => { s.docling.conversion_input_mib = v; }, '输入大小上限', 1, 4096));
    add('输出大小上限 · MiB', '单个输出 Markdown 超过该大小会被拒绝。',
      numberControl(s.docling.conversion_output_mib, v => { s.docling.conversion_output_mib = v; }, '输出大小上限', 1, 4096));
    add('内置转换超时 · 秒', '新转换任务生效。',
      numberControl(s.docling.anydoc_timeout_secs, v => { s.docling.anydoc_timeout_secs = v; }, '内置转换超时', 5, 86400));
  } else if (engine === 'docling') {
    const note = document.createElement('div');
    note.className = 'settings-info-box';
    note.innerHTML = '<strong>组件状态</strong><p>Docling 组件安装状态后端未单独提供；解析失败时请看日志页的错误原文。未安装的组件对应格式会失败并降级或报错。</p>';
    body.appendChild(note);
    add('Docling 转换超时 · 秒', '外层自动额外预留 60 秒用于退出清理。',
      numberControl(s.docling.docling_timeout_secs, v => { s.docling.docling_timeout_secs = v; }, 'Docling 转换超时', 5, 86400));
    add('Docling 线程数', 'Docling 工作线程数。',
      numberControl(s.docling.docling_threads, v => { s.docling.docling_threads = v; }, 'Docling 线程数', 1, 32));
    add('Docling OCR', 'Docling 内置 OCR 开关。',
      checkControl(s.docling.do_ocr, v => { s.docling.do_ocr = v; }, 'Docling OCR'));
    add('Docling 表格结构', 'Docling 表格结构识别开关。',
      checkControl(s.docling.do_table_structure, v => { s.docling.do_table_structure = v; }, 'Docling 表格结构'));
    add('文档最大页数', '超过该页数的文档会被拒绝。',
      numberControl(s.docling.max_document_pages, v => { s.docling.max_document_pages = v; }, '文档最大页数', 1, 100000));
  } else if (engine === 'asr') {
    add('ASR 引擎', '音频 / 视频转写用的后端。',
      selectControl(s.asr.engine, ASR_ENGINE_OPTIONS, v => { s.asr.engine = v; }, 'ASR 引擎'));
    add('服务地址', 'ASR 后端地址；留空表示未配置。',
      textControl(s.asr.base_url, v => { s.asr.base_url = v; }, '服务地址', '例如 http://127.0.0.1:12346'));
    add('模型', 'OpenAI 兼容引擎用的模型名。',
      textControl(s.asr.model, v => { s.asr.model = v; }, '模型'));
    add('超时 · 秒', '单次 ASR 调用超时。',
      numberControl(s.asr.timeout_secs, v => { s.asr.timeout_secs = v; }, '超时', 1, 86400));
    add('切片目标时长 · 秒', '长录音的目标切片长度，30–3600。',
      numberControl(s.asr.chunk_seconds, v => { s.asr.chunk_seconds = v; }, '切片目标时长', 30, 3600));
    add('轮询间隔 · 秒', '查询 ASR 任务状态的间隔，1–60。',
      numberControl(s.asr.poll_interval_secs, v => { s.asr.poll_interval_secs = v; }, '轮询间隔', 1, 60));
    add('单次上传上限 · MiB', '每次向 ASR 服务上传的最大体积。',
      numberControl(Math.round((s.asr.max_upload_bytes || 0) / 1024 / 1024), v => {
        s.asr.max_upload_bytes = v == null ? null : Math.round(v * 1024 * 1024);
      }, '单次上传上限', 1, 4096));
    add('最长剪辑 · 秒', '单次转写的最长音频。',
      numberControl(s.asr.max_clip_seconds, v => { s.asr.max_clip_seconds = v; }, '最长剪辑', 1, 86400));
  } else if (engine === 'vision') {
    add('服务地址', 'OpenAI 兼容视觉模型地址；留空表示未配置。',
      textControl(s.vision.base_url, v => { s.vision.base_url = v; }, '服务地址', '例如 http://127.0.0.1:12345/v1'));
    add('模型', '视觉模型名。',
      textControl(s.vision.model, v => { s.vision.model = v; }, '模型'));
    add('超时 · 秒', '单次视觉模型调用超时。',
      numberControl(s.vision.timeout_secs, v => { s.vision.timeout_secs = v; }, '超时', 1, 86400));
    const keyInput = document.createElement('input');
    keyInput.type = 'password';
    keyInput.placeholder = s.vision.has_key ? '已保存，留空保留原密钥' : '粘贴视觉模型 Key';
    keyInput.setAttribute('aria-label', '视觉模型 Key');
    keyInput.addEventListener('input', () => { state.visionKeyInput = keyInput.value; });
    add('视觉模型 Key', '只写不回显；留空保留已保存的密钥。', keyInput);
    const promptInput = document.createElement('textarea');
    promptInput.placeholder = '留空保留当前提示词；为空则用默认视频总结提示词';
    promptInput.style.cssText = 'width:min(52%,360px);min-height:64px';
    promptInput.setAttribute('aria-label', '系统提示词');
    promptInput.addEventListener('input', () => { state.visionPromptInput = promptInput.value; });
    add('系统提示词', '发给视觉模型的系统提示词；留空不改动。', promptInput);
  }
  $('#engine-modal').hidden = false;
}
function closeEngineModal() {
  $('#engine-modal').hidden = true;
  state.modalEngine = null;
}

/* ---------- MinerU 多 key 管理（只显示掩码；明文永不回显） ---------- */
function normKey(k) {
  return {
    id: (k.index ?? k.id ?? k.key_id ?? ''),
    masked: k.masked || k.mask || k.display || k.masked_key || '',
    provider: k.provider || '',
    success: k.success ?? k.success_count ?? k.ok ?? 0,
    failed: k.failed ?? k.failed_count ?? k.failure_count ?? k.err ?? 0,
    pages: k.pages ?? k.pages_processed ?? k.page_count ?? 0,
  };
}
async function loadKeys() {
  try {
    const data = await api('/api/settings/mineru-keys');
    const arr = Array.isArray(data) ? data : (data.keys || data.items || []);
    state.keys = arr.map(normKey);
  } catch (e) {
    state.keys = [];
    $('#mineru-keys').innerHTML = '<div class="setting-row"><div><strong>无法加载 Key 列表</strong><p>' + esc(apiErrorText(e)) + '</p></div></div>';
    return;
  }
  renderKeys();
}
function renderKeys() {
  const box = $('#mineru-keys');
  if (!box) return;
  if (!state.settings) return;   // 设置未加载完时先不画
  box.innerHTML = '';
  if (!state.keys.length) {
    box.innerHTML = '<div class="setting-row"><div><strong>暂无 MinerU Key</strong><p>下方添加后即可参与轮换；未配 Key 时 MinerU 任务会直接失败并降级到 anydoc/Docling。</p></div></div>';
    return;
  }
  for (const k of state.keys) {
    const row = document.createElement('div');
    row.className = 'key-row';
    const left = document.createElement('div');
    left.className = 'key-masked';
    left.textContent = k.masked || '(无掩码)';
    const sub = document.createElement('small');
    sub.textContent = 'MinerU 官方 · Key 不会以明文返回';
    left.appendChild(sub);
    const stats = document.createElement('div');
    stats.className = 'key-stats';
    stats.innerHTML = '成功 <b>' + esc(k.success) + '</b> · 失败 <b>' + esc(k.failed) + '</b> · 页数 <b>' + esc(k.pages) + '</b>';
    const del = document.createElement('button');
    del.type = 'button'; del.className = 'outline danger-outline'; del.textContent = '删除';
    del.addEventListener('click', () => deleteKey(k.id));
    row.append(left, stats, del);
    box.appendChild(row);
  }
}
async function addKey() {
  const input = $('#new-key-value');
  const statusEl = $('#key-add-status');
  const value = input.value;
  if (!value.trim()) { statusEl.textContent = '请先粘贴 Key。'; return; }
  statusEl.textContent = '正在添加…';
  try {
    await api('/api/settings/mineru-keys', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ key: value }),
    });
    statusEl.textContent = '已添加。';
    toast('MinerU Key 已添加');
    await loadKeys();
  } catch (e) {
    statusEl.textContent = '添加失败：' + apiErrorText(e);
  } finally {
    input.value = '';   // 提交后立即清空，不留明文
  }
}
async function deleteKey(id) {
  if (id === '' || id == null) return;
  if (!window.confirm('确定删除该 Key 吗？删除后立即生效，不可恢复。')) return;
  try {
    await api('/api/settings/mineru-keys/' + encodeURIComponent(id), { method: 'DELETE' });
    toast('Key 已删除');
    await loadKeys();
  } catch (e) {
    toast('删除失败：' + apiErrorText(e));
  }
}

/* ---------- 日志 ---------- */
function normLog(l) {
  return {
    time: l.time || l.timestamp || l.created_at || '',
    level: String(l.level || l.severity || 'INFO').toUpperCase(),
    taskId: l.task_id || l.taskId || '',
    message: l.message || l.msg || l.text || '',
  };
}
async function loadLogs() {
  const loading = $('#logs-loading'), empty = $('#logs-empty'), errEl = $('#logs-error'), listEl = $('#log-list');
  const firstLoad = listEl.children.length === 0;
  if (firstLoad) { loading.hidden = false; empty.hidden = true; errEl.hidden = true; }
  const params = new URLSearchParams({ limit: '200' });
  const level = $('#log-level').value;
  const taskId = $('#log-task').value.trim();
  if (level) params.set('level', level);
  if (taskId) params.set('task_id', taskId);
  try {
    const data = await api('/api/logs?' + params.toString());
    const arr = Array.isArray(data) ? data : (data.logs || data.items || []);
    let logs = arr.map(normLog);
    const q = $('#log-search').value.trim().toLowerCase();
    if (q) logs = logs.filter(l => (l.message + ' ' + l.taskId).toLowerCase().includes(q));
    loading.hidden = true; errEl.hidden = true;
    listEl.innerHTML = '';
    if (!logs.length) { empty.hidden = false; return; }
    empty.hidden = true;
    for (const l of logs) {
      const li = document.createElement('li');
      li.className = 'log-item';
      const head = document.createElement('div');
      head.className = 'log-head';
      const time = document.createElement('span');
      time.className = 'log-time'; time.textContent = l.time ? fmtTime(l.time) : '';
      const lv = document.createElement('span');
      lv.className = 'log-level'; lv.dataset.level = l.level; lv.textContent = l.level;
      head.append(time, lv);
      if (l.taskId) {
        const tid = document.createElement('span');
        tid.className = 'log-task'; tid.textContent = shortId(l.taskId);
        tid.title = l.taskId;
        head.appendChild(tid);
      }
      const msg = document.createElement('p');
      msg.className = 'log-msg'; msg.dataset.level = l.level; msg.textContent = l.message;
      li.append(head, msg);
      listEl.appendChild(li);
    }
  } catch (e) {
    loading.hidden = true;
    errEl.hidden = false;
    errEl.textContent = '加载日志失败：' + apiErrorText(e);
  }
}

/* ---------- 轮询 ---------- */
setInterval(() => {
  if (state.route === 'tasks') {
    // 仅当列表里有进行中任务时才刷，避免空转
    const active = $$('#task-list .badge').some(b => b.dataset.status === 'pending' || b.dataset.status === 'processing');
    if (active || $('#task-list').children.length === 0) loadTasks();
  } else if (state.route === 'detail' && state.detailTask) {
    if (state.detailTask.status === 'pending' || state.detailTask.status === 'processing') loadDetail(false);
  } else if (state.route === 'logs' && $('#log-autorefresh').checked) {
    loadLogs();
  }
}, POLL_MS);
setInterval(checkHealth, 30000);

/* ---------- 事件绑定 ---------- */
function init() {
  $$('.nav button').forEach(b => b.addEventListener('click', () => navigate('#/' + b.dataset.nav)));

  // 上传
  const dz = $('#dropzone'), fi = $('#file-input');
  $('#pick-files').addEventListener('click', () => fi.click());
  dz.addEventListener('click', ev => { if (ev.target === dz) fi.click(); });
  dz.addEventListener('keydown', ev => { if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); fi.click(); } });
  fi.addEventListener('change', () => { addFiles(Array.from(fi.files)); fi.value = ''; });
  ['dragenter', 'dragover'].forEach(ev => dz.addEventListener(ev, e => { e.preventDefault(); dz.classList.add('dragging'); }));
  ['dragleave', 'drop'].forEach(ev => dz.addEventListener(ev, e => { e.preventDefault(); dz.classList.remove('dragging'); }));
  dz.addEventListener('drop', e => { if (e.dataTransfer && e.dataTransfer.files.length) addFiles(Array.from(e.dataTransfer.files)); });
  $('#upload-btn').addEventListener('click', submitUpload);

  // 任务筛选 / 分页
  $('#filter-status').addEventListener('change', () => { state.tasksPage = 1; loadTasks(); });
  $('#filter-client').addEventListener('input', debounce(() => { state.tasksPage = 1; loadTasks(); }, 400));
  $('#filter-search').addEventListener('input', debounce(() => { state.tasksPage = 1; loadTasks(); }, 400));
  $('#refresh-tasks').addEventListener('click', () => loadTasks());
  $('#page-prev').addEventListener('click', () => { if (state.tasksPage > 1) { state.tasksPage--; loadTasks(); } });
  $('#page-next').addEventListener('click', () => { if (state.tasksPage < state.tasksTotalPages) { state.tasksPage++; loadTasks(); } });

  // 详情
  $('#back-to-tasks').addEventListener('click', () => navigate('#/tasks'));
  $('#detail-restart').addEventListener('click', () => { if (state.detailId) restartTask(state.detailId); });
  $('#detail-retry').addEventListener('click', () => { $('#retry-box').hidden = false; $('#retry-prompt').focus(); });
  $('#retry-cancel').addEventListener('click', () => { $('#retry-box').hidden = true; });
  $('#retry-submit').addEventListener('click', async () => {
    const prompt = $('#retry-prompt').value.trim();
    if (!prompt) { toast('请先填写新 prompt。'); return; }
    const fd = new FormData();
    fd.append('task_id', state.detailId);
    fd.append('prompt', prompt);
    try {
      const data = await api('/convert', { method: 'POST', body: fd });
      const tid = data && data.results && data.results[0] && data.results[0].task_id;
      if (tid) { toast('已用新 prompt 创建新任务'); navigate('#/tasks/' + encodeURIComponent(tid)); }
      else toast('重新处理失败：后端未返回新 task_id。');
    } catch (e) {
      toast('重新处理失败：' + apiErrorText(e));
    }
  });
  $('#view-rendered').addEventListener('click', () => { state.detailView = 'rendered'; renderResult(); });
  $('#view-source').addEventListener('click', () => { state.detailView = 'source'; renderResult(); });
  $('#copy-md').addEventListener('click', async () => {
    if (state.detailMarkdown == null) return;
    try { await navigator.clipboard.writeText(state.detailMarkdown); toast('已复制 Markdown。'); }
    catch (e) { toast('复制失败：浏览器未授权剪贴板。'); }
  });
  $('#download-md').addEventListener('click', () => {
    if (state.detailMarkdown == null) return;
    const name = (state.detailTask && baseName(state.detailTask.input).replace(/\.[^.]+$/, '')) || state.detailId || 'result';
    const blob = new Blob([state.detailMarkdown], { type: 'text/markdown;charset=utf-8' });
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = name + '.md';
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  });

  // 设置
  $('#save-settings').addEventListener('click', saveSettings);
  $('#add-key-btn').addEventListener('click', addKey);
  $('#new-key-value').addEventListener('keydown', ev => { if (ev.key === 'Enter') { ev.preventDefault(); addKey(); } });
  $('#engine-modal-close').addEventListener('click', closeEngineModal);
  $('#engine-modal-done').addEventListener('click', () => { closeEngineModal(); renderSettings(); });
  $('#engine-modal').addEventListener('click', ev => { if (ev.target === $('#engine-modal')) closeEngineModal(); });

  // 日志
  $('#log-level').addEventListener('change', loadLogs);
  $('#log-task').addEventListener('input', debounce(loadLogs, 400));
  $('#log-search').addEventListener('input', debounce(loadLogs, 400));
  $('#refresh-logs').addEventListener('click', loadLogs);

  checkHealth();
  route();
}
document.addEventListener('DOMContentLoaded', init);
