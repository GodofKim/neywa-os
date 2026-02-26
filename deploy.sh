#!/bin/bash
# Neywa 배포 스크립트 - 빌드 → 복사 → 검증 → 배포를 한 번에
set -e

DIST="dist/pages"
CARGO="$HOME/.cargo/bin/cargo"

# 1. 버전 확인
CARGO_VER=$(grep '^version' Cargo.toml | head -1 | sed 's/version = "//;s/"//')
TXT_VER=$(cat "$DIST/version.txt" 2>/dev/null || echo "none")

if [ "$CARGO_VER" != "$TXT_VER" ]; then
    echo "❌ 버전 불일치: Cargo.toml=$CARGO_VER / version.txt=$TXT_VER"
    echo "   version.txt를 $CARGO_VER 으로 맞춰주세요"
    exit 1
fi

echo "📦 버전: v$CARGO_VER"

# 2. 빌드
echo "🔨 빌드 중 (arm64)..."
$CARGO build --release 2>&1 | grep -E "Compiling|Finished|error" || true

echo "🔨 빌드 중 (x86_64)..."
$CARGO build --release --target x86_64-apple-darwin 2>&1 | grep -E "Compiling|Finished|error" || true

# 3. 바이너리 복사
cp target/release/neywa "$DIST/neywa-arm64"
cp target/x86_64-apple-darwin/release/neywa "$DIST/neywa-x86_64"

# 4. 아키텍처 + 버전 검증 (★ 이게 핵심: 바이너리 실제 버전 확인)
ARM_VER=$("$DIST/neywa-arm64" --version | awk '{print $2}')
X86_VER_CHECK=$(file "$DIST/neywa-x86_64" | grep -c x86_64 || true)

if [ "$ARM_VER" != "$CARGO_VER" ]; then
    echo "❌ arm64 바이너리 버전 불일치: 바이너리=$ARM_VER / 기대=$CARGO_VER"
    exit 1
fi

if [ "$X86_VER_CHECK" -eq 0 ]; then
    echo "❌ x86_64 바이너리 아키텍처 확인 실패"
    exit 1
fi

echo "✅ arm64: v$ARM_VER"
echo "✅ x86_64: $(file "$DIST/neywa-x86_64" | grep -o 'x86_64')"

# 5. 배포
echo "🚀 배포 중..."
cd "$DIST" && npx wrangler pages deploy . --project-name=neywa --commit-dirty=true

echo ""
echo "✅ 배포 완료! v$CARGO_VER"
echo "   검증: curl -s https://neywa.ai/version.txt"
