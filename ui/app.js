const $ = (id) => document.getElementById(id);
const invoke = (command, args) => window.__TAURI__.core.invoke(command, args);
const sampleRate = 16_000;
const trackLabels = ["Interviewer", "Me"];
const trackHints = ["System audio", "Microphone"];
const states = {
  idle: "Ready",
  recording: "Recording",
  paused: "Paused",
  saved: "Saved locally",
  interrupted: "Recording interrupted",
  testing: "Testing devices",
};
const formatDate = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeStyle: "short",
});

let snapshot = { phase: "idle", frames: 0, tracks: [] };
let ready = false;
let busy = false;
let revision = 0;
let pendingNotice = "";
let pendingError = "";
let lastMessage = "";
const isActive = () => ["recording", "paused"].includes(snapshot.phase);

function formatDuration(frames, rate = sampleRate) {
  const seconds = Math.floor(frames / rate);
  return [Math.floor(seconds / 3600), Math.floor(seconds / 60) % 60, seconds % 60]
    .map((value) => String(value).padStart(2, "0")).join(":");
}

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
  $("history-button").disabled = active || busy || !ready;
  $("settings-button").disabled = active || busy || !ready;
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
    deviceLabels[i].textContent = track?.issue || trackHints[i];
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

const settings = $("settings-dialog");
const history = $("history-dialog");
const confirmDialog = $("confirm-dialog");
for (const dialog of [settings, history]) {
  dialog.querySelector(".dialog-close").addEventListener("click", () => dialog.close());
}

let settingsBusy = false;
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

$("settings-button").addEventListener("click", async () => {
  lockSettings(true);
  settings.showModal();
  notice(settingsNotice, "Loading devices…");
  try {
    setPreferences(await invoke("preferences"));
  } catch (error) {
    notice(settingsNotice, String(error), true);
    return;
  }
  lockSettings(false);
});
$("browse-folder").addEventListener("click", async () => {
  lockSettings(true);
  try {
    const folder = await invoke("choose_folder", { title: "Choose a recordings folder" });
    if (folder) {
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
      folder: $("recordings-folder").value,
    });
    setPreferences(data);
    settings.close();
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
    if (!settings.open && snapshot.phase === "testing") {
      snapshot = await invoke("recording_action", { action: "end_test" });
    }
    render();
  } catch (error) {
    notice(settingsNotice, String(error), true);
  } finally {
    lockSettings(false);
  }
});
settings.addEventListener("close", () => {
  if (snapshot.phase === "testing") {
    invoke("recording_action", { action: "end_test" }).catch((error) => notice($("notice"), String(error), true));
  }
});

const historyNotice = $("history-notice");
const historyList = $("history-list");
let historyBusy = false;
function itemText(tag, className, text) {
  const element = document.createElement(tag);
  element.className = className;
  element.textContent = text;
  return element;
}
function actionButton(label, onClick, style = "") {
  const button = itemText("button", `button ${style}`, label);
  button.type = "button";
  button.addEventListener("click", onClick);
  return button;
}
function historyItem(session) {
  const card = itemText("article", "history-item", "");
  const date = formatDate.format(new Date(session.startedAt));
  const duration = formatDuration(session.durationFrames, session.sampleRate);
  const metadata = [date, duration];
  if (session.state === "recovered") metadata.push("Recovered");
  if (session.state === "interrupted") metadata.push("Interrupted");
  if (session.bookmarks.length) {
    metadata.push(`Bookmarks ${session.bookmarks.map((frame) => formatDuration(frame, session.sampleRate)).join(", ")}`);
  }
  card.append(itemText("h3", "", session.name), itemText("p", "muted", metadata.join(" · ")));
  const issues = [...session.issues];
  for (const [index, track] of session.tracks.entries()) {
    if (track.discontinuities) issues.push(`${trackLabels[index]} reported ${track.discontinuities} audio gaps`);
  }
  if (issues.length) card.append(itemText("p", "muted capture-issue", issues.join(" · ")));
  const actions = itemText("div", "history-actions", "");
  actions.append(
    actionButton("Play Interviewer", () => fileAction(session, { kind: "play", track: 0 })),
    actionButton("Play Me", () => fileAction(session, { kind: "play", track: 1 })),
    actionButton("Open folder", () => fileAction(session, { kind: "open_folder" })),
    actionButton("Rename", () => fileAction(session, { kind: "rename" })),
    actionButton("Export", () => fileAction(session, { kind: "export" })),
    actionButton("Delete", () => fileAction(session, { kind: "delete" }), "delete"),
  );
  card.append(actions);
  return card;
}

async function loadHistory() {
  const { sessions, warnings } = await invoke("list_recordings");
  historyList.replaceChildren(...sessions.map(historyItem));
  const info = warnings.join(" · ") || (sessions.length ? "" : "Your saved recordings will appear here.");
  notice(historyNotice, info, Boolean(warnings.length));
}

function confirmAction(session, rename) {
  const nameInput = $("confirm-name");
  const action = $("confirm-action");
  $("confirm-title").textContent = rename ? "Rename recording" : "Delete recording?";
  $("confirm-copy").textContent = rename ? "" : `“${session.name}” and its files will be permanently deleted.`;
  $("confirm-copy").hidden = rename;
  nameInput.value = rename ? session.name : "";
  nameInput.hidden = !rename;
  nameInput.required = rename;
  action.classList.toggle("danger", !rename);
  action.textContent = rename ? "Rename" : "Delete";
  confirmDialog.returnValue = "";
  return new Promise((resolve) => {
    confirmDialog.addEventListener("close", () => {
      resolve(confirmDialog.returnValue === "confirm" ? (rename ? nameInput.value : true) : null);
    }, { once: true });
    confirmDialog.showModal();
    if (rename) {
      nameInput.focus();
      nameInput.select();
    } else {
      $("confirm-form").querySelector("[value=cancel]").focus();
    }
  });
}

async function fileAction(session, action) {
  if (historyBusy) return;
  historyBusy = true;
  try {
    if (action.kind === "rename") {
      const name = await confirmAction(session, true);
      if (!name) return;
      action.name = name;
    } else if (action.kind === "delete") {
      if (!await confirmAction(session, false)) return;
    } else if (action.kind === "export") {
      const destination = await invoke("choose_folder", { title: "Choose where to copy the recording" });
      if (!destination) return;
      action.destination = destination;
      notice(historyNotice, "Copying WAV files…");
    }
    const location = await invoke("recording_file_action", { id: session.id, action });
    if (action.kind === "rename" || action.kind === "delete") await loadHistory();
    if (location) notice(historyNotice, `Exported to ${location}`);
  } catch (error) {
    notice(historyNotice, String(error), true);
  } finally {
    historyBusy = false;
  }
}
$("history-button").addEventListener("click", async () => {
  if (historyBusy) return;
  historyBusy = true;
  historyList.replaceChildren();
  notice(historyNotice, "Loading recordings…");
  history.showModal();
  try {
    await loadHistory();
  } catch (error) {
    notice(historyNotice, String(error), true);
  } finally {
    historyBusy = false;
  }
});

render();
poll();
