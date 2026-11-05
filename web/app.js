'use strict';

(() => {
  const $ = (id) => document.getElementById(id);
  const NS = 'http://www.w3.org/2000/svg';
  const APP_NAMES = { codex: 'Codex', claude: 'Claude', cursor: 'Cursor', grok: 'Grok Bot', grokbot: 'Grok Bot', 'grok-bot': 'Grok Bot' };
  const COLORS = { input: '#5dd9bc', output: '#f09a7d', cache_read: '#6d91b7', cache_write: '#b5a2de' };
  const TZ = 'Asia/Hong_Kong';
  let sessionToken = '';
  try {
    const fragment = new URLSearchParams(location.hash.slice(1));
    sessionToken = fragment.get('token') || sessionStorage.getItem('token-ledger.session') || '';
    if (fragment.has('token')) {
      history.replaceState(null, '', location.pathname + location.search);
      if (sessionToken) sessionStorage.setItem('token-ledger.session', sessionToken);
    }
  } catch (_) {
    sessionToken = new URLSearchParams(location.hash.slice(1)).get('token') || '';
    history.replaceState(null, '', location.pathname + location.search);
  }

  const dateInZone = (date = new Date()) => {
    const pieces = new Intl.DateTimeFormat('en-CA', { timeZone: TZ, year: 'numeric', month: '2-digit', day: '2-digit' }).formatToParts(date);
    const values = Object.fromEntries(pieces.map((p) => [p.type, p.value]));
    return `${values.year}-${values.month}-${values.day}`;
  };
  const addDays = (date, days) => new Date(Date.parse(`${date}T12:00:00Z`) + days * 86400000).toISOString().slice(0, 10);
  const state = { from: dateInZone(), to: dateInZone(), range: 'today', grain: 'hour', summary: null, coverage: null, loading: false, refreshId: 0 };
  const number = (value) => Number.isFinite(Number(value)) ? Math.max(0, Number(value)) : 0;
  const full = (value) => new Intl.NumberFormat('en-US').format(number(value));
  function compact(value, digits = 2) {
    const n = number(value);
    if (n < 1000) return full(n);
    const unit = n >= 1e12 ? [1e12, 'T'] : n >= 1e9 ? [1e9, 'B'] : n >= 1e6 ? [1e6, 'M'] : [1e3, 'K'];
    return new Intl.NumberFormat('en-US', { maximumFractionDigits: digits }).format(n / unit[0]) + unit[1];
  }
  const appName = (app) => APP_NAMES[String(app).toLowerCase()] || String(app || 'Unknown source');
  const displayName = (value) => value === 'unknown' || value === '' || value == null ? 'Unknown' : String(value);
  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  }
  function svg(tag, attrs = {}) {
    const node = document.createElementNS(NS, tag);
    Object.entries(attrs).forEach(([key, value]) => node.setAttribute(key, String(value)));
    return node;
  }
  function dateLabel(value, options = {}) {
    const d = new Date(`${value}T12:00:00+08:00`);
    return Number.isNaN(d.getTime()) ? value : new Intl.DateTimeFormat('en-US', { month: 'short', day: 'numeric', timeZone: TZ, ...options }).format(d);
  }
  function timeLabel(ms) {
    if (!ms) return 'Not yet scanned';
    const d = new Date(Number(ms));
    return Number.isNaN(d.getTime()) ? 'Time unavailable' : new Intl.DateTimeFormat('en-US', { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit', timeZone: TZ }).format(d) + ' HKT';
  }
  function notice(text, error = false) {
    $('notice').textContent = text;
    $('notice').classList.toggle('error', error);
    $('notice').hidden = !text;
  }
  let toastTimeout;
  function toast(text) {
    $('toast').textContent = text;
    $('toast').hidden = false;
    clearTimeout(toastTimeout);
    toastTimeout = setTimeout(() => { $('toast').hidden = true; }, 6000);
  }
  async function api(path, options = {}) {
    if (!sessionToken) throw new Error('This page needs its local session link. Open the URL printed by TokenLedger, or restart it with serve --open.');
    const headers = { Authorization: `Bearer ${sessionToken}`, ...options.headers };
    const response = await fetch(path, { ...options, headers, credentials: 'omit', cache: 'no-store' });
    const type = response.headers.get('content-type') || '';
    let body;
    if (type.includes('application/json')) body = await response.json();
    else body = { error: `Local server returned HTTP ${response.status}.` };
    if (!response.ok) {
      if (response.status === 401 || response.status === 403) throw new Error('The local session has expired or this link is not authorized. Reopen the URL printed by the current TokenLedger process.');
      throw new Error(typeof body.error === 'string' ? body.error : `Request failed (HTTP ${response.status}).`);
    }
    return body;
  }
  function query() {
    const q = new URLSearchParams({ from: state.from, to: state.to, tz: TZ });
    for (const key of ['app', 'model', 'project', 'account']) {
      if ($(`filter-${key}`).value) q.set(key, $(`filter-${key}`).value);
    }
    return q;
  }
  function updateDateLabel() {
    $('date-label').textContent = state.from === state.to ? dateLabel(state.to, { year: 'numeric' }) : `${dateLabel(state.from)} – ${dateLabel(state.to, { year: 'numeric' })}`;
    $('date-from').value = state.from;
    $('date-to').value = state.to;
    $('date-from').max = state.to;
    $('date-to').min = state.from;
    $('chart-range').textContent = `Asia/Hong_Kong · ${state.grain === 'hour' ? 'hourly' : 'daily'}`;
  }
  function setFilterOptions(id, values, initial, transform = displayName) {
    const select = $(id);
    const previous = select.value;
    select.replaceChildren(new Option(initial, ''));
    const unique = new Set((values || []).map((v) => typeof v === 'string' ? v : (v?.value ?? v?.name ?? '')).filter(Boolean));
    if (previous) unique.add(previous);
    Array.from(unique).sort().forEach((value) => select.add(new Option(transform(value), value)));
    select.value = previous;
  }
  function renderSummary(data) {
    state.summary = data;
    $('total-value').textContent = compact(data.total_tokens);
    $('total-value').title = `${full(data.total_tokens)} recorded tokens`;
    $('record-count').textContent = `${full(data.records)} dated ${number(data.records) === 1 ? 'record' : 'records'}`;
    const tokens = data.tokens || {};
    const keys = ['input', 'output', 'cache_read', 'cache_write'];
    const ids = ['input-value', 'output-value', 'cache-read-value', 'cache-write-value'];
    const sum = keys.reduce((a, key) => a + number(tokens[key]), 0);
    $('breakdown-bar').replaceChildren();
    keys.forEach((key, i) => {
      $(ids[i]).textContent = compact(tokens[key]);
      $(ids[i]).title = full(tokens[key]) + ' tokens';
      if (number(tokens[key]) > 0 && sum > 0) {
        const segment = el('span');
        segment.style.flex = String(number(tokens[key]));
        segment.style.background = COLORS[key];
        segment.title = `${key.replaceAll('_', ' ')}: ${full(tokens[key])} tokens (${(number(tokens[key]) / sum * 100).toFixed(1)}%)`;
        $('breakdown-bar').append(segment);
      }
    });
    $('breakdown-bar').setAttribute('aria-label', keys.map((k) => `${k.replaceAll('_', ' ')} ${full(tokens[k])}`).join(', '));
    $('reasoning-note').textContent = number(tokens.reasoning) > 0 ? `Includes ${compact(tokens.reasoning)} reported reasoning tokens within output.` : 'Reasoning, when reported, is included in output.';
    renderRanks('app-ranks', data.apps, 'app', appName);
    renderRanks('model-ranks', data.models, 'model', displayName);
    renderRanks('project-ranks', data.projects, 'project', displayName);
    const filters = data.filters || {};
    setFilterOptions('filter-app', filters.apps, 'All sources', appName);
    setFilterOptions('filter-model', filters.models, 'All models');
    setFilterOptions('filter-project', filters.projects, 'All projects');
    setFilterOptions('filter-account', filters.accounts, 'All accounts');
    $('reset-filters').hidden = !['app', 'model', 'project', 'account'].some((key) => $(`filter-${key}`).value);
    const undated = number(data.undated_tokens);
    $('undated-note').hidden = !undated;
    $('undated-note').textContent = `${full(undated)} additional recorded tokens have no reliable date. These cumulative lower bounds are kept outside daily totals and charts.`;
    if (data.warnings?.length) notice(data.warnings.map(String).join(' '));
  }
  function renderRanks(id, values, key, transform) {
    const root = $(id);
    root.replaceChildren();
    const items = (values || []).filter((item) => number(item.total_tokens) > 0).sort((a, b) => number(b.total_tokens) - number(a.total_tokens)).slice(0, 4);
    if (!items.length) {
      const empty = el('div', 'rank-empty');
      empty.append(el('span', '', '—'), el('div', '', 'No recorded usage in this view'));
      root.append(empty);
      return;
    }
    const max = number(items[0].total_tokens);
    items.forEach((item, i) => {
      const rank = el('div', 'rank-item');
      const top = el('div', 'rank-top');
      const name = el('div', 'rank-name');
      const text = el('span', 'rank-name-text', transform(item[key]));
      text.title = transform(item[key]);
      name.append(el('span', 'rank-index', String(i + 1).padStart(2, '0')), text);
      const value = el('span', 'rank-value', compact(item.total_tokens));
      value.title = `${full(item.total_tokens)} tokens`;
      top.append(name, value);
      const track = el('div', 'rank-track');
      const fill = el('div', 'rank-fill');
      fill.style.width = `${max ? Math.max(.5, number(item.total_tokens) / max * 100) : 0}%`;
      track.append(fill);
      rank.append(top, track);
      root.append(rank);
    });
  }
  function bucketLabel(bucket, isHour) {
    const text = String(bucket || '');
    if (isHour) {
      if (/^\d{1,2}:\d\d$/.test(text)) return text;
      const match = text.match(/(?:T| )(\d\d)(?::\d\d)?/);
      if (match && !text.endsWith('Z') && !/[+-]\d\d:\d\d$/.test(text)) return match[1] + ':00';
      const parsed = new Date(text);
      if (!Number.isNaN(parsed.getTime())) return new Intl.DateTimeFormat('en-GB', { hour: '2-digit', minute: '2-digit', hourCycle: 'h23', timeZone: TZ }).format(parsed);
      return text;
    }
    return /^\d{4}-\d{2}-\d{2}/.test(text) ? dateLabel(text.slice(0, 10)) : text;
  }
  function renderChart(data) {
    const root = $('usage-chart');
    root.replaceChildren();
    const points = data.points || [];
    const width = Math.max(300, $('chart-wrap').clientWidth || 1000);
    const height = width < 600 ? 190 : width > 1250 ? 285 : 255;
    const left = width < 600 ? 43 : 58, right = width < 600 ? 13 : 20, top = 20, bottom = 35;
    root.setAttribute('viewBox', `0 0 ${width} ${height}`);
    const plotW = width - left - right, plotH = height - top - bottom;
    const maxRaw = Math.max(0, ...points.map((p) => number(p.total_tokens)));
    const magnitude = Math.pow(10, Math.floor(Math.log10(maxRaw || 1)));
    const max = maxRaw ? Math.ceil(maxRaw / magnitude / .5) * magnitude * .5 : 100;
    for (let i = 0; i <= 4; i++) {
      const y = top + plotH * i / 4;
      root.append(svg('line', { x1: left, y1: y, x2: width - right, y2: y, stroke: '#28394a', 'stroke-width': .7, 'stroke-dasharray': i === 4 ? '0' : '3 5' }));
      const label = svg('text', { x: left - 13, y: y + 3, fill: '#637b8e', 'font-size': 9, 'text-anchor': 'end', 'font-family': 'inherit' });
      label.textContent = compact(max * (4 - i) / 4, 1);
      root.append(label);
    }
    $('chart-empty').hidden = maxRaw > 0;
    const step = plotW / Math.max(1, points.length);
    const barWidth = Math.max(.5, Math.min(52, step * .53));
    const maxLabels = Math.max(4, Math.min(12, Math.floor(($('chart-wrap').clientWidth || 800) / 70)));
    const labelEvery = Math.max(1, Math.ceil(points.length / maxLabels));
    points.forEach((point, i) => {
      const x = left + step * (i + .5);
      let usedHeight = 0;
      const group = svg('g');
      const title = svg('title');
      title.textContent = `${point.bucket}: ${full(point.total_tokens)} tokens\nOrdinary input: ${full(point.input)}\nOutput: ${full(point.output)}\nCache read: ${full(point.cache_read)}\nCache write: ${full(point.cache_write)}`;
      group.append(title);
      for (const key of ['cache_read', 'cache_write', 'input', 'output']) {
        const h = number(point[key]) / max * plotH;
        if (!h) continue;
        group.append(svg('rect', { x: x - barWidth / 2, y: top + plotH - usedHeight - h, width: barWidth, height: Math.max(.3, h), rx: 2, fill: COLORS[key], opacity: .85 }));
        usedHeight += h;
      }
      root.append(group);
      if (i % labelEvery === 0 || i === points.length - 1 && points.length > 1 && i % labelEvery > 1) {
        const label = svg('text', { x, y: height - 13, fill: '#6f879b', 'font-size': 9, 'text-anchor': 'middle', 'font-family': 'inherit' });
        label.textContent = bucketLabel(point.bucket, state.grain === 'hour');
        root.append(label);
      }
    });
    root.setAttribute('aria-label', `${state.grain === 'hour' ? 'Hourly' : 'Daily'} recorded usage from ${state.from} through ${state.to}. ${full(points.reduce((sum, p) => sum + number(p.total_tokens), 0))} tokens. ${maxRaw ? 'Bar heights include ordinary input, output, cache reads, and cache writes.' : 'No dated usage in this view.'}`);
  }
  function renderCoverage(data) {
    state.coverage = data;
    const root = $('coverage-sources');
    root.replaceChildren();
    const sources = data.sources || [];
    if (!sources.length) {
      const empty = el('div', 'rank-empty', data.scanning ? 'Scanning approved local sources…' : 'No source scan results yet. Refresh to begin.');
      root.append(empty);
    }
    sources.forEach((source) => {
      const app = String(source.app || '').toLowerCase();
      const card = el('article', 'source-card');
      const symbolApp = app === 'grok-bot' ? 'grok' : app;
      const symbol = el('span', `source-symbol ${['codex', 'claude', 'cursor', 'grok'].includes(symbolApp) ? symbolApp : ''}`, { codex: '⌘', claude: '✳', cursor: '↗', grok: 'G' }[symbolApp] || '◇');
      symbol.setAttribute('aria-hidden', 'true');
      const info = el('div', 'source-info');
      const title = el('div', 'source-title');
      const status = String(source.status || 'unknown');
      const statusClass = ['ok', 'observed', 'available', 'complete', 'imported'].includes(status) ? ' good' : ['error', 'stale', 'unavailable', 'failed'].includes(status) ? ' problem' : '';
      const sourceNames = { 'local:codex': 'Codex · session logs', 'local:claude-code': 'Claude Code · logs', 'local:claude-transcripts': 'Claude Desktop · transcripts', 'claude-desktop': 'Claude Desktop · cache', 'cursor-imports': 'Cursor · export required', 'local:grok-quota': 'Grok Bot · quota only', 'local:identity-conflicts': 'Conflicting record identities' };
      title.append(el('strong', '', sourceNames[source.source_id] || appName(app)), el('span', `source-status${statusClass}`, status.replaceAll('_', ' ')));
      info.append(title, el('p', '', source.message || 'No coverage details available.'));
      let meta = `${full(source.records)} records`;
      if (number(source.undated)) meta += ` · ${full(source.undated)} undated`;
      if (number(source.errors)) meta += ` · ${full(source.errors)} ${number(source.errors) === 1 ? 'issue' : 'issues'}`;
      info.append(el('span', 'source-meta', meta));
      card.append(symbol, info);
      root.append(card);
    });
    const quotaRoot = $('quota-list');
    quotaRoot.replaceChildren();
    quotaRoot.hidden = !(data.quotas || []).length;
    (data.quotas || []).forEach((quota) => {
      const card = el('div', 'quota-card');
      const copy = el('div', 'quota-copy');
      copy.append(el('strong', '', `${appName(quota.app)} · cached quota snapshot`));
      const reset = quota.next_reset_ms ? ` Resets ${timeLabel(quota.next_reset_ms)}.` : '';
      copy.append(el('p', '', `${quota.note || 'Quota is not token usage and is excluded from totals.'}${reset}`));
      const value = el('div', 'quota-number', `${new Intl.NumberFormat('en-US', { maximumFractionDigits: 1 }).format(number(quota.percent_used))}%`);
      value.append(el('span', '', 'used'));
      card.append(copy, value);
      quotaRoot.append(card);
    });
    $('last-scan').textContent = data.scanning ? 'Scanning local sources…' : data.last_scan ? `Last scan ${timeLabel(data.last_scan)}` : 'Waiting for first scan';
    $('refresh').classList.toggle('spinning', !!data.scanning);
    $('refresh').disabled = !!data.scanning;
  }
  async function refreshView(quiet = false) {
    const refreshId = ++state.refreshId;
    if (!quiet) state.loading = true;
    const q = query();
    const timeseriesQ = new URLSearchParams(q);
    timeseriesQ.set('grain', state.grain);
    try {
      const [summary, chart, coverage] = await Promise.all([api(`/api/summary?${q}`), api(`/api/timeseries?${timeseriesQ}`), api('/api/coverage')]);
      if (refreshId !== state.refreshId) return;
      notice('');
      renderSummary(summary);
      renderChart(chart);
      renderCoverage(coverage);
      $('connection-dot').className = 'connection-dot connected';
      $('connection-dot').title = 'Connected to the local TokenLedger process';
    } catch (error) {
      if (refreshId !== state.refreshId) return;
      notice(error.message || 'Could not reach the local TokenLedger process.', true);
      $('connection-dot').className = 'connection-dot error';
      $('connection-dot').title = 'Local server unavailable';
    } finally {
      if (refreshId === state.refreshId) state.loading = false;
    }
  }
  function chooseRange(range) {
    if (range === 'custom') {
      $('custom-dates').hidden = !$('custom-dates').hidden;
      if (!$('custom-dates').hidden) $('date-from').focus();
      return;
    }
    state.range = range;
    state.to = dateInZone();
    state.from = addDays(state.to, range === '7d' ? -6 : range === '30d' ? -29 : 0);
    state.grain = range === 'today' ? 'hour' : 'day';
    $('custom-dates').hidden = true;
    syncRangeButtons();
    updateDateLabel();
    refreshView();
  }
  function syncRangeButtons() {
    document.querySelectorAll('[data-range]').forEach((button) => {
      const selected = button.dataset.range === state.range;
      button.classList.toggle('selected', selected);
      button.setAttribute('aria-pressed', selected ? 'true' : 'false');
    });
    document.querySelectorAll('[data-grain]').forEach((button) => {
      const selected = button.dataset.grain === state.grain;
      button.classList.toggle('selected', selected);
      button.setAttribute('aria-pressed', selected ? 'true' : 'false');
    });
  }
  document.querySelectorAll('[data-range]').forEach((button) => button.addEventListener('click', () => chooseRange(button.dataset.range)));
  document.querySelectorAll('[data-grain]').forEach((button) => button.addEventListener('click', () => { state.grain = button.dataset.grain; syncRangeButtons(); updateDateLabel(); refreshView(); }));
  $('apply-dates').addEventListener('click', () => {
    const from = $('date-from').value, to = $('date-to').value;
    if (!from || !to || from > to) { notice('Choose a valid start date and an end date on or after it.', true); return; }
    if ((Date.parse(to) - Date.parse(from)) / 86400000 > 3660) { notice('Choose a date range no longer than ten years.', true); return; }
    state.from = from;
    state.to = to;
    state.range = 'custom';
    state.grain = from === to ? 'hour' : 'day';
    syncRangeButtons();
    updateDateLabel();
    refreshView();
  });
  $('date-from').addEventListener('change', () => { $('date-to').min = $('date-from').value; });
  $('date-to').addEventListener('change', () => { $('date-from').max = $('date-to').value; });
  for (const key of ['app', 'model', 'project', 'account']) $(`filter-${key}`).addEventListener('change', () => refreshView());
  $('reset-filters').addEventListener('click', () => { for (const key of ['app', 'model', 'project', 'account']) $(`filter-${key}`).value = ''; refreshView(); });
  $('refresh').addEventListener('click', async () => {
    $('refresh').disabled = true;
    $('refresh').classList.add('spinning');
    try { await api('/api/scan', { method: 'POST' }); toast('Local scan requested. AI-tool sources remain read-only.'); await refreshView(); }
    catch (error) { notice(error.message, true); }
    finally { if (!state.coverage?.scanning) { $('refresh').disabled = false; $('refresh').classList.remove('spinning'); } }
  });

  const hints = {
    cursor: 'Cursor: choose a usage-events CSV or JSON export containing recorded token counters.',
    'grok-bot': 'Grok Bot: the export must explicitly identify Grok Bot activity and contain numeric token counters. Quota percentages and chat text cannot be imported as tokens.',
    codex: 'Codex: choose a JSONL session log from another machine. Active/archive copies and repeated usage events are reconciled.',
    claude: 'Claude: choose a Claude Code or retained Desktop JSONL session log. Text without recorded usage is never estimated.'
  };
  function openImport() {
    $('import-error').hidden = true;
    $('import-from').value = state.from;
    $('import-to').value = state.to;
    $('import-dialog').showModal();
  }
  for (const id of ['open-import', 'nav-import']) $(id).addEventListener('click', openImport);
  for (const id of ['close-import', 'cancel-import']) $(id).addEventListener('click', () => $('import-dialog').close());
  $('import-dialog').addEventListener('click', (event) => { if (event.target === $('import-dialog')) { const box = $('import-dialog').getBoundingClientRect(); if (event.clientX < box.left || event.clientX > box.right || event.clientY < box.top || event.clientY > box.bottom) $('import-dialog').close(); } });
  $('import-app').addEventListener('change', () => { $('import-hint').textContent = hints[$('import-app').value]; });
  $('import-file').addEventListener('change', () => {
    const file = $('import-file').files[0];
    $('file-title').textContent = file ? file.name : 'Choose a usage file';
    $('file-description').textContent = file ? `${new Intl.NumberFormat('en-US', { maximumFractionDigits: 2 }).format(file.size / 1024 / 1024)} MiB · Processed locally` : 'CSV, JSON, or JSONL · Up to 32 MiB';
  });
  $('import-form').addEventListener('submit', async (event) => {
    event.preventDefault();
    const errorNode = $('import-error');
    errorNode.hidden = true;
    const file = $('import-file').files[0];
    const account = $('import-account').value.trim();
    const from = $('import-from').value, to = $('import-to').value;
    const submit = $('submit-import');
    try {
      if (!file) throw new Error('Choose a usage export or session log first.');
      if (file.size > 32 * 1024 * 1024) throw new Error('This file is larger than 32 MiB. Export a smaller date window or import a smaller session log.');
      if (!account) throw new Error('Add an account label so unrelated accounts stay separate.');
      if (!from || !to || from > to) throw new Error('Choose the valid date window covered by this file.');
      const extension = file.name.toLowerCase().split('.').pop();
      const format = extension === 'csv' ? 'csv' : ['jsonl', 'ndjson'].includes(extension) ? 'jsonl' : extension === 'json' ? 'json' : '';
      if (!format) throw new Error('Use a CSV, JSON, or JSONL file.');
      submit.disabled = true;
      submit.textContent = 'Reading local records…';
      const content = await file.text();
      const result = await api('/api/import', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ format, app: $('import-app').value, account, from, to, name: file.name, content }) });
      $('import-dialog').close();
      $('import-file').value = '';
      $('file-title').textContent = 'Choose a usage file';
      $('file-description').textContent = 'CSV, JSON, or JSONL · Up to 32 MiB';
      toast(`Imported ${full(result.records)} ${number(result.records) === 1 ? 'record' : 'records'}.${result.warnings?.length ? ' See coverage for details.' : ''}`);
      await refreshView();
      if (result.warnings?.length) notice(result.warnings.map(String).join(' '));
    } catch (error) {
      errorNode.textContent = error.message || 'This import could not be processed.';
      errorNode.hidden = false;
    } finally {
      submit.disabled = false;
      submit.replaceChildren(document.createTextNode('Import records'), el('span', '', '↗'));
      submit.lastChild.setAttribute('aria-hidden', 'true');
    }
  });

  updateDateLabel();
  syncRangeButtons();
  renderChart({ points: [] });
  for (const id of ['app-ranks', 'model-ranks', 'project-ranks']) renderRanks(id, [], '', displayName);
  refreshView();
  setInterval(() => { if (!document.hidden && !$('import-dialog').open && !state.loading) refreshView(true); }, 30000);
  document.addEventListener('visibilitychange', () => { if (!document.hidden && !state.loading) refreshView(true); });
  let resizeTimeout;
  window.addEventListener('resize', () => {
    clearTimeout(resizeTimeout);
    resizeTimeout = setTimeout(() => { if (!state.loading) refreshView(true); }, 200);
  });
})();
