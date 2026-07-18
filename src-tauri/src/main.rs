// FilterKey — 메이플 필터키 매니저
// SystemParametersInfo(SPI_SETFILTERKEYS)로 재부팅 없이 필터키 값을 즉시 적용한다.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::ffi::c_void;
use std::str::FromStr;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

// ─────────────────────────────── Win32 FFI ───────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_snake_case)]
struct FILTERKEYS {
    cbSize: u32,
    dwFlags: u32,
    iWaitMSec: u32,   // DelayBeforeAcceptance: 키 인식 지연
    iDelayMSec: u32,  // AutoRepeatDelay: 반복 시작 대기
    iRepeatMSec: u32, // AutoRepeatRate: 반복 간격
    iBounceMSec: u32, // BounceTime: 재입력 무시 시간
}

const SPI_GETFILTERKEYS: u32 = 0x0032;
const SPI_SETFILTERKEYS: u32 = 0x0033;
const SPIF_UPDATEINIFILE: u32 = 0x0001;
const SPIF_SENDCHANGE: u32 = 0x0002;
const FKF_FILTERKEYSON: u32 = 0x0000_0001;
const FKF_AVAILABLE: u32 = 0x0000_0002;

#[link(name = "user32")]
extern "system" {
    fn SystemParametersInfoW(
        ui_action: u32,
        ui_param: u32,
        pv_param: *mut c_void,
        f_win_ini: u32,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn Beep(freq: u32, duration: u32) -> i32;
}

// ─────────────────────────────── 데이터 모델 ───────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Preset {
    id: String,
    name: String,
    wait_ms: u32,
    delay_ms: u32,
    repeat_ms: u32,
    bounce_ms: u32,
    hotkey: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
struct AppConfig {
    unit: String,          // "ms" | "s"
    persist_registry: bool, // SPIF_UPDATEINIFILE 사용 여부 (재부팅 후에도 유지)
    beep_on_hotkey: bool,   // 단축키 동작 시 비프음
    toggle_hotkey: Option<String>,
    active_preset: Option<String>,
    presets: Vec<Preset>,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            unit: "s".into(),
            persist_registry: true,
            beep_on_hotkey: true,
            toggle_hotkey: Some("ctrl+alt+f9".into()),
            active_preset: None,
            presets: vec![
                Preset {
                    id: "hunt".into(),
                    name: "사냥".into(),
                    wait_ms: 0,
                    delay_ms: 150,
                    repeat_ms: 20,
                    bounce_ms: 0,
                    hotkey: Some("ctrl+alt+f10".into()),
                },
                Preset {
                    id: "boss".into(),
                    name: "보스".into(),
                    wait_ms: 0,
                    delay_ms: 100,
                    repeat_ms: 10,
                    bounce_ms: 0,
                    hotkey: Some("ctrl+alt+f11".into()),
                },
                Preset {
                    id: "windefault".into(),
                    name: "윈도우 기본값".into(),
                    wait_ms: 1000,
                    delay_ms: 1000,
                    repeat_ms: 500,
                    bounce_ms: 0,
                    hotkey: None,
                },
            ],
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SysState {
    on: bool,
    wait_ms: u32,
    delay_ms: u32,
    repeat_ms: u32,
    bounce_ms: u32,
}

type ConfigState = Mutex<AppConfig>;

// ─────────────────────────────── 필터키 제어 ───────────────────────────────

fn get_sys() -> Result<SysState, String> {
    let mut fk = FILTERKEYS {
        cbSize: std::mem::size_of::<FILTERKEYS>() as u32,
        dwFlags: 0,
        iWaitMSec: 0,
        iDelayMSec: 0,
        iRepeatMSec: 0,
        iBounceMSec: 0,
    };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETFILTERKEYS,
            fk.cbSize,
            &mut fk as *mut _ as *mut c_void,
            0,
        )
    };
    if ok == 0 {
        return Err("필터키 상태를 읽지 못했습니다 (SPI_GETFILTERKEYS 실패)".into());
    }
    Ok(SysState {
        on: fk.dwFlags & FKF_FILTERKEYSON != 0,
        wait_ms: fk.iWaitMSec,
        delay_ms: fk.iDelayMSec,
        repeat_ms: fk.iRepeatMSec,
        bounce_ms: fk.iBounceMSec,
    })
}

fn set_sys(
    on: bool,
    wait_ms: u32,
    delay_ms: u32,
    repeat_ms: u32,
    bounce_ms: u32,
    persist: bool,
) -> Result<(), String> {
    let mut flags = FKF_AVAILABLE;
    if on {
        flags |= FKF_FILTERKEYSON;
    }
    let mut fk = FILTERKEYS {
        cbSize: std::mem::size_of::<FILTERKEYS>() as u32,
        dwFlags: flags,
        iWaitMSec: wait_ms,
        iDelayMSec: delay_ms,
        iRepeatMSec: repeat_ms,
        iBounceMSec: bounce_ms,
    };
    let mut spif = SPIF_SENDCHANGE;
    if persist {
        spif |= SPIF_UPDATEINIFILE;
    }
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_SETFILTERKEYS,
            fk.cbSize,
            &mut fk as *mut _ as *mut c_void,
            spif,
        )
    };
    if ok == 0 {
        return Err("필터키 적용에 실패했습니다 (SPI_SETFILTERKEYS 실패)".into());
    }
    Ok(())
}

// ─────────────────────────────── 내부 헬퍼 ───────────────────────────────

fn config_path(app: &AppHandle) -> std::path::PathBuf {
    let dir = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let _ = std::fs::create_dir_all(&dir);
    dir.join("config.json")
}

fn load_config(app: &AppHandle) -> AppConfig {
    std::fs::read_to_string(config_path(app))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn persist_config_file(app: &AppHandle, cfg: &AppConfig) {
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(config_path(app), json);
    }
}

fn emit_state(app: &AppHandle) {
    let active = app
        .state::<ConfigState>()
        .lock()
        .unwrap()
        .active_preset
        .clone();
    if let Ok(sys) = get_sys() {
        let _ = app.emit(
            "state-changed",
            serde_json::json!({ "sys": sys, "activePreset": active }),
        );
    }
}

fn beep_feedback(app: &AppHandle, freq: u32) {
    let enabled = app.state::<ConfigState>().lock().unwrap().beep_on_hotkey;
    if enabled {
        std::thread::spawn(move || unsafe {
            Beep(freq, 120);
        });
    }
}

/// 필터키 ON/OFF 토글. 켤 때 활성 프리셋이 있으면 그 값으로 켠다.
fn toggle_impl(app: &AppHandle) -> Result<SysState, String> {
    let sys = get_sys()?;
    let (persist, preset) = {
        let state = app.state::<ConfigState>();
        let cfg = state.lock().unwrap();
        let p = cfg
            .active_preset
            .as_ref()
            .and_then(|id| cfg.presets.iter().find(|p| &p.id == id).cloned());
        (cfg.persist_registry, p)
    };
    if sys.on {
        // 끄기: 값은 유지하고 ON 플래그만 해제
        set_sys(false, sys.wait_ms, sys.delay_ms, sys.repeat_ms, sys.bounce_ms, persist)?;
    } else if let Some(p) = preset {
        set_sys(true, p.wait_ms, p.delay_ms, p.repeat_ms, p.bounce_ms, persist)?;
    } else {
        set_sys(true, sys.wait_ms, sys.delay_ms, sys.repeat_ms, sys.bounce_ms, persist)?;
    }
    let new_sys = get_sys()?;
    emit_state(app);
    Ok(new_sys)
}

fn apply_preset_impl(app: &AppHandle, id: &str) -> Result<SysState, String> {
    let (preset, persist) = {
        let state = app.state::<ConfigState>();
        let mut cfg = state.lock().unwrap();
        let p = cfg
            .presets
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| "프리셋을 찾을 수 없습니다".to_string())?;
        cfg.active_preset = Some(id.to_string());
        (p, cfg.persist_registry)
    };
    set_sys(true, preset.wait_ms, preset.delay_ms, preset.repeat_ms, preset.bounce_ms, persist)?;
    {
        let state = app.state::<ConfigState>();
        let cfg = state.lock().unwrap();
        persist_config_file(app, &cfg);
    }
    let sys = get_sys()?;
    emit_state(app);
    Ok(sys)
}

fn matches_shortcut(pressed: &Shortcut, config_str: &str) -> bool {
    Shortcut::from_str(config_str)
        .map(|s| s == *pressed)
        .unwrap_or(false)
}

/// 설정의 모든 단축키를 다시 등록. 실패한 단축키 문자열 목록을 반환.
fn sync_hotkeys(app: &AppHandle, cfg: &AppConfig) -> Vec<String> {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let mut failed: Vec<String> = Vec::new();

    let mut candidates: Vec<&str> = Vec::new();
    if let Some(hk) = &cfg.toggle_hotkey {
        candidates.push(hk.as_str());
    }
    for p in &cfg.presets {
        if let Some(hk) = &p.hotkey {
            candidates.push(hk.as_str());
        }
    }

    for s in candidates {
        match Shortcut::from_str(s) {
            Ok(sc) => {
                if gs.register(sc).is_err() {
                    failed.push(s.to_string());
                }
            }
            Err(_) => failed.push(s.to_string()),
        }
    }
    failed
}

// ─────────────────────────────── Tauri 커맨드 ───────────────────────────────

#[tauri::command]
fn get_state(state: State<ConfigState>) -> Result<serde_json::Value, String> {
    let cfg = state.lock().unwrap().clone();
    let sys = get_sys()?;
    Ok(serde_json::json!({ "config": cfg, "sys": sys }))
}

#[tauri::command]
fn apply_values(
    app: AppHandle,
    state: State<ConfigState>,
    on: bool,
    wait_ms: u32,
    delay_ms: u32,
    repeat_ms: u32,
    bounce_ms: u32,
) -> Result<SysState, String> {
    let persist = {
        let mut cfg = state.lock().unwrap();
        cfg.active_preset = None; // 수동 적용 → 커스텀 값
        cfg.persist_registry
    };
    set_sys(on, wait_ms, delay_ms, repeat_ms, bounce_ms, persist)?;
    let sys = get_sys()?;
    emit_state(&app);
    Ok(sys)
}

#[tauri::command]
fn toggle_filter(app: AppHandle) -> Result<SysState, String> {
    toggle_impl(&app)
}

#[tauri::command]
fn apply_preset(app: AppHandle, id: String) -> Result<SysState, String> {
    apply_preset_impl(&app, &id)
}

#[tauri::command]
fn save_config(
    app: AppHandle,
    state: State<ConfigState>,
    config: AppConfig,
) -> Result<Vec<String>, String> {
    let failed = sync_hotkeys(&app, &config);
    {
        let mut cfg = state.lock().unwrap();
        *cfg = config;
        persist_config_file(&app, &cfg);
    }
    Ok(failed)
}

// ─────────────────────────────── main ───────────────────────────────

fn main() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state != ShortcutState::Pressed {
                        return;
                    }
                    let (is_toggle, preset_id) = {
                        let state = app.state::<ConfigState>();
                        let cfg = state.lock().unwrap();
                        let is_toggle = cfg
                            .toggle_hotkey
                            .as_deref()
                            .map(|s| matches_shortcut(shortcut, s))
                            .unwrap_or(false);
                        let preset_id = cfg
                            .presets
                            .iter()
                            .find(|p| {
                                p.hotkey
                                    .as_deref()
                                    .map(|s| matches_shortcut(shortcut, s))
                                    .unwrap_or(false)
                            })
                            .map(|p| p.id.clone());
                        (is_toggle, preset_id)
                    };
                    if is_toggle {
                        if let Ok(sys) = toggle_impl(app) {
                            // 켜짐: 높은음, 꺼짐: 낮은음
                            beep_feedback(app, if sys.on { 1320 } else { 520 });
                        }
                    } else if let Some(id) = preset_id {
                        if apply_preset_impl(app, &id).is_ok() {
                            beep_feedback(app, 990);
                        }
                    }
                })
                .build(),
        )
        .setup(|app| {
            let handle = app.handle().clone();
            let cfg = load_config(&handle);
            app.manage(Mutex::new(cfg.clone()));
            sync_hotkeys(&handle, &cfg);

            // ── 트레이 아이콘 ──
            let show_i = MenuItem::with_id(app, "show", "열기", true, None::<&str>)?;
            let toggle_i =
                MenuItem::with_id(app, "toggle", "필터키 켜기/끄기", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &toggle_i, &quit_i])?;

            TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("메이플헬퍼")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "toggle" => {
                        let _ = toggle_impl(app);
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            // X 버튼 → 종료 대신 트레이로 숨김 (백그라운드 단축키 유지)
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            apply_values,
            toggle_filter,
            apply_preset,
            save_config
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
