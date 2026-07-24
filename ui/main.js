// 메이플헬퍼 — Solaar 스타일 리스트/상세 UI
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

let config = null;              // AppConfig (camelCase)
let sys = null;                 // SysState
let sel = 'custom';             // 'custom' | presetId | 'settings'
let isAdmin = false;            // 관리자 권한 여부
let hanjaPerm = false;          // Scancode Map 한자키 제거 등록 여부
const draft = { wait: 0, delay: 0, repeat: 0, bounce: 0 }; // 편집 중 값 (ms)
const FIELDS = ['wait', 'delay', 'repeat', 'bounce'];

// ─────────────────────────── 유틸 ───────────────────────────

function fmtValue(ms) {
  if (config && config.unit === 's') {
    const s = ms / 1000;
    return (Number.isInteger(s) ? s : parseFloat(s.toFixed(3))) + '초';
  }
  return ms + 'ms';
}

function msToUnit(ms) {
  return config.unit === 's' ? parseFloat((ms / 1000).toFixed(3)) : ms;
}

// 윈도우가 허용하는 하드 한계 (실측: 20001ms부터 SPI_SETFILTERKEYS 거부)
const MAX_MS = 20000;

function unitToMs(v) {
  const n = parseFloat(v);
  if (isNaN(n) || n < 0) return 0;
  return Math.min(MAX_MS, Math.round(config.unit === 's' ? n * 1000 : n));
}

function prettyHotkey(hk) {
  if (!hk) return null;
  return hk
    .split('+')
    .map((t) => {
      const u = t.trim();
      const low = u.toLowerCase();
      if (/^f\d+$/i.test(u)) return u.toUpperCase();
      if (low === 'ctrl') return 'Ctrl';
      if (low === 'alt') return 'Alt';
      if (low === 'shift') return 'Shift';
      if (low === 'super') return 'Win';
      if (KEY_DISPLAY[low]) return KEY_DISPLAY[low];
      return u.length === 1 ? u.toUpperCase() : u;
    })
    .join('+');
}

let toastTimer = null;
function toast(msg, ms = 2400) {
  const el = $('toast');
  el.textContent = msg;
  el.classList.remove('hidden');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.add('hidden'), ms);
}

function selPreset() {
  return config.presets.find((p) => p.id === sel) || null;
}

// ─────────────────────────── 렌더링 ───────────────────────────

function renderList() {
  const list = $('item-list');
  list.innerHTML = '';

  const mk = (id, name, meta, metaOn) => {
    const it = document.createElement('div');
    it.className = 'litem' + (sel === id ? ' sel' : '');
    const nm = document.createElement('span');
    nm.className = 'nm';
    nm.textContent = name;
    it.appendChild(nm);
    if (meta) {
      const m = document.createElement('span');
      m.className = 'meta' + (metaOn ? ' on' : '');
      m.textContent = meta;
      it.appendChild(m);
    }
    it.onclick = () => select(id);
    list.appendChild(it);
    return it;
  };

  mk('custom', '현재 값 (커스텀)', config.activePreset ? '' : (sys && sys.on ? '적용 중' : ''), true);

  const sep = document.createElement('div');
  sep.className = 'list-sep';
  list.appendChild(sep);

  config.presets.forEach((p) => {
    const isActive = config.activePreset === p.id;
    mk(p.id, p.name, isActive ? '적용 중' : (prettyHotkey(p.hotkey) || ''), isActive);
  });
}

function fillEditor() {
  FIELDS.forEach((f) => {
    $('in-' + f).value = msToUnit(draft[f]);
    $('sl-' + f).value = draft[f];
  });
  const suffix = config.unit === 's' ? '초' : 'ms';
  document.querySelectorAll('.unit-suffix').forEach((el) => (el.textContent = suffix));
  const step = config.unit === 's' ? 0.01 : 5;
  FIELDS.forEach((f) => ($('in-' + f).step = step));
}

function renderDetail() {
  const title = $('detail-title');
  const p = selPreset();
  const isSettings = sel === 'settings';
  const isCustom = sel === 'custom';

  $('view-values').classList.toggle('hidden', isSettings);
  $('view-settings').classList.toggle('hidden', !isSettings);
  $('hotkey-row').classList.toggle('hidden', !p);
  $('timer-row').classList.toggle('hidden', !p);
  $('timer-hotkey-row').classList.toggle('hidden', !p);
  $('btn-delete').classList.toggle('hidden', !p);
  $('btn-save').classList.toggle('hidden', !p);
  $('btn-apply').classList.toggle('hidden', isSettings);

  if (isSettings) {
    title.value = '설정';
    title.readOnly = true;
    $('detail-sub').textContent = '전역 단축키 · 동작 옵션';
    $('unit-ms').classList.toggle('active', config.unit === 'ms');
    $('unit-s').classList.toggle('active', config.unit === 's');
    $('opt-persist').checked = config.persistRegistry;
    $('opt-beep').checked = config.beepOnHotkey;
    $('opt-hanja-perm').checked = hanjaPerm;
    const btn = $('toggle-hotkey-btn');
    btn.textContent = config.toggleHotkey ? prettyHotkey(config.toggleHotkey) : '지정 안 됨';
    btn.classList.toggle('set', !!config.toggleHotkey);
  } else if (isCustom) {
    title.value = '현재 값 (커스텀)';
    title.readOnly = true;
    $('detail-sub').textContent = '값을 조정한 뒤 [적용]을 누르면 즉시 반영됩니다';
    fillEditor();
  } else if (p) {
    title.value = p.name;
    title.readOnly = false;
    $('detail-sub').textContent = '프리셋 · 이름을 클릭해 수정할 수 있어요';
    const chip = $('preset-hotkey-btn');
    chip.textContent = p.hotkey ? prettyHotkey(p.hotkey) : '지정 안 됨';
    chip.classList.toggle('set', !!p.hotkey);
    // 타이머
    $('in-timer').value = p.timerSeconds || 0;
    const tchip = $('timer-hotkey-btn');
    tchip.textContent = p.timerHotkey ? prettyHotkey(p.timerHotkey) : '지정 안 됨';
    tchip.classList.toggle('set', !!p.timerHotkey);
    fillEditor();
  }
}

function renderStatus() {
  if (!sys) return;
  $('power-toggle').checked = sys.on;
  $('sys-summary').textContent =
    (sys.on ? '켜짐' : '꺼짐') +
    ' · ' +
    [sys.waitMs, sys.delayMs, sys.repeatMs, sys.bounceMs].map((v) => fmtValue(v)).join(' / ');
}

function renderAll() {
  renderList();
  renderDetail();
  renderStatus();
}

// ─────────────────────────── 선택 ───────────────────────────

function select(id) {
  sel = id;
  const p = selPreset();
  if (p) {
    draft.wait = p.waitMs; draft.delay = p.delayMs;
    draft.repeat = p.repeatMs; draft.bounce = p.bounceMs;
  } else if (sel === 'custom' && sys) {
    draft.wait = sys.waitMs; draft.delay = sys.delayMs;
    draft.repeat = sys.repeatMs; draft.bounce = sys.bounceMs;
  }
  renderList();
  renderDetail();
}

// 프리셋에 draft + 이름 + 타이머 저장
function commitDraftToPreset(p) {
  p.waitMs = draft.wait;
  p.delayMs = draft.delay;
  p.repeatMs = draft.repeat;
  p.bounceMs = draft.bounce;
  const name = $('detail-title').value.trim();
  if (name) p.name = name;
  const t = parseInt($('in-timer').value, 10);
  p.timerSeconds = isNaN(t) || t < 0 ? 0 : t;
}

// ─────────────────────────── 단축키 캡처 ───────────────────────────

// e.code → 전역 단축키 플러그인이 파싱 가능한 키 이름 (특수키)
const CODE_MAP = {
  Space: 'Space', Enter: 'Enter', Tab: 'Tab', CapsLock: 'CapsLock',
  Home: 'Home', End: 'End', PageUp: 'PageUp', PageDown: 'PageDown',
  Insert: 'Insert', Delete: 'Delete',
  ArrowUp: 'Up', ArrowDown: 'Down', ArrowLeft: 'Left', ArrowRight: 'Right',
  Minus: 'Minus', Equal: 'Equal', Comma: 'Comma', Period: 'Period', Slash: 'Slash',
  Semicolon: 'Semicolon', Quote: 'Quote', Backquote: 'Backquote',
  BracketLeft: 'BracketLeft', BracketRight: 'BracketRight', Backslash: 'Backslash',
};

// 표시용 (파싱 이름 → 사람이 읽기 쉬운 기호/한글)
const KEY_DISPLAY = {
  minus: '-', equal: '=', comma: ',', period: '.', slash: '/',
  semicolon: ';', quote: "'", backquote: '`',
  bracketleft: '[', bracketright: ']', backslash: '\\',
  space: 'Space', enter: 'Enter', tab: 'Tab', capslock: 'CapsLock',
  pageup: 'PageUp', pagedown: 'PageDown',
};

let captureCallback = null;
let captureAllowSingle = false;
let capturePendingMod = null; // allowSingle 모드에서 단독 수식키 후보 (keyup 때 확정)

const MOD_CODE = {
  ShiftLeft: 'shift', ShiftRight: 'shift',
  ControlLeft: 'ctrl', ControlRight: 'ctrl',
  AltLeft: 'alt', AltRight: 'alt',
  MetaLeft: 'super', MetaRight: 'super',
};

function captureHotkey(cb, allowSingle = false) {
  captureCallback = cb;
  captureAllowSingle = allowSingle;
  capturePendingMod = null;
  $('hotkey-overlay').classList.remove('hidden');
}

function finishCapture(combo) {
  const cb = captureCallback;
  captureCallback = null;
  capturePendingMod = null;
  $('hotkey-overlay').classList.add('hidden');
  cb(combo);
}

window.addEventListener(
  'keydown',
  (e) => {
    if (!captureCallback) return;
    e.preventDefault();
    e.stopPropagation();

    if (e.key === 'Escape') {
      captureCallback = null;
      capturePendingMod = null;
      $('hotkey-overlay').classList.add('hidden');
      return;
    }
    if (e.key === 'Backspace') {
      finishCapture(null);
      toast('단축키를 해제했어요');
      return;
    }

    let key = null;
    if (/^Key([A-Z])$/.test(e.code)) key = e.code.slice(3).toLowerCase();
    else if (/^Digit(\d)$/.test(e.code)) key = e.code.slice(5);
    else if (/^F(\d{1,2})$/.test(e.code)) key = e.code.toLowerCase();
    else if (CODE_MAP[e.code]) key = CODE_MAP[e.code];

    // 일반 키가 아직 안 잡힘 → 수식키만 눌린 상태
    if (!key) {
      // allowSingle이면 단독 수식키(Shift 등)를 후보로 기억, keyup 때 확정
      if (captureAllowSingle && MOD_CODE[e.code]) {
        capturePendingMod = MOD_CODE[e.code];
      }
      return;
    }

    // 일반 키가 잡혔으니 단독 수식키 후보는 취소
    capturePendingMod = null;

    const mods = [];
    if (e.ctrlKey) mods.push('ctrl');
    if (e.altKey) mods.push('alt');
    if (e.shiftKey) mods.push('shift');
    if (e.metaKey) mods.push('super');

    // 일반 키 단독은 게임 입력과 충돌 → 기본은 막지만, allowSingle이면 허용(타이머용)
    if (mods.length === 0 && !/^f\d{1,2}$/.test(key) && !captureAllowSingle) {
      toast('일반 키 단독은 게임 입력과 충돌해요 — Ctrl/Alt/Shift 조합 또는 F키를 사용하세요');
      return;
    }

    finishCapture([...mods, key].join('+'));
  },
  true
);

// 단독 수식키(Shift/Ctrl/Alt) 확정: 눌렀다 뗄 때, 그 사이 다른 키가 없었으면 그 수식키로 등록
window.addEventListener(
  'keyup',
  (e) => {
    if (!captureCallback || !captureAllowSingle || !capturePendingMod) return;
    if (MOD_CODE[e.code] === capturePendingMod) {
      finishCapture(capturePendingMod);
    }
  },
  true
);

// ─────────────────────────── 백엔드 연동 ───────────────────────────

async function saveConfig() {
  try {
    const failed = await invoke('save_config', { config });
    if (failed && failed.length > 0) {
      toast('일부 단축키 등록 실패 (충돌?): ' + failed.map(prettyHotkey).join(', '), 3600);
    }
  } catch (e) {
    toast('설정 저장 실패: ' + e);
  }
}

async function refresh() {
  const st = await invoke('get_state');
  config = st.config;
  sys = st.sys;
  if (sel !== 'custom' && sel !== 'settings' && !selPreset()) sel = 'custom';
  renderAll();
}

// ─────────────────────────── 이벤트 바인딩 ───────────────────────────

window.addEventListener('DOMContentLoaded', async () => {
  try {
    await refresh();
    select('custom');
  } catch (e) {
    toast('초기화 실패: ' + e);
    return;
  }

  listen('state-changed', (ev) => {
    sys = ev.payload.sys;
    if (config) config.activePreset = ev.payload.activePreset ?? null;
    renderList();
    renderStatus();
  });

  listen('timer-done', () => {
    toast('⏰ 타이머 시간이 되었습니다!', 4000);
  });

  // 마스터 토글
  $('power-toggle').addEventListener('change', async () => {
    try {
      await invoke('toggle_filter');
    } catch (e) {
      toast('토글 실패: ' + e);
      renderStatus();
    }
  });

  // 슬라이더 ↔ 숫자 동기화 (draft에 기록)
  FIELDS.forEach((f) => {
    $('sl-' + f).addEventListener('input', (e) => {
      draft[f] = parseInt(e.target.value, 10) || 0;
      $('in-' + f).value = msToUnit(draft[f]);
    });
    $('in-' + f).addEventListener('input', (e) => {
      draft[f] = unitToMs(e.target.value);
      $('sl-' + f).value = draft[f];
    });
  });

  // 적용
  $('btn-apply').addEventListener('click', async () => {
    // 윈도우 제약: 키 인식 지연(wait)과 연타 무시(bounce)는 동시에 0보다 클 수 없음
    if (draft.wait > 0 && draft.bounce > 0) {
      toast('⚠ "키가 먹히기까지 시간"과 "같은 키 연타 무시 시간"은 동시에 사용할 수 없어요 — 둘 중 하나를 0으로 해주세요', 4200);
      return;
    }
    try {
      const p = selPreset();
      if (p) {
        commitDraftToPreset(p);
        await saveConfig();
        await invoke('apply_preset', { id: p.id });
        toast(`「${p.name}」 적용됨 — 필터키 ON`);
        renderList();
      } else {
        await invoke('apply_values', {
          on: true,
          waitMs: draft.wait, delayMs: draft.delay,
          repeatMs: draft.repeat, bounceMs: draft.bounce,
        });
        toast('적용 완료 — 재부팅 없이 바로 반영됐어요');
      }
    } catch (e) {
      toast('적용 실패: ' + e);
    }
  });

  // 저장 (프리셋)
  $('btn-save').addEventListener('click', () => {
    const p = selPreset();
    if (!p) return;
    commitDraftToPreset(p);
    saveConfig();
    renderList();
    toast(`「${p.name}」 저장됨`);
  });

  // 삭제 (프리셋)
  $('btn-delete').addEventListener('click', () => {
    const p = selPreset();
    if (!p) return;
    config.presets = config.presets.filter((x) => x.id !== p.id);
    if (config.activePreset === p.id) config.activePreset = null;
    saveConfig();
    select('custom');
    toast(`「${p.name}」 삭제됨`);
  });

  // 프리셋 추가 (현재 draft 값으로)
  $('btn-add-preset').addEventListener('click', () => {
    const n = config.presets.length + 1;
    const p = {
      id: 'p' + Math.random().toString(36).slice(2, 10),
      name: '프리셋 ' + n,
      waitMs: draft.wait, delayMs: draft.delay,
      repeatMs: draft.repeat, bounceMs: draft.bounce,
      hotkey: null,
      timerSeconds: 0, timerHotkey: null,
    };
    config.presets.push(p);
    saveConfig();
    select(p.id);
    const title = $('detail-title');
    title.focus();
    title.select();
  });

  // 설정 열기
  $('btn-settings').addEventListener('click', () => select('settings'));

  // 프리셋 이름 변경
  $('detail-title').addEventListener('change', () => {
    const p = selPreset();
    if (!p) return;
    const name = $('detail-title').value.trim();
    if (name) {
      p.name = name;
      saveConfig();
      renderList();
    }
  });
  $('detail-title').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') e.target.blur();
  });

  // 프리셋 단축키
  $('preset-hotkey-btn').addEventListener('click', () => {
    const p = selPreset();
    if (!p) return;
    captureHotkey((combo) => {
      p.hotkey = combo;
      saveConfig();
      renderDetail();
      renderList();
    });
  });

  // 타이머 시간 입력 → 즉시 프리셋에 저장
  $('in-timer').addEventListener('change', () => {
    const p = selPreset();
    if (!p) return;
    const t = parseInt($('in-timer').value, 10);
    p.timerSeconds = isNaN(t) || t < 0 ? 0 : t;
    saveConfig();
  });

  // 타이머 시작 단축키
  $('timer-hotkey-btn').addEventListener('click', () => {
    const p = selPreset();
    if (!p) return;
    // 타이머는 단일키/조합 모두 허용 (allowSingle = true)
    captureHotkey((combo) => {
      p.timerHotkey = combo;
      saveConfig();
      renderDetail();
    }, true);
  });

  // 타이머 지금 시작 (버튼)
  $('timer-start-btn').addEventListener('click', async () => {
    const p = selPreset();
    if (!p) return;
    const t = parseInt($('in-timer').value, 10);
    if (isNaN(t) || t <= 0) {
      toast('타이머 시간을 1초 이상으로 설정하세요');
      return;
    }
    // 최신 값 저장 후 시작
    p.timerSeconds = t;
    await saveConfig();
    try {
      await invoke('start_timer', { id: p.id });
      toast(`⏱ ${t}초 뒤 알림음이 울립니다`);
    } catch (e) {
      toast('타이머 시작 실패: ' + e);
    }
  });

  // 설정: 토글 단축키
  $('toggle-hotkey-btn').addEventListener('click', () =>
    captureHotkey((combo) => {
      config.toggleHotkey = combo;
      saveConfig();
      renderDetail();
    })
  );
  $('toggle-hotkey-clear').addEventListener('click', () => {
    config.toggleHotkey = null;
    saveConfig();
    renderDetail();
  });

  // 설정: 단위 / 영구 저장 / 비프
  $('unit-ms').addEventListener('click', () => switchUnit('ms'));
  $('unit-s').addEventListener('click', () => switchUnit('s'));
  function switchUnit(u) {
    if (config.unit === u) return;
    config.unit = u;
    saveConfig();
    renderAll();
  }
  $('opt-persist').addEventListener('change', (e) => {
    config.persistRegistry = e.target.checked;
    saveConfig();
  });
  $('opt-beep').addEventListener('change', (e) => {
    config.beepOnHotkey = e.target.checked;
    saveConfig();
  });
  // ─────────── 한자키 제거 (Scancode Map) ───────────
  $('opt-hanja-perm').addEventListener('change', async (e) => {
    const want = e.target.checked;
    try {
      await invoke('set_hanja_removal', { enable: want });
      hanjaPerm = want;
      toast(
        want
          ? '한자키 제거 등록됨 — 재부팅하면 우측 Ctrl이 순수 Ctrl로 동작해요 (한자 기능만 사라짐)'
          : '한자키 제거 해제됨 — 재부팅하면 원래대로 돌아와요',
        5000
      );
    } catch (err) {
      e.target.checked = !want;
      toast(
        '실패: ' + err + (isAdmin ? '' : ' — [관리자 권한으로 재시작] 후 다시 시도하세요'),
        5000
      );
    }
  });

  // ─────────── 관리자 권한 표시/재시작 ───────────
  (async () => {
    try {
      isAdmin = await invoke('is_admin');
      $('admin-state').textContent = isAdmin
        ? '관리자 권한으로 실행 중 ✓'
        : '일반 권한 — 한자키 제거 등록에는 관리자 권한 필요';
      if (isAdmin) $('btn-admin').classList.add('hidden');
    } catch (_) {}
    try {
      hanjaPerm = await invoke('get_hanja_removal');
      $('opt-hanja-perm').checked = hanjaPerm;
    } catch (_) {}
  })();
  $('btn-admin').addEventListener('click', async () => {
    try {
      await invoke('restart_as_admin'); // UAC 승인 시 앱이 관리자 권한으로 다시 뜸
    } catch (e) {
      toast('' + e, 3500);
    }
  });

  // 창 포커스 복귀 시 실제 시스템 값 다시 읽기
  window.addEventListener('focus', () => refresh().catch(() => {}));

  // ─────────── 자동 업데이트 확인 ───────────
  $('update-btn').addEventListener('click', async () => {
    const btn = $('update-btn');
    btn.disabled = true;
    btn.textContent = '다운로드 중…';
    try {
      await invoke('install_update'); // 설치 후 자동 재시작됨
    } catch (e) {
      toast('업데이트 실패: ' + e, 4000);
      btn.disabled = false;
      btn.textContent = '지금 업데이트';
    }
  });

  (async () => {
    try {
      const info = await invoke('check_update');
      if (info) {
        $('update-text').textContent = `🍁 새 버전 v${info.version} 사용 가능`;
        $('update-banner').classList.remove('hidden');
      }
    } catch (_) {
      // 개발 모드이거나 네트워크 오류 — 조용히 무시
    }
  })();
});
