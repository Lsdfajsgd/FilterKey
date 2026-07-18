# 🍁 메이플헬퍼 (MapleHelper)

**재부팅 없이** 윈도우 필터키를 즉시 제어하는 프로그램입니다.

레지스트리를 직접 수정하면 재부팅해야 적용되지만, 메이플헬퍼는 윈도우 설정 앱이 사용하는 것과 동일한
접근성 API(`SystemParametersInfo`)를 호출하기 때문에 **적용 즉시 반영**됩니다.

![platform](https://img.shields.io/badge/Windows-10%20%7C%2011-0078d6)
![tauri](https://img.shields.io/badge/Tauri-2-24C8DB)

## 다운로드

👉 **[최신 버전 설치 파일 받기 (Releases)](https://github.com/Lsdfajsgd/FilterKey/releases/latest)**

`MapleHelper_x.x.x_x64-setup.exe`를 받아 실행하면 설치됩니다.
WebView2 런타임이 없는 PC에서는 설치 중 자동으로 함께 설치됩니다.

## 기능

- **필터키 ON/OFF** — 버튼 또는 전역 단축키로 즉시 토글 (기본 `Ctrl+Alt+F9`)
- **프리셋** — 사냥/보스 등 상황별 값 저장, 단축키로 게임 중 즉시 전환
  - 기본 제공: 사냥(`Ctrl+Alt+F10`) · 보스(`Ctrl+Alt+F11`) · 윈도우 기본값
- **4가지 필터키 값을 알기 쉬운 한국어로** 조절 (슬라이더 + 숫자 입력, ms/초 단위 전환)

  | 항목 | 레지스트리 이름 | 의미 |
  |---|---|---|
  | 키가 먹히기까지 시간 | `DelayBeforeAcceptance` | 누르고 나서 입력으로 인정되기까지 (0 권장) |
  | 연타 시작까지 대기시간 | `AutoRepeatDelay` | 꾹 눌렀을 때 연타가 시작되기까지 |
  | 연타 속도 (한 번 간격) | `AutoRepeatRate` | 연타 사이 간격 — 낮을수록 빠름 |
  | 같은 키 연타 무시 시간 | `BounceTime` | 뗐다 다시 누를 때 무시되는 시간 (0 권장) |

- **레지스트리 영구 저장 옵션** — 켜면 재부팅 후에도 유지, 끄면 이번 세션만
- **비프음 피드백** — 게임 화면에서도 단축키 적용 여부를 소리로 확인
- **트레이 상주** — 창을 닫아도 백그라운드에서 단축키 유지

## 원리

윈도우의 필터키 값은 로그온 시점에 시스템이 레지스트리에서 읽어 메모리에 캐싱합니다.
그래서 레지스트리(`HKCU\Control Panel\Accessibility\Keyboard Response`)만 고치면 재부팅 전까지 적용되지 않습니다.

메이플헬퍼는 `SystemParametersInfo(SPI_SETFILTERKEYS)`를 호출해 **메모리의 실시간 값을 직접 갱신**하므로
재부팅이 필요 없습니다. 윈도우 설정 앱과 동일한 방식입니다.

> **참고**: 이 프로그램은 윈도우 접근성 설정만 변경합니다.
> 게임 프로세스에 접근하지 않으며, 키 입력을 생성·자동화하지 않습니다.

## 개발

```bash
npm install
npm run dev     # 개발 실행
npm run build   # 릴리즈 빌드 (NSIS 설치 파일 생성)
```

- 프론트엔드: HTML/CSS/JS (프레임워크 없음)
- 백엔드: Rust + Tauri 2 (`tauri-plugin-global-shortcut`)
- Win32 API: `SystemParametersInfoW` 직접 FFI 호출
