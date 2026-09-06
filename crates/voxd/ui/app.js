// Vox settings UI. Plain DOM, organised like a React app: a store, pure component
// functions returning elements, and a render() that rebuilds from state.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// ---------------------------------------------------------------------------------------------
// Tiny element helper: h(tag, props, ...children)

function h(tag, props, ...children) {
  const el = document.createElement(tag);
  if (props) {
    for (const [k, v] of Object.entries(props)) {
      if (v == null || v === false) continue;
      if (k === "class") el.className = v;
      else if (k === "html") el.innerHTML = v;
      else if (k.startsWith("on")) el.addEventListener(k.slice(2).toLowerCase(), v);
      else if (k === "style" && typeof v === "object") Object.assign(el.style, v);
      else if (k in el && typeof v !== "string") el[k] = v;
      else el.setAttribute(k, v === true ? "" : v);
    }
  }
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

const icons = {
  mic: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="2" width="6" height="12" rx="3"/><path d="M5 10a7 7 0 0 0 14 0"/><path d="M12 17v4M8 21h8"/></svg>`,
  key: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="6" width="18" height="12" rx="3"/><path d="M7 10h.01M11 10h.01M15 10h.01M7 14h10"/></svg>`,
  cpu: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="5" width="14" height="14" rx="2"/><rect x="9" y="9" width="6" height="6"/><path d="M9 2v3M15 2v3M9 19v3M15 19v3M2 9h3M2 15h3M19 9h3M19 15h3"/></svg>`,
  sliders: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3"/><path d="M1 14h6M9 8h6M17 16h6"/></svg>`,
  activity: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 12h-4l-3 9L9 3l-3 9H2"/></svg>`,
  check: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>`,
  x: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M18 6 6 18M6 6l12 12"/></svg>`,
  refresh: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-2.6-6.4"/><path d="M21 3v6h-6"/></svg>`,
  folder: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>`,
};
const svg = (name) => h("span", { html: icons[name], style: { display: "contents" } });

// ---------------------------------------------------------------------------------------------
// Store

const store = {
  config: null,
  status: null,
  devices: [],
  models: [],
  paths: null,
  capturing: false,
  meter: null, // { device, db, peak }
  error: null,
};

let saveTimer = null;
function updateConfig(mutate) {
  mutate(store.config);
  render();
  clearTimeout(saveTimer);
  saveTimer = setTimeout(async () => {
    try {
      await invoke("set_config", { config: store.config });
      toast("Saved", "check");
    } catch (e) {
      toast(`Couldn't save: ${e}`, "x", true);
    }
  }, 250);
}

// ---------------------------------------------------------------------------------------------
// Toast

let toastTimer = null;
function toast(text, icon = "check", isError = false) {
  const root = document.getElementById("toast-root");
  root.replaceChildren(h("div", { class: `toast ${isError ? "error" : ""}` }, svg(icon), text));
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => root.replaceChildren(), isError ? 4000 : 1600);
}

// ---------------------------------------------------------------------------------------------
// Components

function Topbar() {
  const s = store.status || {};
  let state = s.state || "idle";
  let text = "Idle";
  if (state === "idle" && !s.engine_loaded) {
    state = s.last_error ? "error" : "loading";
    text = s.last_error ? "Engine error" : "Loading model…";
  } else {
    text = {
      idle: `Ready — hold ${s.hotkey || "…"}`,
      arming: "Opening microphone…",
      recording: "Recording",
      finalizing: "Transcribing…",
      injecting: "Typing…",
    }[state] || state;
  }
  return h(
    "header",
    { class: "topbar" },
    h("div", { class: "brand" },
      h("div", { class: "logo" }, svg("mic")),
      h("div", null, h("div", { class: "brand-name" }, "Vox"), h("div", { class: "brand-sub" }, "Local push-to-talk dictation"))
    ),
    h("div", { class: "spacer" }),
    h("div", { class: "status-pill", "data-state": state }, h("span", { class: "dot" }), text)
  );
}

function Card(icon, title, desc, ...body) {
  return h(
    "section",
    { class: "card" },
    h("div", { class: "card-head" },
      h("div", { class: "card-icon" }, svg(icon)),
      h("div", null, h("div", { class: "card-title" }, title), h("div", { class: "card-desc" }, desc))
    ),
    ...body
  );
}

function Switch(on, onChange) {
  return h("button", {
    class: `switch ${on ? "on" : ""}`,
    role: "switch",
    "aria-checked": on ? "true" : "false",
    onClick: () => onChange(!on),
  });
}

function ToggleRow(title, desc, on, onChange) {
  return h("div", { class: "toggle-row" },
    h("div", null, h("div", { class: "t" }, title), h("div", { class: "d" }, desc)),
    Switch(on, onChange)
  );
}

// Keys Windows leaves alone, best first. `why` is shown next to the suggestion.
const SUGGESTED_HOTKEYS = [
  { chord: "RCtrl", label: "Right Ctrl", why: "Windows never uses it on its own" },
  { chord: "CapsLock", label: "Caps Lock", why: "big target; Vox suppresses the toggle" },
  { chord: "ScrollLock", label: "Scroll Lock", why: "does nothing on a modern PC" },
  { chord: "Pause", label: "Pause / Break", why: "does nothing on a modern PC" },
  { chord: "Mouse4", label: "Mouse 4", why: "your mouse's side button" },
  { chord: "Ctrl+Shift+Space", label: "Ctrl+Shift+Space", why: "free in Windows" },
];

function HotkeyCard() {
  const hk = store.config.hotkey;
  const taken = SUGGESTED_HOTKEYS.filter((s) => s.chord !== hk.chord);
  return Card("key", "Hotkey", "Hold (or tap) to dictate. Pick something Windows doesn't already use.",
    h("div", { class: "row between wrap" },
      h("div", { class: "row" },
        h("span", { class: "kbd" }, hk.chord),
        h("span", { class: "hint" }, "current binding")
      ),
      h("button", { class: "btn primary", onClick: captureHotkey }, "Change…")
    ),
    h("div", { class: "col" },
      h("div", { class: "label" }, "SUGGESTIONS — CLICK TO USE"),
      h("div", { class: "row wrap", style: { gap: "6px" } },
        ...taken.map((s) =>
          h("button", {
            class: "btn sm", title: s.why,
            onClick: () => { updateConfig((c) => (c.hotkey.chord = s.chord)); toast(`Bound to ${s.label}`); },
          }, s.label)
        )
      ),
      h("div", { class: "hint" }, "Avoid Windows-key combos — Windows claims most of them. Ctrl+Alt+Del and Win+L can't be intercepted by any app.")
    ),
    h("div", { class: "divider" }),
    h("div", { class: "row between wrap" },
      h("div", { class: "col" },
        h("div", { class: "label" }, "MODE"),
        h("div", { class: "hint" }, hk.mode === "toggle" ? "Tap to start, tap again to stop." : "Hold while talking, release to type.")
      ),
      h("div", { class: "segmented" },
        h("button", { class: hk.mode === "push_to_talk" ? "active" : "", onClick: () => updateConfig((c) => (c.hotkey.mode = "push_to_talk")) }, "Hold"),
        h("button", { class: hk.mode === "toggle" ? "active" : "", onClick: () => updateConfig((c) => (c.hotkey.mode = "toggle")) }, "Toggle")
      )
    ),
    hk.mode === "push_to_talk" &&
      h("div", { class: "col" },
        h("div", { class: "row between" },
          h("div", { class: "label" }, "IGNORE TAPS SHORTER THAN"),
          h("span", { class: "hint" }, `${hk.min_press_ms} ms`)
        ),
        h("input", {
          class: "range", type: "range", min: 0, max: 600, step: 50, value: hk.min_press_ms,
          onInput: (e) => { hk.min_press_ms = Number(e.target.value); e.target.parentElement.querySelector(".hint").textContent = `${hk.min_press_ms} ms`; },
          onChange: (e) => updateConfig((c) => (c.hotkey.min_press_ms = Number(e.target.value))),
        })
      )
  );
}

function deviceLabel(d) {
  const tags = [];
  if (d.is_default_communications) tags.push("headset default");
  if (d.is_default_console) tags.push("default");
  if (d.is_bluetooth) tags.push("bluetooth");
  return { name: d.name, tags };
}

// Which device the current selection actually resolves to right now.
function effectiveDevice() {
  const dev = store.config.audio.device;
  if (dev.kind === "specific") return store.devices.find((d) => d.id === dev.id);
  const flag = dev.kind === "default_communications" ? "is_default_communications" : "is_default_console";
  return store.devices.find((d) => d[flag]);
}

function MicCard() {
  const dev = store.config.audio.device;
  const kind = dev.kind;
  const specificId = kind === "specific" ? dev.id : null;
  const options = [
    { kind: "default_communications", title: "Follow Windows' headset default", desc: "AirPods and other headsets take over the moment they connect. Recommended." },
    { kind: "default_console", title: "Follow Windows' default microphone", desc: "Whatever Sound settings shows as the default input." },
  ];
  const meter = store.meter;
  const active = effectiveDevice();
  return Card("mic", "Microphone", "Which input Vox opens when you press the hotkey. It is only held open while you talk.",
    active?.is_bluetooth &&
      h("div", { class: "alert info" },
        h("strong", null, "Your music will sound muffled while you dictate."),
        " Bluetooth headsets can't send high-quality audio and carry a microphone at the same time, so Windows drops ",
        active.name.replace(/^Headset \(|\)$/g, ""),
        " into call mode whenever Vox records — a limitation of Bluetooth itself, not something Vox can work around. It sounds normal again a moment after you release the key. To avoid it entirely, choose a wired or USB microphone below and keep the headphones for listening."
      ),
    h("div", { class: "radio-list" },
      ...options.map((o) =>
        h("div", { class: `radio ${kind === o.kind ? "active" : ""}`, onClick: () => updateConfig((c) => (c.audio.device = { kind: o.kind })) },
          h("span", { class: "rb" }),
          h("div", null, h("div", { class: "rt" }, o.title), h("div", { class: "rd" }, o.desc))
        )
      ),
      h("div", { class: `radio ${kind === "specific" ? "active" : ""}`, style: { flexWrap: "wrap" },
          onClick: (e) => { if (e.target.tagName !== "SELECT" && kind !== "specific") pickDevice(store.devices[0]); } },
        h("span", { class: "rb" }),
        h("div", { style: { flex: 1 } },
          h("div", { class: "rt" }, "Always use a specific device"),
          h("div", { class: "rd" }, "Stays on this device even when Windows changes its default.")
        ),
        kind === "specific" &&
          h("select", { class: "select", style: { marginTop: "8px", flexBasis: "100%" },
              onChange: (e) => pickDevice(store.devices.find((d) => d.id === e.target.value)) },
            ...store.devices.map((d) => h("option", { value: d.id, selected: d.id === specificId }, deviceLabel(d).name)),
            !store.devices.some((d) => d.id === specificId) && specificId && h("option", { value: specificId, selected: true }, `${dev.name || specificId} (not connected)`)
          )
      )
    ),
    h("div", { class: "col" },
      h("div", { class: "row between" },
        h("div", { class: "label" }, "DETECTED INPUTS"),
        h("button", { class: "btn sm", onClick: refreshDevices }, svg("refresh"), "Refresh")
      ),
      store.devices.length === 0
        ? h("div", { class: "hint" }, "No active microphones found.")
        : h("div", { class: "stack", style: { gap: "4px" } },
            ...store.devices.map((d) => {
              const { name, tags } = deviceLabel(d);
              return h("div", { class: "row", style: { fontSize: "13px" } }, name, ...tags.map((t) => h("span", { class: "tag" }, t)));
            })
          )
    ),
    h("div", { class: "divider" }),
    h("div", { class: "row" },
      h("button", { class: `btn ${meter ? "" : "primary"}`, onClick: toggleMeter }, meter ? "Stop test" : "Test microphone"),
      h("div", { class: "meter" }, h("div", { class: "fill", id: "meter-fill" })),
      h("span", { class: "meter-db", id: "meter-db" }, meter ? "" : "—")
    ),
    meter && h("div", { class: "hint" }, `Listening on ${meter.device}. Speak — the bar should move.`)
  );
}

function EngineCard() {
  const eng = store.config.engine;
  const models = store.models;
  const current = models.find((m) => m.name === eng.model);
  const gpu = store.paths?.gpu_build;
  return Card("cpu", "Speech engine", "whisper.cpp, entirely on this PC. Bigger models are more accurate but slower.",
    h("div", { class: "col" },
      h("div", { class: "label" }, "MODEL"),
      h("select", { class: "select", onChange: (e) => updateConfig((c) => (c.engine.model = e.target.value)) },
        ...models.map((m) => h("option", { value: m.name, selected: m.name === eng.model }, `${m.name}  ·  ${m.size_mb} MB`)),
        !current && h("option", { value: eng.model, selected: true }, `${eng.model} (not found)`)
      ),
      h("div", { class: "hint" },
        "base.en ≈ 0.2 s per dictation on CPU · small.en ≈ 0.7 s, a little more accurate · large-v3-turbo needs a GPU build.")
    ),
    h("div", { class: "row between wrap" },
      h("div", { class: "col" },
        h("div", { class: "label" }, "BACKEND"),
        h("div", { class: "hint" }, gpu ? "GPU acceleration is compiled in." : "This build is CPU-only. GPU (CUDA/Vulkan) comes with a GPU build.")
      ),
      h("div", { class: "segmented" },
        h("button", { class: eng.backend === "auto" ? "active" : "", disabled: !gpu, onClick: () => updateConfig((c) => (c.engine.backend = "auto")) }, "Auto"),
        h("button", { class: eng.backend === "cpu" || !gpu ? "active" : "", onClick: () => updateConfig((c) => (c.engine.backend = "cpu")) }, "CPU")
      )
    ),
    h("div", { class: "row between wrap" },
      h("div", { class: "col" },
        h("div", { class: "label" }, "ACCURACY / SPEED"),
        h("div", { class: "hint" }, eng.beam_size > 1 ? "Beam search: a bit more accurate, slower." : "Greedy: fastest.")
      ),
      h("div", { class: "segmented" },
        h("button", { class: eng.beam_size <= 1 ? "active" : "", onClick: () => updateConfig((c) => (c.engine.beam_size = 1)) }, "Fast"),
        h("button", { class: eng.beam_size > 1 ? "active" : "", onClick: () => updateConfig((c) => (c.engine.beam_size = 2)) }, "Careful")
      )
    ),
    ToggleRow("Keep the model loaded", "Instant first dictation; uses RAM while idle.", eng.preload, (v) => updateConfig((c) => (c.engine.preload = v))),
    store.paths && h("div", { class: "hint" }, "Add models: drop ggml-*.bin files into ", h("a", { class: "link", onClick: () => invoke("open_path", { path: store.paths.models_dir }) }, store.paths.models_dir))
  );
}

function BehaviorCard() {
  const c = store.config;
  const strat = c.injection.strategy;
  return Card("sliders", "Behavior", "How Vox runs and how text is delivered.",
    ToggleRow("Start with Windows", "Run in the tray at login.", c.behavior.autostart, (v) => updateConfig((cfg) => (cfg.behavior.autostart = v))),
    ToggleRow("Sounds", "A short tick when recording starts and stops.", c.behavior.sounds, (v) => updateConfig((cfg) => (cfg.behavior.sounds = v))),
    h("div", { class: "divider" }),
    h("div", { class: "col" },
      h("div", { class: "label" }, "HOW TEXT IS TYPED"),
      h("select", { class: "select", onChange: (e) => updateConfig((cfg) => {
          cfg.injection.strategy = e.target.value === "auto" ? { kind: "auto", clipboard_threshold: 120 } : { kind: e.target.value };
        }) },
        h("option", { value: "auto", selected: strat.kind === "auto" }, "Automatic — type short text, paste long text"),
        h("option", { value: "unicode", selected: strat.kind === "unicode" }, "Always type keystrokes (works everywhere, slower on long text)"),
        h("option", { value: "clipboard", selected: strat.kind === "clipboard" }, "Always paste via clipboard (instant)")
      )
    )
  );
}

function ActivityCard() {
  const s = store.status || {};
  return Card("activity", "Activity", "What Vox did most recently.",
    s.last_error && h("div", { class: "alert" }, s.last_error),
    h("div", { class: `transcript ${s.last_transcript ? "" : "empty"}` }, s.last_transcript || "Nothing dictated yet. Hold the hotkey, talk, release."),
    h("div", { class: "stats" },
      h("div", { class: "stat" }, h("div", { class: "v" }, s.last_latency_ms != null ? `${s.last_latency_ms} ms` : "—"), h("div", { class: "k" }, "release → text")),
      h("div", { class: "stat" }, h("div", { class: "v" }, s.dictations ?? 0), h("div", { class: "k" }, "dictations this session")),
      h("div", { class: "stat" }, h("div", { class: "v", style: { fontSize: "15px", marginTop: "4px" } }, s.device || "—"), h("div", { class: "k" }, "last microphone")),
      h("div", { class: "stat" }, h("div", { class: "v", style: { fontSize: "15px", marginTop: "4px" } }, s.model || "—"), h("div", { class: "k" }, "model"))
    )
  );
}

function Footer() {
  const p = store.paths;
  return h("footer", { class: "footer" },
    h("span", null, `Vox ${p?.version || ""} · ${p?.gpu_build ? "GPU build" : "CPU build"}`),
    h("div", { class: "spacer" }),
    p && h("a", { class: "link", onClick: () => invoke("open_path", { path: p.log.replace(/\\[^\\]+$/, "") }) }, "Logs"),
    p && h("a", { class: "link", onClick: () => invoke("open_path", { path: p.config.replace(/\\[^\\]+$/, "") }) }, "Config folder"),
    h("a", { class: "link", style: { color: "var(--red)" }, onClick: () => invoke("quit_app") }, "Quit Vox")
  );
}

function CaptureOverlay() {
  return h("div", { class: "overlay", onClick: (e) => { if (e.target === e.currentTarget) cancelCapture(); } },
    h("div", { class: "modal" },
      h("div", { class: "ring" }, svg("key")),
      h("h3", null, "Press your new hotkey"),
      h("p", null, "Any key, a combo like Ctrl+Shift+Space, or a mouse side button. A modifier on its own — right Ctrl, say — binds when you let go of it. Nothing you press reaches Windows while this is open. Escape cancels."),
      h("button", { class: "btn", style: { marginTop: "8px" }, onClick: cancelCapture }, "Cancel")
    )
  );
}

function App() {
  if (store.error) {
    return h("div", { class: "app" }, Topbar(), h("main", { class: "content" }, h("div", { class: "alert span-2" }, store.error)));
  }
  if (!store.config) {
    return h("div", { class: "app" }, Topbar(), h("main", { class: "content" }, h("div", { class: "hint span-2" }, "Loading…")));
  }
  return h("div", { class: "app" },
    Topbar(),
    h("main", { class: "content" },
      HotkeyCard(),
      MicCard(),
      EngineCard(),
      BehaviorCard(),
      h("div", { class: "span-2" }, ActivityCard())
    ),
    Footer(),
    store.capturing && CaptureOverlay()
  );
}

function render() {
  const root = document.getElementById("root");
  const scroll = window.scrollY;
  root.replaceChildren(App());
  window.scrollTo(0, scroll);
  if (store.meter) updateMeterBar(store.meter.db, store.meter.peak);
}

// ---------------------------------------------------------------------------------------------
// Actions

async function captureHotkey() {
  if (store.capturing) return;
  store.capturing = true;
  render();
  try {
    const res = await invoke("capture_hotkey");
    store.capturing = false;
    if (res.chord) {
      updateConfig((c) => (c.hotkey.chord = res.chord));
      toast(`Bound to ${res.chord}`);
    } else {
      render();
      if (res.reason === "timeout") toast("No key detected — click Change and try again.", "x", true);
    }
  } catch (e) {
    store.capturing = false;
    render();
    toast(String(e), "x", true);
  }
}

async function cancelCapture() {
  await invoke("cancel_hotkey_capture");
  store.capturing = false;
  render();
}

function pickDevice(d) {
  if (!d) return;
  updateConfig((c) => (c.audio.device = { kind: "specific", id: d.id, name: d.name }));
}

async function refreshDevices() {
  try {
    store.devices = await invoke("list_devices");
  } catch (e) {
    toast(String(e), "x", true);
  }
  render();
}

async function toggleMeter() {
  if (store.meter) {
    await invoke("stop_meter");
    store.meter = null;
    render();
    return;
  }
  try {
    const device = await invoke("start_meter");
    store.meter = { device, db: -90, peak: 0 };
    render();
  } catch (e) {
    toast(`Microphone: ${e}`, "x", true);
  }
}

function updateMeterBar(db, peak) {
  const fill = document.getElementById("meter-fill");
  const label = document.getElementById("meter-db");
  if (!fill) return;
  const pct = Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
  fill.style.width = `${pct}%`;
  if (label) label.textContent = db <= -89 ? "silence" : `${db.toFixed(0)} dB`;
  if (peak > 0.98) fill.style.background = "var(--red)";
}

// ---------------------------------------------------------------------------------------------
// Boot

async function init() {
  render();
  try {
    const [config, status, devices, models, paths] = await Promise.all([
      invoke("get_config"), invoke("get_status"), invoke("list_devices").catch(() => []), invoke("list_models"), invoke("get_paths"),
    ]);
    Object.assign(store, { config, status, devices, models, paths });
  } catch (e) {
    store.error = `Could not load settings: ${e}`;
  }
  render();

  await listen("status", (e) => {
    store.status = e.payload;
    // Only the status-dependent parts change; a full re-render would drop focus in inputs.
    const top = document.querySelector(".topbar");
    if (top) top.replaceWith(Topbar());
    const act = document.querySelector(".span-2");
    if (act) act.replaceChildren(ActivityCard());
  });
  await listen("devices_changed", refreshDevices);
  await listen("level", (e) => {
    if (!store.meter) return;
    store.meter.db = e.payload.db;
    store.meter.peak = e.payload.peak;
    updateMeterBar(e.payload.db, e.payload.peak);
  });
  window.addEventListener("beforeunload", () => { if (store.meter) invoke("stop_meter"); });
}

init();
