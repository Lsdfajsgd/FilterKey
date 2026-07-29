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

#[link(name = "winmm")]
extern "system" {
    // 메모리 WAV 재생 (볼륨 조절을 위해 Beep 대신 사용)
    fn PlaySoundW(sound: *const u8, hmod: isize, flags: u32) -> i32;
}
const SND_SYNC: u32 = 0x0000;
const SND_MEMORY: u32 = 0x0004;

/// 1760Hz 짧은 톤 두 번("띠띠")을 volume(0~100)에 맞춘 진폭으로 WAV 바이트 생성
fn build_alarm_wav(volume: u32) -> Vec<u8> {
    use std::f64::consts::PI;
    let sample_rate: u32 = 44100;
    let freq = 1760.0f64;
    let vol = (volume.min(100) as f64) / 100.0;
    let amp = vol * 32767.0 * 0.95;
    let beep_samples = sample_rate * 90 / 1000; // 90ms
    let gap_samples = sample_rate * 70 / 1000; // 70ms
    let fade = (sample_rate * 3 / 1000) as f64; // 3ms 페이드로 클릭음 제거

    let mut samples: Vec<i16> = Vec::new();
    for b in 0..2 {
        for n in 0..beep_samples {
            let t = n as f64 / sample_rate as f64;
            let env = {
                let ni = n as f64;
                let rem = (beep_samples - n) as f64;
                (ni / fade).min(rem / fade).min(1.0)
            };
            let s = amp * env * (2.0 * PI * freq * t).sin();
            samples.push(s as i16);
        }
        if b == 0 {
            for _ in 0..gap_samples {
                samples.push(0);
            }
        }
    }

    let data_bytes = samples.len() * 2;
    let mut wav = Vec::with_capacity(44 + data_bytes);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

/// 알림음 재생 (볼륨 조절 가능). 별도 스레드에서 동기 재생하며 버퍼를 살려둔다.
fn play_alarm(volume: u32) {
    std::thread::spawn(move || {
        let wav = build_alarm_wav(volume);
        unsafe {
            PlaySoundW(wav.as_ptr(), 0, SND_MEMORY | SND_SYNC);
        }
    });
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

/// 관리자 권한으로 자기 자신을 다시 실행한다 (UAC 확인창).
fn spawn_elevated() -> Result<(), String> {
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
    Ok(())
}

#[tauri::command]
fn restart_as_admin(app: AppHandle) -> Result<(), String> {
    spawn_elevated()?;
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
    #[serde(default = "yes")]
    filter_on: bool, // 이 프리셋 적용 시 필터키를 켤지 (윈도우 기본값 복원용 프리셋은 false)
    timer_enabled: bool,          // 이 프리셋의 타이머 사용 여부
    timer_seconds: u32,           // 알림음까지 시간(초)
    timer_hotkey: Option<String>, // 타이머를 시작하는 키
}

fn yes() -> bool {
    true
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
            filter_on: true,
            timer_enabled: false,
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
    alarm_volume: u32,      // 타이머 알림음 볼륨 (0~100)
    timer_start_sound: bool, // 타이머 시작 시 확인음 재생 여부
    combo_leader: Option<String>, // 조합키 시작 버튼(리더 키). 누른 동안만 뒤 키를 가로챈다
    always_admin: bool, // 시작할 때 자동으로 관리자 권한으로 재실행 (게임 내 훅 동작에 필요)
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
            alarm_volume: 85,
            timer_start_sound: false,
            combo_leader: None,
            always_admin: false,
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
                    filter_on: false, // 기본값 복원 = 필터키 끄기 (켜면 키가 씹힘)
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
    let raw = std::fs::read_to_string(config_path(app)).ok();
    let had_filter_on = raw
        .as_deref()
        .map(|s| s.contains("filterOn"))
        .unwrap_or(false);
    let mut cfg: AppConfig = raw
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // 구버전 설정 마이그레이션: filterOn 필드가 없던 시절의 "윈도우 기본값" 프리셋은
    // 필터키를 켠 채 적용되어 키가 씹혔다. 값이 그대로면 필터키 끄기로 보정한다.
    if !had_filter_on {
        for p in cfg.presets.iter_mut() {
            if p.id == "windefault" && p.wait_ms >= 1000 && p.delay_ms >= 1000 {
                p.filter_on = false;
            }
        }
    }
    dedupe_hotkeys(&mut cfg); // 중복 단축키 정리 (마지막 것만 유지)
    cfg
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

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
static TIMER_GEN: AtomicU64 = AtomicU64::new(0);
static ALARM_VOLUME: AtomicU32 = AtomicU32::new(85);
static TIMER_START_SOUND: AtomicBool = AtomicBool::new(false);

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
        // "띠띠" — 볼륨 조절된 짧은 알림음 두 번
        play_alarm(ALARM_VOLUME.load(Ordering::Relaxed));
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

// ── 조합키 시작 버튼(리더 키) ──
// 리더를 누르고 있는 동안 눌린 키는 메이플헬퍼가 가로채(게임에 전달 안 함) 프리셋을 전환한다.
// 리더에서 손을 떼면 아무것도 가로채지 않는다.
static LEADER_KEY: Mutex<Option<MainKey>> = Mutex::new(None);
static LEADER_HELD: AtomicBool = AtomicBool::new(false);
/// (조합할 키, 프리셋 id)
static LEADER_BINDINGS: Mutex<Vec<(MainKey, String)>> = Mutex::new(Vec::new());
/// 가로챈 keydown의 vk 목록 — 대응하는 keyup도 같이 삼켜 키 눌림이 남지 않게 한다
static SWALLOWED: Mutex<Vec<u32>> = Mutex::new(Vec::new());
/// 단축키 녹화 중에는 훅이 아무것도 가로채지 않는다.
/// (가로채면 리더 조합을 UI에서 입력할 수 없다)
static CAPTURE_MODE: AtomicBool = AtomicBool::new(false);

impl MainKey {
    /// Shift/Ctrl/Alt/Win 계열인지 — 이들은 게임 조작에 필수라 리더로 써도 가로채지 않는다
    fn is_modifier(&self) -> bool {
        !matches!(self, MainKey::Exact(_))
    }
}

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

/// 타이머는 "현재 적용된(active) 프리셋"의 것만 활성화된다.
/// 그래서 다른 프리셋에 걸어둔 타이머는 그 프리셋을 적용하기 전까지 동작하지 않는다.
fn rebuild_timer_bindings(cfg: &AppConfig) {
    let mut v = Vec::new();
    if let Some(active_id) = &cfg.active_preset {
        if let Some(p) = cfg.presets.iter().find(|p| &p.id == active_id) {
            if p.timer_enabled && p.timer_seconds > 0 {
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
        }
    }
    *TIMER_BINDINGS.lock().unwrap() = v;
}

/// 리더 키 설정과 "리더+X" 형태의 프리셋 단축키를 훅용 바인딩으로 만든다.
fn rebuild_leader_bindings(cfg: &AppConfig) {
    let leader = cfg
        .combo_leader
        .as_deref()
        .and_then(|s| parse_timer_key(s))
        .map(|(m, _, _, _)| m);
    let has_leader = leader.is_some();
    *LEADER_KEY.lock().unwrap() = leader;
    if !has_leader {
        LEADER_HELD.store(false, Ordering::Relaxed);
        LEADER_BINDINGS.lock().unwrap().clear();
        return;
    }

    let mut v = Vec::new();
    for p in &cfg.presets {
        if let Some(hk) = &p.hotkey {
            // "leader+X" 형태만 훅에서 처리 (나머지는 전역 단축키로 등록됨)
            if let Some(rest) = strip_leader_prefix(hk) {
                if let Some(vk) = token_to_vk(&rest.trim().to_lowercase()) {
                    v.push((MainKey::Exact(vk), p.id.clone()));
                }
            }
        }
    }
    *LEADER_BINDINGS.lock().unwrap() = v;
}

/// "leader+X" → Some("X"), 그 외 → None
fn strip_leader_prefix(hk: &str) -> Option<String> {
    let lower = hk.trim().to_lowercase();
    lower
        .strip_prefix("leader+")
        .map(|rest| rest.to_string())
        .filter(|r| !r.is_empty())
}

fn key_down(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}

/// Ctrl/Alt/Shift/Win 중 하나라도 눌려 있는지.
/// 이 상태의 입력은 Alt+Tab 같은 OS/게임 기본 조합이므로 리더로 가로채면 안 된다.
fn any_modifier_down() -> bool {
    key_down(0x11) || key_down(0x12) || key_down(0x10) || key_down(0x5B) || key_down(0x5C)
}

/// 그 자체가 수식키인 가상키인지 (가로채면 조합이 깨지므로 항상 통과시킨다)
fn is_modifier_vk(vk: u32) -> bool {
    matches!(
        vk,
        0x10 | 0x11 | 0x12 | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5 | 0x5B | 0x5C
    )
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
            if TIMER_START_SOUND.load(Ordering::Relaxed) {
                std::thread::spawn(|| unsafe {
                    Beep(740, 80);
                });
            }
        }
        break;
    }
}

/// 리더 키를 누른 동안 눌린 키를 처리한다. 가로챌지(true) 여부를 반환.
fn handle_leader_combo(vk: u32) -> bool {
    let preset_id = LEADER_BINDINGS
        .lock()
        .unwrap()
        .iter()
        .find(|(k, _)| main_matches(k, vk))
        .map(|(_, id)| id.clone());

    if let Some(id) = preset_id {
        if let Some(app) = TIMER_APP.get() {
            // 훅 콜백은 즉시 반환해야 하므로 실제 적용은 별도 스레드에서
            let handle = app.clone();
            std::thread::spawn(move || {
                if apply_preset_impl(&handle, &id).is_ok() {
                    beep_feedback(&handle, 990);
                }
            });
        }
    }
    // 리더를 누른 동안의 입력은 매칭 여부와 상관없이 게임에 전달하지 않는다
    true
}

/// 저수준 키보드 훅.
/// - 평소에는 아무것도 가로채지 않고 감지만 한다(타이머 트리거).
/// - 리더 키를 누르고 있는 동안에만 뒤따르는 키를 가로채 프리셋을 전환한다.
unsafe extern "system" fn timer_hook_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    // 단축키 녹화 중에는 전부 그대로 통과 (UI가 키를 받아야 조합을 기록할 수 있다)
    if CAPTURE_MODE.load(Ordering::Relaxed) {
        return CallNextHookEx(0, code, wparam, lparam);
    }
    if code >= 0 {
        let msg = wparam as u32;
        let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
        let vk = kb.vkCode;
        const WM_KEYDOWN: u32 = 0x0100;
        const WM_SYSKEYDOWN: u32 = 0x0104;
        const WM_KEYUP: u32 = 0x0101;
        const WM_SYSKEYUP: u32 = 0x0105;

        let leader = LEADER_KEY.lock().unwrap().clone();

        if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
            // 오토리핏 무시: 처음 눌릴 때만 처리
            let fresh = {
                let mut down = TIMER_DOWN.lock().unwrap();
                if down.contains(&vk) {
                    false
                } else {
                    down.push(vk);
                    true
                }
            };

            if let Some(lk) = &leader {
                if main_matches(lk, vk) {
                    // Alt+Tab / Ctrl+Tab 처럼 수식키와 함께 눌린 경우는 OS·게임의 기본 조합이므로
                    // 리더로 취급하지 않고 그대로 통과시킨다.
                    if any_modifier_down() {
                        return CallNextHookEx(0, code, wparam, lparam);
                    }
                    LEADER_HELD.store(true, Ordering::Relaxed);
                    // 리더가 Shift/Ctrl/Alt면 게임 조작에 필요하므로 그대로 통과시키고,
                    // 그 외 키(CapsLock, ` 등)는 가로채 원래 기능이 발동하지 않게 한다.
                    if !lk.is_modifier() {
                        SWALLOWED.lock().unwrap().push(vk);
                        return 1;
                    }
                    return CallNextHookEx(0, code, wparam, lparam);
                }
            }

            if LEADER_HELD.load(Ordering::Relaxed) {
                // 수식키 자체는 가로채지 않는다 (가로채면 Alt+Tab 등 조합이 깨진다)
                if is_modifier_vk(vk) {
                    return CallNextHookEx(0, code, wparam, lparam);
                }
                if fresh {
                    handle_leader_combo(vk);
                }
                SWALLOWED.lock().unwrap().push(vk);
                return 1; // 리더를 누른 동안의 입력은 게임에 전달하지 않음
            }

            if fresh {
                maybe_trigger_timer(vk);
            }
        } else if msg == WM_KEYUP || msg == WM_SYSKEYUP {
            TIMER_DOWN.lock().unwrap().retain(|&x| x != vk);

            if let Some(lk) = &leader {
                if main_matches(lk, vk) {
                    LEADER_HELD.store(false, Ordering::Relaxed);
                }
            }

            // 눌림을 가로챈 키는 뗌도 같이 가로채야 게임에 키가 눌린 채로 남지 않는다
            let was_swallowed = {
                let mut s = SWALLOWED.lock().unwrap();
                if let Some(pos) = s.iter().position(|&x| x == vk) {
                    s.remove(pos);
                    true
                } else {
                    false
                }
            };
            if was_swallowed {
                return 1;
            }
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
    // filter_on=false인 프리셋(예: 윈도우 기본값)은 값만 되돌리고 필터키를 끈다.
    // (기본값을 필터키 ON으로 적용하면 1초 이상 눌러야 인식되어 키가 씹힌다)
    set_sys(
        preset.filter_on,
        preset.wait_ms,
        preset.delay_ms,
        preset.repeat_ms,
        preset.bounce_ms,
        persist,
    )?;
    {
        let state = app.state::<ConfigState>();
        let cfg = state.lock().unwrap();
        persist_config_file(app, &cfg);
        rebuild_timer_bindings(&cfg); // 적용된 프리셋의 타이머만 활성화
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

/// 현재 등록되어 있는 단축키 문자열 (해제를 확실히 하기 위해 직접 추적)
static REGISTERED_HOTKEYS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 같은 조합이 여러 곳에 지정돼 있으면 **가장 마지막 것만 남기고** 앞의 것들을 비운다.
/// (사용자가 나중에 지정한 것이 이기고, 이전 설정은 자동 취소)
/// 순서: 토글 단축키 → 프리셋 적용 단축키 → 프리셋 타이머 단축키
fn dedupe_hotkeys(cfg: &mut AppConfig) {
    // 뒤에서부터 훑으며 이미 본 조합이면 비운다
    let mut seen: Vec<String> = Vec::new();
    let mut claim = |slot: &mut Option<String>| {
        if let Some(hk) = slot.clone() {
            let norm = hk.trim().to_lowercase();
            if seen.contains(&norm) {
                *slot = None; // 뒤쪽(나중)에 이미 쓰인 조합 → 앞쪽 것은 취소
            } else {
                seen.push(norm);
            }
        }
    };

    for p in cfg.presets.iter_mut().rev() {
        claim(&mut p.timer_hotkey);
        claim(&mut p.hotkey);
    }
    claim(&mut cfg.toggle_hotkey);
}

/// 설정의 모든 단축키를 다시 등록. 형식 오류로 등록 못 한 것만 반환한다.
///
/// - 이전에 등록한 단축키를 하나씩 명시적으로 해제한 뒤 재등록한다.
///   (unregister_all만 믿으면 실패 시 옛 단축키가 남아 "예전 값이 계속 먹는" 문제가 생긴다)
/// - 같은 조합이 여러 곳에 지정되면 **나중에 지정한 것이 이기고 앞의 것은 자동 해제**된다.
fn sync_hotkeys(app: &AppHandle, cfg: &AppConfig) -> Vec<String> {
    let gs = app.global_shortcut();

    // 1) 이전 등록분을 하나씩 확실히 해제
    {
        let mut prev = REGISTERED_HOTKEYS.lock().unwrap();
        for s in prev.iter() {
            if let Ok(sc) = Shortcut::from_str(s) {
                let _ = gs.unregister(sc);
            }
        }
        prev.clear();
    }
    let _ = gs.unregister_all(); // 혹시 남은 것까지 정리

    // 2) 후보 수집 (뒤에 오는 것이 우선 — 프리셋 목록 순서상 나중 항목이 이김)
    let mut candidates: Vec<String> = Vec::new();
    if let Some(hk) = &cfg.toggle_hotkey {
        candidates.push(hk.clone());
    }
    for p in &cfg.presets {
        if let Some(hk) = &p.hotkey {
            // "리더+X" 조합은 저수준 훅에서 처리하므로 전역 단축키로 등록하지 않는다
            if strip_leader_prefix(hk).is_none() {
                candidates.push(hk.clone());
            }
        }
        // 타이머 단축키도 훅에서 처리 (키를 삼키지 않고 통과시켜야 하므로)
    }

    // 3) 중복 제거 — 같은 조합은 마지막 것만 남긴다 (뒤에서부터 훑어 처음 만난 것만 유지)
    let mut seen: Vec<String> = Vec::new();
    let mut unique_rev: Vec<String> = Vec::new();
    for s in candidates.iter().rev() {
        let norm = s.trim().to_lowercase();
        if !seen.contains(&norm) {
            seen.push(norm);
            unique_rev.push(s.clone());
        }
    }

    // 4) 등록
    let mut failed: Vec<String> = Vec::new();
    let mut registered: Vec<String> = Vec::new();
    for s in unique_rev.into_iter().rev() {
        match Shortcut::from_str(&s) {
            Ok(sc) => {
                // 혹시 시스템에 남아 있을 수 있으니 등록 전에 한 번 더 해제 시도
                let _ = gs.unregister(sc);
                if gs.register(sc).is_ok() {
                    registered.push(s);
                } else {
                    failed.push(s);
                }
            }
            Err(_) => failed.push(s),
        }
    }
    *REGISTERED_HOTKEYS.lock().unwrap() = registered;
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
        rebuild_timer_bindings(&cfg); // 활성 프리셋 없음 → 타이머 해제
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

/// 볼륨 미리듣기: 지정 볼륨으로 알림음을 즉시 재생한다.
#[tauri::command]
fn test_alarm(volume: u32) {
    play_alarm(volume.min(100));
}

/// 단축키 녹화 모드 — 켜져 있는 동안 훅은 어떤 키도 가로채지 않는다.
/// (리더 키를 누른 채 조합을 입력하려면 UI가 그 키들을 받아야 한다)
#[tauri::command]
fn set_capture_mode(on: bool) {
    CAPTURE_MODE.store(on, Ordering::Relaxed);
    if !on {
        // 녹화 중 눌렸던 상태가 남지 않도록 정리
        LEADER_HELD.store(false, Ordering::Relaxed);
        TIMER_DOWN.lock().unwrap().clear();
        SWALLOWED.lock().unwrap().clear();
    }
}

#[tauri::command]
fn save_config(
    app: AppHandle,
    state: State<ConfigState>,
    mut config: AppConfig,
) -> Result<Vec<String>, String> {
    dedupe_hotkeys(&mut config); // 중복이면 마지막 것만 남기고 앞의 것은 취소
    let failed = sync_hotkeys(&app, &config);
    rebuild_timer_bindings(&config);
    rebuild_leader_bindings(&config);
    ALARM_VOLUME.store(config.alarm_volume.min(100), Ordering::Relaxed);
    TIMER_START_SOUND.store(config.timer_start_sound, Ordering::Relaxed);
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

            // "항상 관리자 권한" 옵션이 켜져 있으면 시작 시 자동으로 승격 재실행.
            // (게임이 관리자 권한이면 일반 권한 훅은 게임 입력을 볼 수 없다 — UIPI)
            if cfg.always_admin && !unsafe { IsUserAnAdmin() != 0 } {
                if spawn_elevated().is_ok() {
                    handle.exit(0);
                    return Ok(());
                }
            }
            app.manage(Mutex::new(cfg.clone()));
            sync_hotkeys(&handle, &cfg);

            // 타이머용 패시브 키보드 훅 설치 (키를 삼키지 않고 감지만)
            let _ = TIMER_APP.set(handle.clone());
            rebuild_timer_bindings(&cfg);
            rebuild_leader_bindings(&cfg);
            ALARM_VOLUME.store(cfg.alarm_volume.min(100), Ordering::Relaxed);
            TIMER_START_SOUND.store(cfg.timer_start_sound, Ordering::Relaxed);
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
            cancel_timer,
            test_alarm,
            set_capture_mode
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
