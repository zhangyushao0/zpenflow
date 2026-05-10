import React, { useEffect, useState, useCallback, useRef } from "react";
import {
    Button,
    Switch,
    Dropdown,
    Option,
    SpinButton,
    Field,
    Text,
    Title3,
    Subtitle2,
    Caption1,
    Badge,
    MessageBar,
    MessageBarBody,
    MessageBarTitle,
    MessageBarActions,
    Spinner,
    makeStyles,
    tokens,
    shorthands,
} from "@fluentui/react-components";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const useStyles = makeStyles({
    root: {
        display: "flex",
        flexDirection: "column",
        gap: "16px",
        padding: "24px 28px 20px",
        maxWidth: "720px",
        margin: "0 auto",
        height: "100vh",
        boxSizing: "border-box",
        overflowY: "auto",
    },
    header: {
        display: "flex",
        alignItems: "center",
        gap: "14px",
    },
    headerSpacer: { flex: 1 },
    title: { margin: 0 },
    statusDetail: {
        color: tokens.colorNeutralForeground3,
        overflow: "hidden",
        textOverflow: "ellipsis",
        whiteSpace: "nowrap",
        flex: 1,
    },
    card: {
        ...shorthands.padding("16px", "18px"),
        // Mica is the host window's backdrop. We paint a low-alpha
        // surface on top so cards read as cards but the desktop blur
        // stays visible. (Plain `tokens.colorNeutralBackground2` would
        // be opaque and would hide Mica entirely.)
        backgroundColor: "rgba(43, 43, 43, 0.55)",
        backdropFilter: "blur(20px) saturate(120%)",
        WebkitBackdropFilter: "blur(20px) saturate(120%)",
        ...shorthands.border("1px", "solid", tokens.colorNeutralStroke2),
        ...shorthands.borderRadius("8px"),
    },
    cardTitle: {
        marginBottom: "12px",
        textTransform: "uppercase",
        letterSpacing: "0.05em",
        color: tokens.colorNeutralForeground3,
    },
    rowGroup: {
        display: "grid",
        gridTemplateColumns: "minmax(0, 1fr) minmax(0, 1fr)",
        gap: "20px",
    },
    column: {
        minWidth: 0,
    },
    row: {
        display: "flex",
        alignItems: "center",
        justifyContent: "space-between",
        gap: "12px",
        minHeight: "36px",
    },
    resolutionField: {
        display: "flex",
        flexDirection: "column",
        alignItems: "stretch",
        gap: "6px",
        minHeight: "auto",
        marginTop: "2px",
        marginBottom: "8px",
    },
    rowLabel: {
        flex: 1,
        color: tokens.colorNeutralForeground1,
    },
    resolutionControl: {
        display: "flex",
        flexDirection: "column",
        gap: "8px",
        width: "100%",
        minWidth: 0,
    },
    resolutionDropdown: {
        width: "100%",
        minWidth: 0,
    },
    resolutionInputs: {
        display: "grid",
        gridTemplateColumns: "minmax(96px, 1fr) 20px minmax(96px, 1fr)",
        alignItems: "center",
        columnGap: "8px",
    },
    resolutionSeparator: {
        color: tokens.colorNeutralForeground3,
        textAlign: "center",
        lineHeight: "32px",
        minWidth: "20px",
        position: "relative",
        zIndex: 1,
    },
    resolutionSpin: {
        width: "100%",
        minWidth: 0,
    },
    bindingRow: {
        display: "flex",
        flexDirection: "column",
        gap: "8px",
        paddingTop: "10px",
        paddingBottom: "10px",
        ...shorthands.borderTop("1px", "solid", tokens.colorNeutralStroke3),
    },
    bindingHeader: {
        display: "flex",
        alignItems: "center",
        gap: "12px",
    },
    bindingHeaderLabel: {
        flex: "0 0 130px",
        color: tokens.colorNeutralForeground1,
    },
    bindingHeaderDropdown: {
        flex: "0 1 200px",
        minWidth: 0,
    },
    bindingDetail: {
        display: "flex",
        alignItems: "center",
        gap: "6px",
        flexWrap: "wrap",
        paddingLeft: "142px",
    },
    bindingDetailEmpty: {
        paddingLeft: "142px",
        color: tokens.colorNeutralForeground4,
        fontSize: "12px",
        fontStyle: "italic",
    },
    modBtn: {
        ...shorthands.padding("3px", "9px"),
        ...shorthands.border("1px", "solid", tokens.colorNeutralStroke2),
        ...shorthands.borderRadius("4px"),
        backgroundColor: "transparent",
        color: tokens.colorNeutralForeground3,
        fontSize: "12px",
        height: "26px",
        cursor: "pointer",
        userSelect: "none",
        ":hover": {
            backgroundColor: tokens.colorNeutralBackground1Hover,
        },
    },
    modBtnOn: {
        backgroundColor: tokens.colorBrandBackground2,
        color: tokens.colorBrandForeground2,
        ...shorthands.border("1px", "solid", tokens.colorBrandStroke1),
    },
    keyCapture: {
        ...shorthands.padding("3px", "10px"),
        ...shorthands.border("1px", "dashed", tokens.colorNeutralStroke1),
        ...shorthands.borderRadius("4px"),
        height: "26px",
        minWidth: "80px",
        display: "inline-flex",
        alignItems: "center",
        cursor: "pointer",
        color: tokens.colorNeutralForeground1,
        fontSize: "12px",
        fontFamily: "ui-monospace, 'Cascadia Mono', Consolas, monospace",
        backgroundColor: tokens.colorNeutralBackground1,
        ":hover": {
            ...shorthands.border("1px", "dashed", tokens.colorBrandStroke1),
        },
    },
    keyCaptureCapturing: {
        ...shorthands.border("1px", "solid", tokens.colorBrandStroke1),
        backgroundColor: tokens.colorBrandBackground2,
        color: tokens.colorBrandForeground2,
    },
    keyCaptureEmpty: {
        color: tokens.colorNeutralForeground4,
        fontStyle: "italic",
    },
    mouseGroup: {
        display: "inline-flex",
        ...shorthands.border("1px", "solid", tokens.colorNeutralStroke2),
        ...shorthands.borderRadius("4px"),
        overflow: "hidden",
    },
    mouseBtn: {
        ...shorthands.padding("3px", "12px"),
        ...shorthands.border("none"),
        borderRight: `1px solid ${tokens.colorNeutralStroke2}`,
        backgroundColor: "transparent",
        color: tokens.colorNeutralForeground3,
        fontSize: "12px",
        height: "26px",
        cursor: "pointer",
        ":hover": { backgroundColor: tokens.colorNeutralBackground1Hover },
    },
    mouseBtnOn: {
        backgroundColor: tokens.colorBrandBackground2,
        color: tokens.colorBrandForeground2,
    },
    footer: {
        display: "flex",
        justifyContent: "flex-end",
        alignItems: "center",
        gap: "12px",
        marginTop: "auto",
    },
    saveStatus: {
        marginRight: "auto",
        color: tokens.colorNeutralForeground3,
    },
    hint: {
        color: tokens.colorNeutralForeground4,
        fontSize: "12px",
        marginTop: "-4px",
        marginBottom: "8px",
    },
});

const MOD_ORDER = ["Ctrl", "Alt", "Shift", "Win"];
const DEFAULT_RESOLUTION = { width: 2880, height: 1800 };
const RESOLUTION_PRESETS = [
    { key: "2880x1800", label: "2880×1800 native", width: 2880, height: 1800 },
    { key: "2560x1600", label: "2560×1600", width: 2560, height: 1600 },
    { key: "1920x1200", label: "1920×1200", width: 1920, height: 1200 },
    { key: "1920x1080", label: "1920×1080", width: 1920, height: 1080 },
    { key: "1600x1000", label: "1600×1000", width: 1600, height: 1000 },
    { key: "1280x800", label: "1280×800", width: 1280, height: 800 },
];

function resolutionKey(resolution) {
    const key = `${resolution.width}x${resolution.height}`;
    return RESOLUTION_PRESETS.some((p) => p.key === key) ? key : "custom";
}

function resolutionLabel(resolution) {
    return RESOLUTION_PRESETS.find((p) => p.key === resolutionKey(resolution))?.label
        ?? `${resolution.width}×${resolution.height}`;
}

function numericSpinValue(data) {
    const raw = data.value ?? data.displayValue;
    const value = typeof raw === "number" ? raw : Number(raw);
    return Number.isFinite(value) ? Math.round(value) : null;
}

function statusBadge(state) {
    switch (state.state) {
        case "stopped":
            return { color: "warning", text: "paused" };
        case "preparing":
            return { color: "informative", text: "starting" };
        case "listening":
            return { color: "informative", text: "ready" };
        case "connecting":
            return { color: "informative", text: "connecting" };
        case "connected":
            return { color: "success", text: "connected" };
        case "disconnected":
            return { color: "informative", text: "ready" };
        case "error":
            return { color: "danger", text: "error" };
        default:
            return { color: "subtle", text: state.state ?? "—" };
    }
}

function statusDescription(state) {
    switch (state.state) {
        case "stopped": return "Service paused";
        case "preparing": return "Starting ADB daemon — first install can take 10–30 s";
        case "listening": return "Waiting for the Penflow Android app";
        case "connecting": return `Negotiating with ${state.peer}`;
        case "connected": return `${state.device_width}×${state.device_height}  ${state.peer}`;
        case "disconnected": return "Last session ended";
        case "error": return state.message;
        default: return "starting…";
    }
}

function splitKeySpec(spec) {
    const parts = (spec || "").split("+").map((x) => x.trim()).filter(Boolean);
    const mods = [];
    let key = "";
    for (const p of parts) {
        if (MOD_ORDER.includes(p)) mods.push(p);
        else key = p;
    }
    return { mods, key };
}

function joinKeySpec(mods, key) {
    return [...mods, key].filter(Boolean).join("+");
}

/** Convert a Rust `Binding` enum value into editor state. */
function bindingToState(binding) {
    const s = { kind: binding.kind, mods: [], key: "", mouse: "left" };
    switch (binding.kind) {
        case "key_tap":
        case "key_hold":
            ({ mods: s.mods, key: s.key } = splitKeySpec(binding.key ?? ""));
            break;
        case "key_chord":
            ({ mods: s.mods, key: s.key } = splitKeySpec((binding.keys ?? []).join("+")));
            break;
        case "mouse_button":
            s.mouse = binding.button ?? "left";
            break;
    }
    return s;
}

function stateToBinding(s) {
    switch (s.kind) {
        case "none":         return { kind: "none" };
        case "eraser_toggle":return { kind: "eraser_toggle" };
        case "key_tap":      return { kind: "key_tap",  key: joinKeySpec(s.mods, s.key) };
        case "key_hold":     return { kind: "key_hold", key: joinKeySpec(s.mods, s.key) };
        case "key_chord":    return { kind: "key_chord", keys: [...s.mods, s.key].filter(Boolean) };
        case "mouse_button": return { kind: "mouse_button", button: s.mouse };
    }
}

function BindingRow({ label, slot, onChange, styles }) {
    const [capturing, setCapturing] = useState(false);
    const captureRef = useRef(null);

    const setKind = (kind) => onChange({ ...slot, kind });
    const toggleMod = (m) => {
        const mods = slot.mods.includes(m)
            ? slot.mods.filter((x) => x !== m)
            : [...slot.mods, m];
        onChange({ ...slot, mods });
    };
    const setKey = (mods, key) => onChange({ ...slot, mods, key });
    const setMouse = (mouse) => onChange({ ...slot, mouse });
    const clear = () => onChange({ ...slot, mods: [], key: "" });

    const onKeyDown = (e) => {
        e.preventDefault();
        e.stopPropagation();
        if (e.key === "Control" || e.key === "Shift" || e.key === "Alt" || e.key === "Meta") {
            if (slot.kind === "key_hold") {
                const m = e.key === "Meta" ? "Win" : e.key;
                setKey([m], "");
                setCapturing(false);
            }
            return;
        }
        const mods = [];
        if (e.ctrlKey) mods.push("Ctrl");
        if (e.altKey) mods.push("Alt");
        if (e.shiftKey) mods.push("Shift");
        if (e.metaKey) mods.push("Win");
        let key = e.key;
        if (key === " ") key = "Space";
        else if (key.length === 1) key = key.toUpperCase();
        setKey(mods, key);
        setCapturing(false);
    };

    const renderDetail = () => {
        if (slot.kind === "none") return null;
        if (slot.kind === "eraser_toggle") {
            return (
                <span className={styles.bindingDetailEmpty}>
                    Toggles eraser tool while pressed
                </span>
            );
        }
        if (slot.kind === "mouse_button") {
            return (
                <div className={styles.bindingDetail}>
                    <div className={styles.mouseGroup}>
                        {[["left","Left"],["middle","Middle"],["right","Right"]].map(([v, l]) => (
                            <button
                                key={v}
                                type="button"
                                className={
                                    styles.mouseBtn + (slot.mouse === v ? " " + styles.mouseBtnOn : "")
                                }
                                onClick={() => setMouse(v)}
                            >{l}</button>
                        ))}
                    </div>
                </div>
            );
        }
        // key_tap / key_hold / key_chord
        return (
            <div className={styles.bindingDetail}>
                {MOD_ORDER.map((m) => (
                    <button
                        key={m}
                        type="button"
                        className={
                            styles.modBtn + (slot.mods.includes(m) ? " " + styles.modBtnOn : "")
                        }
                        onClick={() => toggleMod(m)}
                    >{m}</button>
                ))}
                <span
                    ref={captureRef}
                    tabIndex={0}
                    className={
                        styles.keyCapture
                        + (capturing ? " " + styles.keyCaptureCapturing : "")
                        + (!slot.key && !capturing ? " " + styles.keyCaptureEmpty : "")
                    }
                    onClick={() => {
                        setCapturing(true);
                        setTimeout(() => captureRef.current?.focus(), 0);
                    }}
                    onKeyDown={onKeyDown}
                    onBlur={() => setCapturing(false)}
                >
                    {capturing ? "press a key…" : (slot.key || "press a key")}
                </span>
                <button
                    type="button"
                    className={styles.modBtn}
                    onClick={clear}
                    title="Clear"
                >✕</button>
            </div>
        );
    };

    return (
        <div className={styles.bindingRow}>
            <div className={styles.bindingHeader}>
                <span className={styles.bindingHeaderLabel}>{label}</span>
                <div className={styles.bindingHeaderDropdown}>
                    <Dropdown
                        value={kindLabel(slot.kind)}
                        selectedOptions={[slot.kind]}
                        onOptionSelect={(_, d) => setKind(d.optionValue)}
                    >
                        <Option value="none">Disabled</Option>
                        <Option value="key_tap">Tap key</Option>
                        <Option value="key_hold">Hold key</Option>
                        <Option value="mouse_button">Mouse button</Option>
                        <Option value="eraser_toggle">Eraser toggle</Option>
                    </Dropdown>
                </div>
            </div>
            {renderDetail()}
        </div>
    );
}

function kindLabel(k) {
    return ({
        none: "Disabled",
        key_tap: "Tap key",
        key_hold: "Hold key",
        mouse_button: "Mouse button",
        eraser_toggle: "Eraser toggle",
    })[k] || k;
}

export default function App() {
    const styles = useStyles();
    const [status, setStatus] = useState({ state: "stopped" });
    const [settings, setSettings] = useState(null);
    const [slots, setSlots] = useState([
        { kind: "key_hold", mods: ["Ctrl"], key: "", mouse: "left" },
        { kind: "key_hold", mods: ["Shift"], key: "", mouse: "left" },
        { kind: "key_tap",  mods: [],       key: "E", mouse: "left" },
    ]);
    const [saveMsg, setSaveMsg] = useState("");
    const [elevated, setElevated] = useState(false);
    const [vddInstalled, setVddInstalled] = useState(true);
    const [vddInstalling, setVddInstalling] = useState(false);
    const [vddInstallError, setVddInstallError] = useState("");

    // Initial load.
    useEffect(() => {
        (async () => {
            const s = await invoke("get_settings");
            setSettings(s);
            setSlots([
                bindingToState(s.bindings.button_0),
                bindingToState(s.bindings.button_1),
                bindingToState(s.bindings.button_2),
            ]);
            try { setElevated(await invoke("is_elevated")); } catch {}
            try { setStatus(await invoke("get_status")); } catch {}
            try { setVddInstalled(await invoke("is_vdd_installed")); } catch {}
        })();

        const unlistenP = listen("service-state", (ev) => setStatus(ev.payload));
        return () => { unlistenP.then((fn) => fn()).catch(() => {}); };
    }, []);

    const onInstallVdd = useCallback(async () => {
        setVddInstalling(true);
        setVddInstallError("");
        try {
            await invoke("install_vdd");
            setVddInstalled(await invoke("is_vdd_installed"));
        } catch (e) {
            setVddInstallError(String(e));
        } finally {
            setVddInstalling(false);
        }
    }, []);

    const onSave = useCallback(async () => {
        if (!settings) return;
        const next = {
            ...settings,
            bindings: {
                button_0: stateToBinding(slots[0]),
                button_1: stateToBinding(slots[1]),
                button_2: stateToBinding(slots[2]),
            },
        };
        setSaveMsg("saving…");
        try {
            await invoke("save_settings", { new: next });
            setSaveMsg("saved");
            if (next.run_as_admin) {
                const e = await invoke("is_elevated");
                if (!e) {
                    setSaveMsg("relaunching as administrator…");
                    await invoke("relaunch_as_admin");
                }
            }
        } catch (e) {
            setSaveMsg("error: " + e);
        }
        setTimeout(() => setSaveMsg(""), 3000);
    }, [settings, slots]);

    const onToggle = useCallback(async () => {
        const cur = await invoke("get_status");
        if (cur.state === "stopped") await invoke("start_service");
        else                          await invoke("stop_service");
    }, []);

    if (!settings) {
        return (
            <div className={styles.root}>
                <Text>Loading…</Text>
            </div>
        );
    }

    const badge = statusBadge(status);
    const isPaused = status.state === "stopped";
    const vddResolution = settings.vdd_resolution ?? DEFAULT_RESOLUTION;
    const selectedResolution = resolutionKey(vddResolution);
    const setVddResolution = (next) => {
        setSettings({ ...settings, vdd_resolution: next });
    };

    return (
        <div className={styles.root}>
            <header className={styles.header}>
                <Title3 className={styles.title}>Penflow</Title3>
                <span className={styles.statusDetail}>{statusDescription(status)}</span>
                <Badge appearance="filled" color={badge.color}>{badge.text}</Badge>
            </header>

            {!vddInstalled && (
                <MessageBar intent={vddInstallError ? "error" : "warning"}>
                    <MessageBarBody>
                        <MessageBarTitle>
                            {vddInstallError ? "VDD install failed" : "Virtual Display Driver not installed"}
                        </MessageBarTitle>
                        {vddInstallError
                            ? vddInstallError
                            : "Penflow needs the bundled Virtual Display Driver to capture an extended monitor instead of mirroring your desktop. Click Install — UAC will prompt once."}
                    </MessageBarBody>
                    <MessageBarActions>
                        {vddInstalling
                            ? <Spinner size="tiny" label="Installing…" />
                            : <Button appearance="primary" onClick={onInstallVdd}>
                                Install VDD
                              </Button>}
                    </MessageBarActions>
                </MessageBar>
            )}

            <section className={styles.card}>
                <div className={styles.rowGroup}>
                    <div className={styles.column}>
                        <Subtitle2 className={styles.cardTitle}>Encoder</Subtitle2>
                        <Field label="Bitrate (Mbps)" orientation="horizontal" className={styles.row}>
                            <SpinButton
                                value={Math.round(settings.bitrate_bps / 1_000_000)}
                                min={5}
                                max={500}
                                step={5}
                                onChange={(_, d) => {
                                    const v = d.value ?? Number(d.displayValue);
                                    if (Number.isFinite(v)) {
                                        setSettings({ ...settings, bitrate_bps: v * 1_000_000 });
                                    }
                                }}
                            />
                        </Field>
                        <Field label="Frame rate" orientation="horizontal" className={styles.row}>
                            <Dropdown
                                value={`${settings.fps} fps`}
                                selectedOptions={[String(settings.fps)]}
                                onOptionSelect={(_, d) => setSettings({ ...settings, fps: Number(d.optionValue) })}
                            >
                                <Option value="60">60 fps</Option>
                                <Option value="90">90 fps</Option>
                                <Option value="120">120 fps</Option>
                            </Dropdown>
                        </Field>
                        {(settings.topology ?? "extend") === "extend" && (
                        <Field label="Virtual display" className={styles.resolutionField}>
                            <div className={styles.resolutionControl}>
                                <Dropdown
                                    className={styles.resolutionDropdown}
                                    value={resolutionLabel(vddResolution)}
                                    selectedOptions={[selectedResolution]}
                                    onOptionSelect={(_, d) => {
                                        if (d.optionValue === "custom") return;
                                        const preset = RESOLUTION_PRESETS.find((p) => p.key === d.optionValue);
                                        if (preset) {
                                            setVddResolution({ width: preset.width, height: preset.height });
                                        }
                                    }}
                                >
                                    {RESOLUTION_PRESETS.map((p) => (
                                        <Option key={p.key} value={p.key}>{p.label}</Option>
                                    ))}
                                    <Option value="custom">Custom</Option>
                                </Dropdown>
                                <div className={styles.resolutionInputs}>
                                    <SpinButton
                                        aria-label="Virtual display width"
                                        className={styles.resolutionSpin}
                                        value={vddResolution.width}
                                        min={640}
                                        max={7680}
                                        step={2}
                                        onChange={(_, d) => {
                                            const value = numericSpinValue(d);
                                            if (value !== null) {
                                                setVddResolution({ ...vddResolution, width: value });
                                            }
                                        }}
                                    />
                                    <Caption1 className={styles.resolutionSeparator}>×</Caption1>
                                    <SpinButton
                                        aria-label="Virtual display height"
                                        className={styles.resolutionSpin}
                                        value={vddResolution.height}
                                        min={480}
                                        max={4320}
                                        step={2}
                                        onChange={(_, d) => {
                                            const value = numericSpinValue(d);
                                            if (value !== null) {
                                                setVddResolution({ ...vddResolution, height: value });
                                            }
                                        }}
                                    />
                                </div>
                            </div>
                        </Field>
                        )}
                        <Field label="Display mode" orientation="horizontal" className={styles.row}>
                            <Dropdown
                                value={(settings.topology ?? "extend") === "duplicate" ? "Duplicate" : "Extend"}
                                selectedOptions={[settings.topology ?? "extend"]}
                                onOptionSelect={(_, d) => setSettings({ ...settings, topology: d.optionValue })}
                            >
                                <Option value="extend">Extend (separate desktop)</Option>
                                <Option value="duplicate">Duplicate primary</Option>
                            </Dropdown>
                        </Field>
                        {(settings.topology ?? "extend") === "duplicate" && (
                            <Caption1 className={styles.hint}>
                                Streaming your primary monitor directly. The virtual-display driver and resolution settings are bypassed in this mode.
                            </Caption1>
                        )}
                        <Field label="Codec" orientation="horizontal" className={styles.row}>
                            <Dropdown
                                value={settings.codec === "hevc" ? "HEVC" : "H.264"}
                                selectedOptions={[settings.codec]}
                                onOptionSelect={(_, d) => setSettings({ ...settings, codec: d.optionValue })}
                            >
                                <Option value="hevc">HEVC</Option>
                                <Option value="h264">H.264</Option>
                            </Dropdown>
                        </Field>
                    </div>
                    <div className={styles.column}>
                        <Subtitle2 className={styles.cardTitle}>System</Subtitle2>
                        <div className={styles.row}>
                            <span className={styles.rowLabel} title="Add Penflow to your Windows logon">
                                Start with Windows
                            </span>
                            <Switch
                                checked={settings.autostart}
                                onChange={(_, d) => setSettings({ ...settings, autostart: d.checked })}
                            />
                        </div>
                        <div className={styles.row}>
                            <span className={styles.rowLabel} title="Required for injecting input into elevated apps">
                                Run as administrator
                            </span>
                            <Switch
                                checked={settings.run_as_admin}
                                onChange={(_, d) => setSettings({ ...settings, run_as_admin: d.checked })}
                            />
                        </div>
                        <div className={styles.row}>
                            <span className={styles.rowLabel} title="Show the latency HUD overlay on the tablet. Takes effect after the next reconnect.">
                                Show tablet HUD overlay
                            </span>
                            <Switch
                                checked={settings.hud_enabled !== false}
                                onChange={(_, d) => setSettings({ ...settings, hud_enabled: d.checked })}
                            />
                        </div>
                        <Caption1 style={{ color: tokens.colorNeutralForeground4 }}>
                            {elevated ? "Currently running as administrator" : "Currently running unelevated"}
                        </Caption1>
                    </div>
                </div>
            </section>

            <section className={styles.card}>
                <Subtitle2 className={styles.cardTitle}>Pen buttons</Subtitle2>
                <Caption1 className={styles.hint}>
                    Click the key field then press the combination you want to bind.
                </Caption1>
                {[
                    { label: "Barrel button 1", idx: 0 },
                    { label: "Barrel button 2", idx: 1 },
                    { label: "Tertiary",        idx: 2 },
                ].map(({ label, idx }) => (
                    <BindingRow
                        key={idx}
                        label={label}
                        slot={slots[idx]}
                        onChange={(next) => {
                            const ns = [...slots];
                            ns[idx] = next;
                            setSlots(ns);
                        }}
                        styles={styles}
                    />
                ))}
            </section>

            <footer className={styles.footer}>
                <span className={styles.saveStatus}>{saveMsg}</span>
                <Button onClick={onToggle}>{isPaused ? "Resume" : "Pause"}</Button>
                <Button appearance="primary" onClick={onSave}>Save</Button>
            </footer>
        </div>
    );
}
