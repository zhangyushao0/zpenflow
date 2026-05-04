// Penflow GUI frontend.
//
// Talks to the Rust backend via Tauri's `invoke` (commands) and
// `listen` (events). Pure ES module; no framework dependencies.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

// ---------- status ----------

const STATUS_PILL_CLASSES = ["connected", "listening", "error", "stopped"];

function renderStatus(state) {
    const pill = $("status-pill");
    const detail = $("status-detail");
    const toggle = $("btn-toggle");

    pill.classList.remove(...STATUS_PILL_CLASSES);

    switch (state.state) {
        case "stopped":
            pill.textContent = "paused";
            pill.classList.add("stopped");
            detail.textContent = "Service is paused. Click Resume to start listening.";
            toggle.textContent = "Resume";
            break;
        case "listening":
            pill.textContent = "listening";
            pill.classList.add("listening");
            detail.textContent = "Waiting for the Penflow Android app to connect…";
            toggle.textContent = "Pause";
            break;
        case "connecting":
            pill.textContent = "connecting…";
            pill.classList.add("listening");
            detail.textContent = `Negotiating with ${state.peer}`;
            toggle.textContent = "Pause";
            break;
        case "connected":
            pill.textContent = "connected";
            pill.classList.add("connected");
            detail.textContent = `${state.peer} → ${state.device_width}×${state.device_height}`;
            toggle.textContent = "Pause";
            break;
        case "disconnected":
            pill.textContent = "disconnected";
            pill.classList.add("listening");
            detail.textContent = "Last session ended cleanly. Re-arming…";
            toggle.textContent = "Pause";
            break;
        case "error":
            pill.textContent = "error";
            pill.classList.add("error");
            detail.textContent = state.message;
            toggle.textContent = "Pause";
            break;
        default:
            pill.textContent = state.state ?? "—";
            detail.textContent = JSON.stringify(state);
    }
}

// ---------- settings I/O ----------

async function loadSettings() {
    const s = await invoke("get_settings");
    $("bitrate").value = Math.round(s.bitrate_bps / 1_000_000);
    $("fps").value = String(s.fps);
    $("codec").value = s.codec;
    $("autostart").checked = s.autostart;
    $("run-as-admin").checked = s.run_as_admin;
    applyBindings(s.bindings);
}

function applyBindings(b) {
    setBinding(0, b.button_0);
    setBinding(1, b.button_1);
    setBinding(2, b.button_2);
}

function setBinding(slot, binding) {
    const row = document.querySelector(`.binding-row[data-slot="${slot}"]`);
    const kindSel = row.querySelector(".binding-kind");
    const argInp = row.querySelector(".binding-arg");
    kindSel.value = binding.kind;
    switch (binding.kind) {
        case "key_tap":
        case "key_hold":
            argInp.value = binding.key ?? "";
            argInp.placeholder = "key";
            argInp.style.display = "";
            break;
        case "key_chord":
            argInp.value = (binding.keys ?? []).join("+");
            argInp.placeholder = "Ctrl+Z";
            argInp.style.display = "";
            break;
        case "mouse_button":
            argInp.value = binding.button ?? "left";
            argInp.placeholder = "left/right/middle";
            argInp.style.display = "";
            break;
        case "none":
        case "eraser_toggle":
            argInp.value = "";
            argInp.style.display = "none";
            break;
    }
}

function readBinding(slot) {
    const row = document.querySelector(`.binding-row[data-slot="${slot}"]`);
    const kind = row.querySelector(".binding-kind").value;
    const arg = row.querySelector(".binding-arg").value.trim();
    switch (kind) {
        case "none":
            return { kind: "none" };
        case "key_tap":
            return { kind: "key_tap", key: arg };
        case "key_hold":
            return { kind: "key_hold", key: arg };
        case "key_chord":
            return { kind: "key_chord", keys: arg.split("+").map((s) => s.trim()).filter(Boolean) };
        case "mouse_button":
            return { kind: "mouse_button", button: arg.toLowerCase() || "left" };
        case "eraser_toggle":
            return { kind: "eraser_toggle" };
    }
}

async function save() {
    const settings = {
        bitrate_bps: Number($("bitrate").value) * 1_000_000,
        fps: Number($("fps").value),
        codec: $("codec").value,
        bindings: {
            button_0: readBinding(0),
            button_1: readBinding(1),
            button_2: readBinding(2),
        },
        autostart: $("autostart").checked,
        run_as_admin: $("run-as-admin").checked,
    };
    const status = $("save-status");
    status.textContent = "saving…";
    try {
        await invoke("save_settings", { new: settings });
        status.textContent = "saved";

        // If user just turned on "run as admin" and we aren't elevated,
        // offer to relaunch.
        if (settings.run_as_admin) {
            const elevated = await invoke("is_elevated");
            if (!elevated) {
                status.textContent = "relaunching with admin…";
                await invoke("relaunch_as_admin");
            }
        }
    } catch (e) {
        status.textContent = "error: " + e;
    }
    setTimeout(() => { status.textContent = ""; }, 3000);
}

// ---------- service toggle ----------

async function toggleService() {
    const current = await invoke("get_status");
    if (current.state === "stopped") {
        await invoke("start_service");
    } else {
        await invoke("stop_service");
    }
}

// ---------- bindings ----------

document.querySelectorAll(".binding-kind").forEach((sel) => {
    sel.addEventListener("change", () => {
        const slot = Number(sel.closest(".binding-row").dataset.slot);
        // Force-refresh the input visibility / placeholder.
        setBinding(slot, { kind: sel.value, key: "", keys: [], button: "left" });
    });
});

$("btn-save").addEventListener("click", save);
$("btn-toggle").addEventListener("click", toggleService);

listen("service-state", (ev) => renderStatus(ev.payload));

// Initial pass.
loadSettings().catch((e) => console.error("loadSettings failed", e));
invoke("get_status").then(renderStatus).catch((e) => console.error(e));
