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
    list.textContent = '';

    for (const s of services) {
      const color = s.state === 'Running' ? 'green'
        : s.state === 'Stopped' ? 'grey'
        : s.state === 'Starting' ? 'yellow' : 'red';
      const pid = s.pid ? ` (PID ${s.pid})` : '';

      const card = document.createElement('div');
      card.className = 'service-card';

      const left = document.createElement('div');
      const dot = document.createElement('span');
      dot.className = `status-dot ${color}`;
      const nameSpan = document.createElement('span');
      nameSpan.className = 'service-name';
      nameSpan.textContent = s.name;
      left.appendChild(dot);
      left.appendChild(nameSpan);

      const right = document.createElement('div');
      const stateSpan = document.createElement('span');
      stateSpan.className = 'service-state';
      stateSpan.textContent = s.state + pid;
      const btn = document.createElement('button');
      btn.textContent = 'Restart';
      btn.addEventListener('click', async () => {
        await invoke('restart_service', { service: s.name });
        await refreshServices();
      });
      right.appendChild(stateSpan);
      right.appendChild(btn);

      card.appendChild(left);
      card.appendChild(right);
      list.appendChild(card);
    }
    document.getElementById('connection-banner').classList.add('hidden');
  } catch (e) {
    document.getElementById('connection-banner').classList.remove('hidden');
  }
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
    list.textContent = '';

    for (const s of sites) {
      const row = document.createElement('div');
      row.className = 'site-row';

      const left = document.createElement('div');
      const nameSpan = document.createElement('span');
      nameSpan.className = 'site-name';
      nameSpan.textContent = s.name;
      nameSpan.addEventListener('click', () => {
        window.__TAURI__.shell.open('https://' + s.name + '.test');
      });
      const pathDiv = document.createElement('div');
      pathDiv.className = 'site-path';
      pathDiv.textContent = s.path;
      left.appendChild(nameSpan);
      left.appendChild(pathDiv);

      const right = document.createElement('div');
      right.style.display = 'flex';
      right.style.gap = '6px';
      right.style.alignItems = 'center';

      if (s.secured) {
        const sslBadge = document.createElement('span');
        sslBadge.className = 'site-badge ssl';
        sslBadge.textContent = 'SSL';
        right.appendChild(sslBadge);
      }
      if (s.php_version) {
        const phpBadge = document.createElement('span');
        phpBadge.className = 'site-badge';
        phpBadge.textContent = 'PHP ' + s.php_version;
        right.appendChild(phpBadge);
      }

      const sslBtn = document.createElement('button');
      sslBtn.textContent = s.secured ? 'Unsecure' : 'Secure';
      sslBtn.addEventListener('click', async () => {
        await invoke(s.secured ? 'unsecure_site' : 'secure_site', { name: s.name });
        await refreshSites();
      });

      const unlinkBtn = document.createElement('button');
      unlinkBtn.textContent = 'Unlink';
      unlinkBtn.addEventListener('click', async () => {
        await invoke('unlink_site', { name: s.name });
        await refreshSites();
      });

      right.appendChild(sslBtn);
      right.appendChild(unlinkBtn);

      row.appendChild(left);
      row.appendChild(right);
      list.appendChild(row);
    }
  } catch (e) { console.error('Failed to load sites:', e); }
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
    list.textContent = '';

    for (const v of versions) {
      const row = document.createElement('div');
      row.className = 'php-version-row';

      const label = document.createElement('span');
      if (v.active) label.className = 'php-active';
      label.textContent = 'PHP ' + v.version + (v.active ? ' (active)' : '');

      const right = document.createElement('div');
      const pathSpan = document.createElement('span');
      pathSpan.className = 'site-path';
      pathSpan.textContent = v.path;
      right.appendChild(pathSpan);

      if (!v.active) {
        const btn = document.createElement('button');
        btn.textContent = 'Switch';
        btn.addEventListener('click', async () => {
          await invoke('switch_php', { version: v.version });
          await refreshPhp();
          await refreshServices();
        });
        right.appendChild(btn);
      }

      row.appendChild(label);
      row.appendChild(right);
      list.appendChild(row);
    }
  } catch (e) { console.error('Failed to load PHP versions:', e); }
}

// Dump streaming
let dumpPaused = false;
let dumpBuffer = [];
let dumpLineCount = 0;

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
  dumpLineCount++;

  if (dumpLineCount > MAX_LINES && dumpLineCount % 100 === 0) {
    const lines = output.textContent.split('\n');
    output.textContent = lines.slice(lines.length - MAX_LINES).join('\n');
    dumpLineCount = MAX_LINES;
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
  dumpLineCount = 0;
});


// Initial load
refreshServices();
