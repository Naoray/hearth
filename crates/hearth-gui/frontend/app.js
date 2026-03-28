const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const MAX_LINES = 5000;
const MAX_BUFFER_LINES = 1000;

// Panel navigation + data refresh on switch
const panelRefreshMap = { services: refreshServices, sites: refreshSites, php: refreshPhp };
document.querySelectorAll('.nav-item').forEach(btn => {
  btn.addEventListener('click', () => {
    document.querySelectorAll('.nav-item').forEach(b => b.classList.remove('active'));
    document.querySelectorAll('.panel').forEach(p => p.classList.remove('active'));
    btn.classList.add('active');
    document.getElementById(`panel-${btn.dataset.panel}`).classList.add('active');
    const fn_ = panelRefreshMap[btn.dataset.panel];
    if (fn_) fn_();
  });
});

// Services
async function refreshServices() {
  try {
    const services = await invoke('get_status');
    const list = document.getElementById('services-list');
    list.innerHTML = services.map(s => {
      const color = s.state === 'Running' ? 'green'
        : s.state === 'Stopped' ? 'grey'
        : s.state === 'Starting' ? 'yellow' : 'red';
      const pid = s.pid ? ` (PID ${s.pid})` : '';
      return `<div class="service-card">
        <div><span class="status-dot ${color}"></span><span class="service-name">${s.name}</span></div>
        <div><span class="service-state">${s.state}${pid}</span>
          <button onclick="restartService('${s.name}')">Restart</button></div>
      </div>`;
    }).join('');
    document.getElementById('connection-banner').classList.add('hidden');
  } catch (e) {
    document.getElementById('connection-banner').classList.remove('hidden');
  }
}

async function restartService(name) {
  await invoke('restart_service', { service: name });
  await refreshServices();
}

document.getElementById('start-all-btn').addEventListener('click', async () => {
  await invoke('start_services');
  await refreshServices();
});
document.getElementById('stop-all-btn').addEventListener('click', async () => {
  await invoke('stop_services');
  await refreshServices();
});
document.getElementById('start-daemon-btn').addEventListener('click', async () => {
  await invoke('ensure_daemon');
  setTimeout(refreshServices, 1500);
});

// Sites
async function refreshSites() {
  try {
    const sites = await invoke('get_sites');
    const list = document.getElementById('sites-list');
    list.innerHTML = sites.map(s => {
      const sslBadge = s.secured ? '<span class="site-badge ssl">SSL</span>' : '';
      const phpBadge = s.php_version ? `<span class="site-badge">PHP ${s.php_version}</span>` : '';
      return `<div class="site-row">
        <div><span class="site-name" onclick="window.__TAURI__.shell.open('https://${s.name}.test')">${s.name}</span>
          <div class="site-path">${s.path}</div></div>
        <div style="display:flex;gap:6px;align-items:center">
          ${sslBadge}${phpBadge}
          <button onclick="toggleSsl('${s.name}', ${s.secured})">${s.secured ? 'Unsecure' : 'Secure'}</button>
          <button onclick="unlinkSite('${s.name}')">Unlink</button></div>
      </div>`;
    }).join('');
  } catch (e) { console.error('Failed to load sites:', e); }
}

async function toggleSsl(name, isSecured) {
  await invoke(isSecured ? 'unsecure_site' : 'secure_site', { name });
  await refreshSites();
}
async function unlinkSite(name) {
  await invoke('unlink_site', { name });
  await refreshSites();
}

document.getElementById('link-site-btn').addEventListener('click', async () => {
  const { open } = window.__TAURI__.dialog;
  const selected = await open({ directory: true, title: 'Select site directory' });
  if (selected) {
    await invoke('link_site', { path: selected, name: null });
    await refreshSites();
  }
});

// PHP
async function refreshPhp() {
  try {
    const versions = await invoke('get_php_versions');
    const list = document.getElementById('php-list');
    list.innerHTML = versions.map(v => {
      const cls = v.active ? 'php-active' : '';
      return `<div class="php-version-row">
        <span class="${cls}">PHP ${v.version}${v.active ? ' (active)' : ''}</span>
        <div><span class="site-path">${v.path}</span>
          ${v.active ? '' : `<button onclick="switchPhp('${v.version}')">Switch</button>`}</div>
      </div>`;
    }).join('');
  } catch (e) { console.error('Failed to load PHP versions:', e); }
}

async function switchPhp(version) {
  await invoke('switch_php', { version });
  await refreshPhp();
  await refreshServices();
}

// Dump streaming
let dumpPaused = false;
let dumpBuffer = [];

listen('dump-line', (event) => {
  if (dumpPaused) {
    dumpBuffer.push(event.payload);
    if (dumpBuffer.length > MAX_BUFFER_LINES) {
      dumpBuffer = dumpBuffer.slice(dumpBuffer.length - MAX_BUFFER_LINES);
    }
    return;
  }
  appendDumpLine(event.payload);
});
listen('dump-connected', () => {
  document.getElementById('dump-status').className = 'status-dot green';
});
listen('dump-disconnected', () => {
  document.getElementById('dump-status').className = 'status-dot grey';
});

function appendDumpLine(line) {
  const output = document.getElementById('dump-output');
  output.textContent += line + '\n';
  const lines = output.textContent.split('\n');
  if (lines.length > MAX_LINES) {
    output.textContent = lines.slice(lines.length - MAX_LINES).join('\n');
  }
  output.scrollTop = output.scrollHeight;
}

document.getElementById('dump-pause-btn').addEventListener('click', () => {
  dumpPaused = !dumpPaused;
  document.getElementById('dump-pause-btn').textContent = dumpPaused ? 'Resume' : 'Pause';
  if (!dumpPaused) { dumpBuffer.forEach(appendDumpLine); dumpBuffer = []; }
});
document.getElementById('dump-clear-btn').addEventListener('click', () => {
  document.getElementById('dump-output').textContent = '';
});


// Initial load
refreshServices();
