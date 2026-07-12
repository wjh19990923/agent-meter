const $ = (id) => document.getElementById(id);
const state = { loading: true, refreshing: false };

function formatTokens(value) {
  const number = Number(value || 0);
  if (number >= 1e9) return `${(number / 1e9).toFixed(number >= 10e9 ? 1 : 2)}B`;
  if (number >= 1e6) return `${(number / 1e6).toFixed(number >= 10e6 ? 1 : 2)}M`;
  if (number >= 1e3) return `${(number / 1e3).toFixed(number >= 10e3 ? 1 : 2)}K`;
  return number.toLocaleString();
}

function formatCost(value) {
  const number = Number(value || 0);
  if (number >= 100) return `$${number.toFixed(0)}`;
  if (number >= 10) return `$${number.toFixed(1)}`;
  return `$${number.toFixed(2)}`;
}

function renderChart(id, history) {
  const chart = $(id);
  const byDate = new Map((history || []).map((point) => [point.date, point.total]));
  const days = Array.from({ length: 7 }, (_, index) => {
    const date = new Date();
    date.setDate(date.getDate() - (6 - index));
    const key = new Date(date.getTime() - date.getTimezoneOffset() * 60000).toISOString().slice(0, 10);
    return { key, label: `${date.getMonth() + 1}/${date.getDate()}`, value: byDate.get(key) || 0 };
  });
  const max = Math.max(...days.map((day) => day.value), 1);
  chart.replaceChildren(...days.map((day) => {
    const item = document.createElement('div');
    item.className = 'chart-day';
    item.title = `${day.key}: ${formatTokens(day.value)} tokens`;
    const wrap = document.createElement('div');
    wrap.className = 'chart-bar-wrap';
    const bar = document.createElement('i');
    bar.className = 'chart-bar';
    bar.style.height = `${Math.max(2, (day.value / max) * 100)}%`;
    const label = document.createElement('span');
    label.className = 'chart-label';
    label.textContent = day.label;
    wrap.append(bar);
    item.append(wrap, label);
    return item;
  }));
}

function renderAgent(name, usage) {
  $(`${name}Project`).textContent = usage.project;
  $(`${name}Total`).textContent = formatTokens(usage.total);
  $(`${name}Input`).textContent = formatTokens(usage.input);
  $(`${name}Cached`).textContent = formatTokens(usage.cached);
  $(`${name}Output`).textContent = formatTokens(usage.output);
  $(`${name}Cost`).textContent = `${formatCost(usage.costUsd)} est.`;
  renderChart(`${name}Chart`, usage.history);
}

function show(view) {
  ['loadingState', 'content', 'emptyState', 'errorState'].forEach((id) => $(id).classList.add('is-hidden'));
  $(view).classList.remove('is-hidden');
}

async function refresh() {
  if (state.refreshing) return;
  state.refreshing = true;
  $('refreshButton').disabled = true;
  if (state.loading) show('loadingState');
  try {
    const snapshot = await invoke('get_usage');
    $('totalTokens').textContent = `${formatTokens(snapshot.total)} tokens`;
    $('totalCost').textContent = formatCost(snapshot.costUsd);
    $('compactCodex').textContent = formatTokens(snapshot.codex.total);
    $('compactClaude').textContent = formatTokens(snapshot.claude.total);
    $('compactCost').textContent = formatCost(snapshot.costUsd);
    $('dockedTotal').textContent = `${formatTokens(snapshot.total)} tok`;
    $('dockedCost').textContent = formatCost(snapshot.costUsd);
    $('statusText').textContent = snapshot.status === 'live' ? 'Watching local logs' : 'Ready';
    $('liveIndicator').classList.toggle('is-live', snapshot.status === 'live');
    $('updatedAt').textContent = `Updated ${new Date(snapshot.updatedAt).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`;
    renderAgent('codex', snapshot.codex);
    renderAgent('claude', snapshot.claude);
    show(snapshot.status === 'empty' ? 'emptyState' : 'content');
  } catch (error) {
    console.error(error);
    $('totalTokens').textContent = 'Unavailable';
    $('compactCost').textContent = 'Error';
    $('statusText').textContent = 'Read error';
    show('errorState');
  } finally {
    state.loading = false;
    state.refreshing = false;
    $('refreshButton').disabled = false;
  }
}

$('refreshButton').addEventListener('click', refresh);
$('expandButton').addEventListener('click', async () => {
  const expanded = await invoke('toggle_expanded');
  document.querySelector('.widget').classList.toggle('expanded', expanded);
  $('expandButton').querySelector('span').textContent = expanded ? '↙' : '↗';
  $('expandButton').setAttribute('aria-label', expanded ? 'Collapse widget' : 'Expand details');
});
$('retryButton').addEventListener('click', refresh);
$('minimizeButton').addEventListener('click', () => invoke('hide_window'));
$('closeButton').addEventListener('click', () => invoke('hide_window'));
$('pinButton').addEventListener('click', async () => {
  try {
    const pinned = await invoke('toggle_pin');
    $('pinButton').classList.toggle('is-active', pinned);
    $('pinButton').setAttribute('aria-label', pinned ? 'Disable always on top' : 'Enable always on top');
    $('pinButton').title = `Always on top: ${pinned ? 'on' : 'off'}`;
    $('statusText').textContent = pinned ? 'Always on top enabled' : 'Always on top disabled';
  } catch (error) {
    console.error(error);
    $('statusText').textContent = 'Could not change window level';
  }
});
$('startupToggle').addEventListener('change', async (event) => {
  if (event.target.checked) await enable(); else await disable();
  event.target.checked = await isEnabled();
});

invoke('get_settings').then((settings) => {
  $('pinButton').classList.toggle('is-active', settings.pinned);
  $('pinButton').title = `Always on top: ${settings.pinned ? 'on' : 'off'}`;
  document.querySelector('.widget').classList.toggle('expanded', settings.expanded);
  $('expandButton').querySelector('span').textContent = settings.expanded ? '↙' : '↗';
});
isEnabled().then((enabled) => { $('startupToggle').checked = enabled; });
listen('edge-docked', (event) => {
  document.querySelector('.widget').classList.toggle('edge-docked', Boolean(event.payload));
});
setInterval(() => invoke('check_edge_docking'), 180);
setInterval(refresh, 15000);
refresh();
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { disable, enable, isEnabled } from '@tauri-apps/plugin-autostart';
