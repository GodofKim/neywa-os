# Neywa OS - Claude Code 가이드

## ⚠️ 중요 규칙

### 바이너리 파일명 (절대 변경 금지!)
- `neywa-arm64` - Apple Silicon용
- `neywa-x86_64` - Intel Mac용

**install.sh와 discord.rs의 self_update() 함수에서 이 파일명을 사용함!**

### Rust 빌드 (반드시 이 cargo 사용!)
```bash
~/.cargo/bin/cargo build --release                              # arm64
~/.cargo/bin/cargo build --release --target x86_64-apple-darwin # x86_64
```

❌ `/usr/local/bin/cargo` 사용 금지 (x86_64 Homebrew 버전)

---

## 배포

### ★ 반드시 deploy.sh 사용할 것 (절대 수동 배포 금지!)

```bash
./deploy.sh
```

스크립트가 자동으로: 빌드 → 복사 → **바이너리 실제 버전 검증** → 배포

> **왜 수동 배포가 위험한가?**
> version.txt만 올리고 바이너리를 새로 빌드하지 않으면,
> `!update`가 실행되어도 서버에서 구버전 바이너리를 다운로드해서
> 업데이트가 됐는데도 버전이 안 바뀌는 현상이 발생한다.
> (실제로 이 실수로 0.3.21이 배포된 상태에서 version.txt만 0.4.3이 되어
>  `!update`를 해도 0.3이 계속 실행되는 문제가 있었음 - 2026-02)

### 버전 관리 규칙
- **Cargo.toml**과 **dist/pages/version.txt**의 버전은 항상 동일해야 함
- 배포할 때마다 버전 올리기 (Semantic Versioning)
  - 버그 수정: patch 올림 (0.2.0 → 0.2.1)
  - 새 기능 추가: minor 올림 (0.2.0 → 0.3.0)
  - 큰 변경/호환성 깨짐: major 올림 (0.2.0 → 1.0.0)
- `!update` 명령어가 version.txt를 참조하여 업데이트 여부 결정

### 수동 배포가 불가피한 경우
```bash
# 1. 버전 업데이트
#    - Cargo.toml의 version 수정
#    - dist/pages/version.txt도 같은 버전으로 수정

# 2. 빌드
~/.cargo/bin/cargo build --release
~/.cargo/bin/cargo build --release --target x86_64-apple-darwin

# 3. 바이너리 복사 (파일명 주의!)
cp target/release/neywa dist/pages/neywa-arm64
cp target/x86_64-apple-darwin/release/neywa dist/pages/neywa-x86_64

# 4. ★ 반드시 바이너리 실제 버전 확인 (아키텍처 확인만으로는 부족!)
dist/pages/neywa-arm64 --version   # 반드시 새 버전인지 확인
file dist/pages/neywa-x86_64       # x86_64 확인

# 5. 배포
cd dist/pages && npx wrangler pages deploy . --project-name=neywa --commit-dirty=true

# 6. 배포 후 검증
curl -s https://neywa.ai/version.txt       # 버전 확인
curl -sL https://neywa.ai/neywa-arm64 -o /tmp/t && /tmp/t --version  # 실제 바이너리 버전 확인
```

### 배포 전 확인사항
- [ ] install.sh의 BINARY 변수가 `neywa-arm64` / `neywa-x86_64` 인지 확인
- [ ] discord.rs의 self_update()가 같은 파일명 사용하는지 확인
- [ ] **`dist/pages/neywa-arm64 --version`으로 바이너리 실제 버전 확인** ← 핵심!

---

## Discord 명령어

| 명령어 | 설명 |
|--------|------|
| `!help` | 도움말 |
| `!status` | 현재 상태 |
| `!new` | 새 세션 시작 |
| `!stop` | 처리 중단 & 대기열 클리어 |
| `!queue` | 대기열 확인 |
| `!update` | 자동 업데이트 |
| `!z` | Z 모드 토글 |
| `!compact` | 세션 컨텍스트 윈도우 압축 |
| `!slash <cmd>` | Claude Code 슬래시 명령어 실행 (e.g., `!slash cost`, `!slash compact`) |

---

## 프로젝트 구조

```
src/
├── main.rs      # CLI, PID 관리, Ctrl+C 핸들링
├── discord.rs   # Discord 봇, !update 명령어
├── claude.rs    # Claude Code CLI 래퍼
├── config.rs    # 설정 파일 관리
└── tray.rs      # macOS 트레이 아이콘

dist/pages/
├── index.html      # 웹사이트
├── install.sh      # 설치 스크립트
├── neywa-arm64     # Apple Silicon 바이너리
└── neywa-x86_64    # Intel Mac 바이너리
```
