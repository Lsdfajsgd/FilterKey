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

// ───────── 타이머 트리거용 패시브 키보드 훅 (키를 삼키지 않고 감지만) ─────────
// 스킬키를 누르면 그 입력은 게임으로 그대로 전달되고, 동시에 해당 프리셋의 타이머만 시작.

const WH_KEYBOARD_LL: i32 = 13;

#[repr(C)]
#[allow(non_snake_case)]
struct KBDLLHOOKSTRUCT {
    vkCode: u32,
    scanCode: u32,
    flags: u32,
    time: u32,
    dwExtraInfo: usize,
}

#[link(name = "user32")]
extern "system" {
    fn SetWindowsHookExW(
        id_hook: i32,
        lpfn: unsafe extern "system" fn(i32, usize, isize) -> isize,
        hmod: isize,
        thread_id: u32,
    ) -> isize;
    fn CallNextHookEx(hhk: isize, code: i32, wparam: usize, lparam: isize) -> isize;
    fn GetAsyncKeyState(vk: i32) -> i16;
}

// ──────────── 한자키 제거 (Scancode Map — OS 커널 차원, 재부팅 필요) ────────────
// 우측 Ctrl(E0 1D)을 왼쪽 Ctrl(1D)로 리매핑: 드라이버가 한자로 번역하기 전 단계라
// Ctrl 기능은 그대로 살아있고 한자 기능만 사라진다. 훅/상주 프로세스 불필요.

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(hkey: isize, sub_key: *const u16, options: u32, sam: u32, result: *mut isize) -> i32;
    fn RegQueryValueExW(hkey: isize, name: *const u16, reserved: *mut u32, typ: *mut u32, data: *mut u8, len: *mut u32) -> i32;
    fn RegSetValueExW(hkey: isize, name: *const u16, reserved: u32, typ: u32, data: *const u8, len: u32) -> i32;
    fn RegDeleteValueW(hkey: isize, name: *const u16) -> i32;
    fn RegCloseKey(hkey: isize) -> i32;
}

const HKLM: isize = 0x80000002u32 as i32 as isize;
const KEY_READ: u32 = 0x20019;
const KEY_SET_VALUE: u32 = 0x0002;
const REG_BINARY_TYPE: u32 = 3;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_ACCESS_DENIED: i32 = 5;
const SCANCODE_KEY: &str = "SYSTEM\\CurrentControlSet\\Control\\Keyboard Layout";
const SCANCODE_VALUE: &str = "Scancode Map";

/// 우리가 추가/제거하는 매핑: (물리 키 스캔코드, 새로 출력할 스캔코드)
/// - 0xE01D(우측 Ctrl) → 0x001D(왼쪽 Ctrl): 한국어 101키 종류1 배열에서 우측 Ctrl이 한자로 동작하는 것을 순수 Ctrl로 변경
/// - 0x0071(전용 한자키) → 0x0000: 전용 한자키가 있는 103/106키 배열에서 한자키 비활성화
const OUR_SCANCODE_ENTRIES: [(u16, u16); 2] = [(0xE01D, 0x001D), (0x0071, 0x0000)];

fn open_scancode_key(sam: u32) -> Result<isize, String> {
    let path = to_wide(SCANCODE_KEY);
    let mut hkey: isize = 0;
    let r = unsafe { RegOpenKeyExW(HKLM, path.as_ptr(), 0, sam, &mut hkey) };
    if r != 0 {
        return Err(format!("레지스트리 키 열기 실패 (코드 {r})"));
    }
    Ok(hkey)
}

/// Scancode Map 값을 (원본, 새값) 목록으로 파싱. 값이 없으면 빈 목록.
fn read_scancode_entries() -> Result<Vec<(u16, u16)>, String> {
    let hkey = open_scancode_key(KEY_READ)?;
    let name = to_wide(SCANCODE_VALUE);
    let mut len: u32 = 0;
    let r = unsafe {
        RegQueryValueExW(hkey, name.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut len)
    };
    if r == ERROR_FILE_NOT_FOUND {
        unsafe { RegCloseKey(hkey) };
        return Ok(Vec::new());
    }
    if r != 0 {
        unsafe { RegCloseKey(hkey) };
        return Err(format!("레지스트리 값 조회 실패 (코드 {r})"));
    }
    let mut buf = vec![0u8; len as usize];
    let r = unsafe {
        RegQueryValueExW(hkey, name.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), buf.as_mut_ptr(), &mut len)
    };
    unsafe { RegCloseKey(hkey) };
    if r != 0 {
        return Err(format!("레지스트리 값 읽기 실패 (코드 {r})"));
    }
    // 형식: 8바이트 헤더(0) + DWORD 개수 + 매핑 DWORD들(low=새값, high=원본) + 종결자(0)
    let mut entries = Vec::new();
    if buf.len() >= 12 {
        let mut off = 12;
        while off + 4 <= buf.len() {
            let new = u16::from_le_bytes([buf[off], buf[off + 1]]);
            let orig = u16::from_le_bytes([buf[off + 2], buf[off + 3]]);
            if new == 0 && orig == 0 {
                break; // 종결자
            }
            entries.push((orig, new));
            off += 4;
        }
    }
    Ok(entries)
}

fn write_scancode_entries(entries: &[(u16, u16)]) -> Result<(), String> {
    let hkey = open_scancode_key(KEY_SET_VALUE)?;
    let name = to_wide(SCANCODE_VALUE);
    let r = if entries.is_empty() {
        let r = unsafe { RegDeleteValueW(hkey, name.as_ptr()) };
        if r == ERROR_FILE_NOT_FOUND { 0 } else { r }
    } else {
        let mut blob = vec![0u8; 8]; // version + flags
        blob.extend(((entries.len() as u32) + 1).to_le_bytes());
        for (orig, new) in entries {
            blob.extend(new.to_le_bytes());
            blob.extend(orig.to_le_bytes());
        }
        blob.extend([0u8; 4]); // 종결자
        unsafe { RegSetValueExW(hkey, name.as_ptr(), 0, REG_BINARY_TYPE, blob.as_ptr(), blob.len() as u32) }
    };
    unsafe { RegCloseKey(hkey) };
    if r == ERROR_ACCESS_DENIED {
        return Err("관리자 권한이 필요합니다".into());
    }
    if r != 0 {
        return Err(format!("레지스트리 쓰기 실패 (코드 {r})"));
    }
    Ok(())
}

/// 현재 Scancode Map에 우리의 우측 Ctrl 리매핑이 등록되어 있는지
#[tauri::command]
fn get_hanja_removal() -> bool {
    read_scancode_entries()
        .map(|v| v.iter().any(|e| e.0 == 0xE01D && e.1 == 0x001D))
        .unwrap_or(false)
}

/// 한자키 제거 등록/해제 — 기존 Scancode Map의 다른 매핑은 보존(병합)
#[tauri::command]
fn set_hanja_removal(enable: bool) -> Result<(), String> {
    let mut entries = read_scancode_entries()?;
    if enable {
        for (orig, new) in OUR_SCANCODE_ENTRIES {
            if !entries.iter().any(|e| e.0 == orig) {
                entries.push((orig, new));
            }
        }
    } else {
        entries.retain(|e| !OUR_SCANCODE_ENTRIES.iter().any(|o| o.0 == e.0));
    }
    write_scancode_entries(&entries)
}

// ─────────────────── 관리자 권한 (게임이 관리자로 실행될 때 필요) ───────────────────

#[link(name = "shell32")]
extern "system" {
    fn IsUserAnAdmin() -> i32;
    fn ShellExecuteW(
        hwnd: isize,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show_cmd: i32,
    ) -> isize;
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[tauri::command]
fn is_admin() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

#[tauri::command]
fn restart_as_admin(app: AppHandle) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_w = to_wide(&exe.to_string_lossy());
    let verb = to_wide("runas");
    let r = unsafe {
        ShellExecuteW(
            0,
            verb.as_ptr(),
            exe_w.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    if r <= 32 {
        return Err("관리자 권한 실행이 취소되었거나 실패했습니다".into());
    }
    app.exit(0);
    Ok(())
}

// ─────────────────────────────── 데이터 모델 ───────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
struct Preset {
    id: String,
    name: String,
    wait_ms: u32,
    delay_ms: u32,
    repeat_ms: u32,
    bounce_ms: u32,
    hotkey: Option<String>,
    timer_seconds: u32,          // 알림음까지 시간(초). 0이면 타이머 사용 안 함
    timer_hotkey: Option<String>, // 타이머를 시작하는 전역 단축키
}

impl Default for Preset {
    fn default() -> Self {
        Preset {
            id: String::new(),
            name: String::new(),
            wait_ms: 0,
            delay_ms: 0,
            repeat_ms: 0,
            bounce_ms: 0,
            hotkey: None,
            timer_seconds: 0,
            timer_hotkey: None,
        }
    }
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
                    ..Default::default()
                },
                Preset {
                    id: "boss".into(),
                    name: "보스".into(),
                    wait_ms: 0,
                    delay_ms: 100,
                    repeat_ms: 10,
                    bounce_ms: 0,
                    hotkey: Some("ctrl+alt+f11".into()),
                    ..Default::default()
                },
                Preset {
                    id: "windefault".into(),
                    name: "윈도우 기본값".into(),
                    wait_ms: 1000,
                    delay_ms: 1000,
                    repeat_ms: 500,
                    bounce_ms: 0,
                    hotkey: None,
                    ..Default::default()
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

// ─────────────────────────────── 타이머 알림 ───────────────────────────────
// 프리셋별 타이머: 단축키(또는 버튼)를 누른 시점부터 지정 시간이 지나면 알림음.
// 새 타이머를 시작하면 이전 타이머는 취소된다(세대 번호 비교).

use std::sync::atomic::{AtomicU64, Ordering};
static TIMER_GEN: AtomicU64 = AtomicU64::new(0);

/// 지정한 초가 지난 뒤 알림음(3단 비프)을 울린다. 0초면 아무것도 안 함.
fn start_timer_impl(app: &AppHandle, seconds: u32) {
    if seconds == 0 {
        return;
    }
    let generation = TIMER_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(seconds as u64));
        // 그 사이 다른 타이머가 시작됐으면 이 타이머는 무효
        if TIMER_GEN.load(Ordering::SeqCst) != generation {
            return;
        }
        // "띠띠" — 짧은 알림음 두 번
        unsafe {
            Beep(1760, 90);
            std::thread::sleep(std::time::Duration::from_millis(70));
            Beep(1760, 90);
        }
        let _ = handle.emit("timer-done", ());
    });
}

// ── 타이머 트리거 바인딩 (패시브 키보드 훅이 이 목록을 보고 타이머를 시작) ──

#[derive(Clone)]
enum MainKey {
    Exact(u32), // 특정 가상키 코드
    Shift,
    Ctrl,
    Alt,
    Meta,
}

#[derive(Clone)]
struct TimerBinding {
    seconds: u32,
    main: MainKey,
    need_ctrl: bool,
    need_alt: bool,
    need_shift: bool,
}

use std::sync::OnceLock;
static TIMER_APP: OnceLock<AppHandle> = OnceLock::new();
static TIMER_BINDINGS: Mutex<Vec<TimerBinding>> = Mutex::new(Vec::new());
static TIMER_DOWN: Mutex<Vec<u32>> = Mutex::new(Vec::new()); // 현재 눌린 키(오토리핏 방지용)

/// 키 토큰("a","f1","space","capslock","comma"...) → 윈도우 가상키 코드
fn token_to_vk(t: &str) -> Option<u32> {
    // 알파벳
    if t.len() == 1 {
        let c = t.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return Some(c.to_ascii_uppercase() as u32); // 'A'=0x41
        }
        if c.is_ascii_digit() {
            return Some(c as u32); // '0'=0x30
        }
    }
    // F1~F24
    if let Some(n) = t.strip_prefix('f') {
        if let Ok(num) = n.parse::<u32>() {
            if (1..=24).contains(&num) {
                return Some(0x70 + (num - 1)); // VK_F1=0x70
            }
        }
    }
    let vk = match t {
        "space" => 0x20,
        "enter" => 0x0D,
        "tab" => 0x09,
        "capslock" => 0x14,
        "backspace" => 0x08,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "minus" => 0xBD,
        "equal" => 0xBB,
        "comma" => 0xBC,
        "period" => 0xBE,
        "slash" => 0xBF,
        "semicolon" => 0xBA,
        "quote" => 0xDE,
        "backquote" => 0xC0,
        "bracketleft" => 0xDB,
        "bracketright" => 0xDD,
        "backslash" => 0xDC,
        _ => return None,
    };
    Some(vk)
}

/// "shift", "ctrl+a", "ctrl+shift+f1" → (메인키, ctrl필요, alt필요, shift필요)
fn parse_timer_key(s: &str) -> Option<(MainKey, bool, bool, bool)> {
    let tokens: Vec<String> = s
        .split('+')
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let (mods, main_tok) = tokens.split_at(tokens.len() - 1);
    let (mut need_ctrl, mut need_alt, mut need_shift) = (false, false, false);
    for m in mods {
        match m.as_str() {
            "ctrl" => need_ctrl = true,
            "alt" => need_alt = true,
            "shift" => need_shift = true,
            "super" => {}
            _ => return None,
        }
    }
    let main = match main_tok[0].as_str() {
        "shift" => MainKey::Shift,
        "ctrl" => MainKey::Ctrl,
        "alt" => MainKey::Alt,
        "super" => MainKey::Meta,
        other => MainKey::Exact(token_to_vk(other)?),
    };
    Some((main, need_ctrl, need_alt, need_shift))
}

fn rebuild_timer_bindings(cfg: &AppConfig) {
    let mut v = Vec::new();
    for p in &cfg.presets {
        if p.timer_seconds == 0 {
            continue;
        }
        if let Some(hk) = &p.timer_hotkey {
            if let Some((main, c, a, s)) = parse_timer_key(hk) {
                v.push(TimerBinding {
                    seconds: p.timer_seconds,
                    main,
                    need_ctrl: c,
                    need_alt: a,
                    need_shift: s,
                });
            }
        }
    }
    *TIMER_BINDINGS.lock().unwrap() = v;
}

fn key_down(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}

fn main_matches(m: &MainKey, vk: u32) -> bool {
    match m {
        MainKey::Exact(x) => *x == vk,
        MainKey::Shift => vk == 0xA0 || vk == 0xA1 || vk == 0x10,
        MainKey::Ctrl => vk == 0xA2 || vk == 0xA3 || vk == 0x11,
        MainKey::Alt => vk == 0xA4 || vk == 0xA5 || vk == 0x12,
        MainKey::Meta => vk == 0x5B || vk == 0x5C,
    }
}

/// 눌린 키가 어떤 타이머 바인딩과 맞으면 그 타이머를 시작한다.
fn maybe_trigger_timer(vk: u32) {
    let bindings = TIMER_BINDINGS.lock().unwrap().clone();
    for b in &bindings {
        if !main_matches(&b.main, vk) {
            continue;
        }
        // 필요한 수식키가 눌려 있는지 (VK_CONTROL=0x11, VK_MENU=0x12, VK_SHIFT=0x10)
        if b.need_ctrl && !key_down(0x11) {
            continue;
        }
        if b.need_alt && !key_down(0x12) {
            continue;
        }
        if b.need_shift && !key_down(0x10) {
            continue;
        }
        if let Some(app) = TIMER_APP.get() {
            start_timer_impl(app, b.seconds);
            beep_feedback(app, 740);
        }
        break;
    }
}

/// 패시브 훅: 키를 절대 삼키지 않고(항상 CallNextHookEx), 눌림만 감지한다.
unsafe extern "system" fn timer_hook_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code >= 0 {
        let msg = wparam as u32;
        let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
        let vk = kb.vkCode;
        const WM_KEYDOWN: u32 = 0x0100;
        const WM_SYSKEYDOWN: u32 = 0x0104;
        const WM_KEYUP: u32 = 0x0101;
        const WM_SYSKEYUP: u32 = 0x0105;
        if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
            // 오토리핏 무시: 이미 눌린 상태면 무시하고, 처음 눌릴 때만 트리거
            let fresh = {
                let mut down = TIMER_DOWN.lock().unwrap();
                if down.contains(&vk) {
                    false
                } else {
                    down.push(vk);
                    true
                }
            };
            if fresh {
                maybe_trigger_timer(vk);
            }
        } else if msg == WM_KEYUP || msg == WM_SYSKEYUP {
            TIMER_DOWN.lock().unwrap().retain(|&x| x != vk);
        }
    }
    CallNextHookEx(0, code, wparam, lparam)
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
        // 타이머 단축키는 전역 단축키(키를 삼킴)가 아니라 패시브 훅으로 처리하므로 여기서 등록하지 않는다.
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

// ─────────────────────────────── 자동 업데이트 ───────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    version: String,
    notes: Option<String>,
}

#[tauri::command]
async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(u)) => Ok(Some(UpdateInfo {
            version: u.version.clone(),
            notes: u.body.clone(),
        })),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "설치할 업데이트가 없습니다".to_string())?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart();
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

/// 프리셋의 타이머 시작. 반환값은 설정된 초(0이면 미설정).
#[tauri::command]
fn start_timer(app: AppHandle, id: String) -> Result<u32, String> {
    let seconds = {
        let state = app.state::<ConfigState>();
        let cfg = state.lock().unwrap();
        cfg.presets
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.timer_seconds)
            .ok_or_else(|| "프리셋을 찾을 수 없습니다".to_string())?
    };
    if seconds == 0 {
        return Err("타이머 시간이 설정되지 않았습니다".into());
    }
    start_timer_impl(&app, seconds);
    Ok(seconds)
}

/// 실행 중인 타이머를 취소한다.
#[tauri::command]
fn cancel_timer() {
    TIMER_GEN.fetch_add(1, Ordering::SeqCst);
}

#[tauri::command]
fn save_config(
    app: AppHandle,
    state: State<ConfigState>,
    config: AppConfig,
) -> Result<Vec<String>, String> {
    let failed = sync_hotkeys(&app, &config);
    rebuild_timer_bindings(&config);
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
        .plugin(tauri_plugin_updater::Builder::new().build())
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

            // 타이머용 패시브 키보드 훅 설치 (키를 삼키지 않고 감지만)
            let _ = TIMER_APP.set(handle.clone());
            rebuild_timer_bindings(&cfg);
            unsafe {
                SetWindowsHookExW(WH_KEYBOARD_LL, timer_hook_proc, 0, 0);
            }

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
            save_config,
            check_update,
            install_update,
            is_admin,
            restart_as_admin,
            get_hanja_removal,
            set_hanja_removal,
            start_timer,
            cancel_timer
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
