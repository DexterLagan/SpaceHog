const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (s, r = document) => r.querySelector(s);
const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};

function fmt(n) {
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return (i === 0 ? n : n.toFixed(n >= 100 ? 0 : 1)) + " " + u[i];
}
function ago(secs) {
  if (!secs) return "never";
  const d = Math.max(0, Date.now() / 1000 - secs);
  if (d < 60) return "just now";
  if (d < 3600) return Math.floor(d / 60) + " min ago";
  if (d < 86400) return Math.floor(d / 3600) + " h ago";
  return Math.floor(d / 86400) + " d ago";
}

const drives = new Map();   // letter -> { view, section, scanEl, metaEl, list }
const expanded = new Set(); // "C:|Windows"
let scanning = false;

// A bar row: [bar + size label] [name]. Label goes inside the bar when it fits, else to its right.
function makeRow(size, max, nameNode, tooltip) {
  const line = el("div", "line");
  const cell = el("div", "cell");
  const bar = el("div", "bar");
  bar.style.width = (max ? (size / max) * 100 : 0) + "%";
  bar.appendChild(el("span", "in", fmt(size)));
  cell.append(bar, el("span", "out", fmt(size)));
  nameNode.title = tooltip;
  line.append(cell, nameNode);
  return line;
}

function fitLabels(root = document) {
  for (const cell of root.querySelectorAll(".cell")) {
    cell.classList.remove("inside");
    const bar = cell.firstChild, out = cell.lastChild;
    if (bar.offsetWidth >= out.offsetWidth + 6) cell.classList.add("inside");
  }
}
window.addEventListener("resize", () => fitLabels());

function renderDrive(v) {
  let d = drives.get(v.letter);
  if (!d) {
    const section = el("section", "drive");
    const head = el("header");
    const metaEl = el("div", "meta");
    const scanEl = el("div", "scan");
    head.append(el("h2", null, v.letter), metaEl, scanEl);
    const list = el("ul", "rows");
    section.append(head, list);
    $("#drives").appendChild(section);
    d = { section, metaEl, scanEl, list };
    drives.set(v.letter, d);
  }
  d.view = v;
  d.metaEl.textContent = `${fmt(v.total - v.free)} used of ${fmt(v.total)} · ${fmt(v.free)} free`;
  d.list.replaceChildren();
  if (!v.has_data && !v.hasData) {
    d.list.appendChild(el("li", "empty", "no data yet — scanning…"));
    return;
  }
  const max = v.folders.length ? v.folders[0].size : 0;
  for (const f of v.folders) {
    const li = el("li", "folder");
    const name = el("div", "name");
    const arrow = el("span", "arrow", "▶");
    name.append(arrow, f.name);
    const line = makeRow(f.size, max, name, `${v.letter}\\${f.name}`);
    line.dataset.drive = v.letter;
    line.dataset.rel = f.name;
    line.dataset.kind = "folder";
    const files = el("ul", "files");
    files.hidden = true;
    li.append(line, files);
    d.list.appendChild(li);
    line.addEventListener("click", () => toggle(v.letter, f.name, arrow, files));
    if (expanded.has(v.letter + "|" + f.name)) toggle(v.letter, f.name, arrow, files, true);
  }
  fitLabels(d.list);
}

async function toggle(letter, folder, arrow, files, forceOpen = false) {
  const key = letter + "|" + folder;
  if (!forceOpen && expanded.has(key)) {
    expanded.delete(key);
    files.hidden = true;
    arrow.textContent = "▶";
    return;
  }
  expanded.add(key);
  files.hidden = false;
  arrow.textContent = "▼";
  files.replaceChildren(el("li", "msg", "loading…"));
  try {
    const list = await invoke("top_files", { drive: letter, folder });
    files.replaceChildren();
    if (!list.length) files.appendChild(el("li", "msg", "no files"));
    const max = list.length ? list[0].size : 0;
    for (const f of list) {
      const i = f.path.lastIndexOf("\\");
      const name = el("div", "name");
      if (i >= 0) name.appendChild(el("span", "dir", f.path.slice(0, i + 1)));
      name.append(f.path.slice(i + 1));
      const line = makeRow(f.size, max, name, `${letter}\\${folder}\\${f.path}`);
      line.dataset.drive = letter;
      line.dataset.rel = folder + "\\" + f.path;
      line.dataset.kind = "file";
      const li = el("li");
      li.appendChild(line);
      files.appendChild(li);
    }
    fitLabels(files);
  } catch (e) {
    files.replaceChildren(el("li", "msg", "error: " + e));
  }
}

function setStatus(text, busy) {
  const s = $("#status");
  s.textContent = text;
  s.classList.toggle("busy", !!busy);
}

// ---- context menu + delete ----
let target = null;
const menu = $("#menu");
document.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  const line = e.target.closest(".line[data-rel]");
  if (!line) { menu.hidden = true; return; }
  target = { drive: line.dataset.drive, rel: line.dataset.rel, isFile: line.dataset.kind === "file" };
  menu.hidden = false;
  menu.style.left = Math.min(e.clientX, innerWidth - menu.offsetWidth - 8) + "px";
  menu.style.top = Math.min(e.clientY, innerHeight - menu.offsetHeight - 8) + "px";
});
document.addEventListener("click", () => (menu.hidden = true));
document.addEventListener("keydown", (e) => { if (e.key === "Escape") { menu.hidden = true; closeModal(); } });

menu.addEventListener("click", async (e) => {
  const act = e.target.dataset.act;
  if (!act || !target) return;
  if (act === "reveal") {
    try { await invoke("reveal", { drive: target.drive, rel: target.rel, isFile: target.isFile }); }
    catch (err) { setStatus("error: " + err); }
  } else if (act === "delete") {
    openModal(target);
  }
});

const modal = $("#modal");
let pending = null;
function openModal(t) {
  pending = t;
  $("#modal-path").textContent = `${t.drive}\\${t.rel}`;
  $("#modal-err").textContent = "";
  modal.hidden = false;
}
function closeModal() { modal.hidden = true; pending = null; }
$("#modal-cancel").addEventListener("click", closeModal);
$("#modal-ok").addEventListener("click", async () => {
  if (!pending) return;
  try {
    await invoke("delete_path", { drive: pending.drive, rel: pending.rel });
    closeModal();
    startScan(false); // incremental: the parent directory's mtime changed, so it gets re-read
  } catch (err) {
    $("#modal-err").textContent = String(err);
  }
});

// ---- scanning ----
function startScan(full) {
  invoke("start_scan", { full });
  scanning = true;
  $("#rescan").disabled = true;
  setStatus("scanning…", true);
}
$("#rescan").addEventListener("click", () => startScan(true));

listen("scan-progress", (e) => {
  if (!scanning) return;
  for (const p of e.payload) {
    const d = drives.get(p.drive);
    if (d) d.scanEl.textContent = `scanning ${p.files.toLocaleString()} files · ${fmt(p.bytes)}`;
  }
});
listen("drive-done", (e) => {
  renderDrive(e.payload);
  const d = drives.get(e.payload.letter);
  if (d) d.scanEl.textContent = "";
});
listen("scan-finished", () => {
  scanning = false;
  $("#rescan").disabled = false;
  for (const d of drives.values()) d.scanEl.textContent = "";
  setStatus("scan complete " + ago(Date.now() / 1000));
});

(async () => {
  try {
    for (const v of await invoke("get_state")) renderDrive(v);
    const newest = Math.max(0, ...[...drives.values()].map((d) => d.view.scannedAt || 0));
    if (newest) setStatus("cached from " + ago(newest));
  } catch (e) { setStatus("error: " + e); }
  startScan(false);
})();
