import { formatDuration, trackLabels } from "./library-core.js";
import { library } from "./library.js";

const $ = (id) => document.getElementById(id);
const invoke = (command, args) => window.__TAURI__.core.invoke(command, args);
const trackHints = ["System audio", "Microphone"];
const states = {
  idle: "Ready",
  recording: "Recording",
  paused: "Paused",
  saved: "Saved locally",
  interrupted: "Recording interrupted",
  testing: "Testing devices",
};

let snapshot = { phase: "idle", frames: 0, tracks: [] };
let currentView = "recorder";
let ready = false;
let busy = false;
let revision = 0;
let pendingNotice = "";
let pendingError = "";
let lastMessage = "";
const isActive = () => ["recording", "paused"].includes(snapshot.phase);

const navigation = [...document.querySelectorAll("[data-view]")];

function buttonLabel(button, label, icon) {
  button.setAttribute("aria-label", label);
  button.title = label;
  if (icon) button.querySelector("use").setAttribute("href", `#icon-${icon}`);
}

function notice(element, message, error = false) {
  if (element.textContent !== message) element.textContent = message;
  element.classList.toggle("error", error);
}

const waveforms = [...document.querySelectorAll("[data-wave]")].map((element) => {
  const bars = Array.from({ length: 30 }, () => document.createElement("span"));
  element.append(...bars);
  return { bars, levels: bars.map(() => 0) };
});
const timers = [...document.querySelectorAll("[data-track-time]")];
const deviceLabels = [...document.querySelectorAll("[data-device]")];

function updateWave(index, level) {
  const { bars, levels } = waveforms[index];
  if (snapshot.phase === "recording") {
    levels.shift();
    levels.push(level);
  } else {
    levels.fill(0);
  }
  bars.forEach((bar, i) => {
    const envelope = 0.58 + 0.42 * Math.sin((i / (bars.length - 1)) * Math.PI);
    bar.style.transform = `scaleY(${(0.09 + 0.82 * levels[i]).toFixed(3)})`;
    bar.style.opacity = (0.19 + 0.81 * levels[i] * envelope).toFixed(3);
  });
}

function render() {
  const { phase, tracks, frames, message, bookmarks } = snapshot;
  const active = isActive();
  const status = pendingNotice || states[phase];
  $("phase").dataset.state = phase;
  if ($("phase-text").textContent !== status) $("phase-text").textContent = status;
  $("session-time").textContent = formatDuration(frames);
  $("record-button").disabled = busy || !ready;
  for (const link of navigation) {
    link.disabled = link.dataset.view !== currentView && (active || busy || !ready);
  }
  buttonLabel($("record-button"), active ? "Stop and save" : "Start recording", active ? "stop" : "mic");
  $("pause-button").hidden = !active;
  $("bookmark-button").hidden = !active;
  $("pause-button").disabled = busy;
  $("bookmark-button").disabled = busy;
  buttonLabel($("pause-button"), phase === "paused" ? "Resume recording" : "Pause recording", phase === "paused" ? "play" : "pause");
  buttonLabel($("bookmark-button"), bookmarks ? `Add bookmark · ${bookmarks} saved` : "Add bookmark", "bookmark");

  for (let i = 0; i < trackLabels.length; i++) {
    const track = phase === "testing" ? undefined : tracks[i];
    timers[i].textContent = formatDuration(track?.frames ?? 0);
    deviceLabels[i].textContent = track?.issue || (active ? track?.device : null) || trackHints[i];
    deviceLabels[i].title = track?.issue || track?.device || trackHints[i];
    deviceLabels[i].classList.toggle("issue", Boolean(track?.issue));
    updateWave(i, track?.level ?? 0);
  }

  if (message !== lastMessage) {
    lastMessage = message;
    pendingError = "";
  }
  notice($("notice"), pendingError || message || (phase === "saved" ? "Both WAV tracks are in Recordings." : ""),
    Boolean(pendingError || message));
  $("view-recording").hidden = !snapshot.sessionId || active || busy;
  renderDeviceTest();
}

async function poll() {
  const generation = revision;
  try {
    if (!busy) {
      const data = await invoke("recording_status");
      if (generation === revision && !busy) {
        snapshot = data;
        ready = true;
        render();
      }
    }
  } catch (error) {
    notice($("notice"), String(error), true);
  } finally {
    setTimeout(poll, snapshot.phase === "recording" || snapshot.phase === "testing" ? 65 : 350);
  }
}

async function record(action) {
  if (busy) return;
  revision++;
  busy = true;
  pendingError = "";
  pendingNotice = { start: "Starting…", stop: "Saving…", pause: "Pausing…", resume: "Resuming…" }[action] ?? "";
  render();
  try {
    snapshot = await invoke("recording_action", { action });
  } catch (error) {
    snapshot = await invoke("recording_status").catch(() => snapshot);
    lastMessage = snapshot.message;
    pendingError = String(error);
  } finally {
    busy = false;
    pendingNotice = "";
    render();
    if (action === "start" && isActive()) $("record-button").focus();
  }
}

$("record-button").addEventListener("click", () => record(isActive() ? "stop" : "start"));
$("pause-button").addEventListener("click", () => record(snapshot.phase === "paused" ? "resume" : "pause"));
$("bookmark-button").addEventListener("click", () => record("bookmark"));

async function navigate(view, sessionId) {
  if (view === currentView || busy || settingsBusy || library.busy || isActive()) return;
  if (currentView === "library") library.close();
  if (currentView === "settings" && snapshot.phase === "testing") {
    try { snapshot = await invoke("recording_action", { action: "end_test" }); }
    catch (error) { pendingError = String(error); }
  }
  currentView = view;
  for (const link of navigation) {
    $(`${link.dataset.view}-view`).hidden = link.dataset.view !== view;
    if (link.dataset.view === view) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
  window.scrollTo(0, 0);
  if (view === "library") {
    $("history-heading").focus();
    await library.open(sessionId);
  } else if (view === "settings") {
    $("settings-heading").focus();
    await loadPreferences();
  } else {
    $("record-button").focus();
  }
  render();
}

for (const link of navigation) link.addEventListener("click", () => navigate(link.dataset.view));
$("view-recording").addEventListener("click", () => navigate("library", snapshot.sessionId));

let settingsBusy = false;
let pendingFolder = null;
const settingsNotice = $("settings-notice");
const preferenceControls = ["output-device", "mic-device", "browse-folder", "test-button", "save-settings"];
function lockSettings(locked) {
  settingsBusy = locked;
  for (const id of preferenceControls) $(id).disabled = locked;
}
function formatBytes(bytes) {
  if (bytes == null) return "Storage availability could not be read.";
  return `${new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 }).format(bytes / 1024 ** 3)} GB free`;
}
function deviceOptions(select, devices, selected) {
  select.replaceChildren(new Option("Windows default", ""));
  for (const { id, name } of devices) select.add(new Option(name, id));
  if (selected && !devices.some(({ id }) => id === selected)) {
    select.add(new Option("Unavailable device (saved)", selected));
  }
  select.value = selected ?? "";
}
function setPreferences(data) {
  pendingFolder = null;
  deviceOptions($("output-device"), data.outputs, data.output);
  deviceOptions($("mic-device"), data.microphones, data.microphone);
  $("recordings-folder").value = data.folder;
  $("recordings-folder").title = data.folder;
  $("free-space").textContent = formatBytes(data.availableBytes);
  notice(settingsNotice, data.deviceWarnings.join(" · "), Boolean(data.deviceWarnings.length));
}
function selection() {
  return {
    microphone: $("mic-device").value || null,
    output: $("output-device").value || null,
  };
}
function renderDeviceTest() {
  const testing = snapshot.phase === "testing";
  $("test-button").textContent = testing ? "End test" : "Test devices";
  $("test-hint").hidden = !testing;
  for (const [i, name] of ["output", "mic"].entries()) {
    const track = testing ? snapshot.tracks[i] : undefined;
    $(`test-${name}`).value = track?.level ?? 0;
    const issue = track?.issue ?? "";
    const element = $(`test-${name}-issue`);
    if (element.textContent !== issue) element.textContent = issue;
  }
}

async function loadPreferences() {
  lockSettings(true);
  notice(settingsNotice, "Loading devices…");
  try {
    setPreferences(await invoke("preferences"));
  } catch (error) {
    notice(settingsNotice, String(error), true);
  } finally {
    lockSettings(false);
  }
}
$("browse-folder").addEventListener("click", async () => {
  lockSettings(true);
  try {
    const folder = await invoke("choose_folder");
    if (folder) {
      pendingFolder = folder;
      $("recordings-folder").value = folder;
      $("recordings-folder").title = folder;
      $("free-space").textContent = "Save settings to use this folder.";
    }
  } catch (error) {
    notice(settingsNotice, String(error), true);
  } finally {
    lockSettings(false);
  }
});
$("settings-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (settingsBusy) return;
  lockSettings(true);
  try {
    const data = await invoke("save_preferences", {
      ...selection(),
      folder: pendingFolder,
    });
    setPreferences(data);
    notice(settingsNotice, "Settings saved.");
  } catch (error) {
    notice(settingsNotice, String(error), true);
  } finally {
    lockSettings(false);
  }
});
$("test-button").addEventListener("click", async () => {
  lockSettings(true);
  notice(settingsNotice, "");
  try {
    snapshot = snapshot.phase === "testing"
      ? await invoke("recording_action", { action: "end_test" })
      : await invoke("test_devices", selection());
    if (currentView !== "settings" && snapshot.phase === "testing") {
      snapshot = await invoke("recording_action", { action: "end_test" });
    }
    render();
  } catch (error) {
    notice(settingsNotice, String(error), true);
  } finally {
    lockSettings(false);
  }
});
render();
poll();
