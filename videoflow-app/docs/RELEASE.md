# VideoFlow Desktop — CI/CD & phát hành

## Quy trình phát hành (tự động hoàn toàn)

```bash
# Từ thư mục gốc của repo VideoFlow
# 1) (khuyến nghị) đồng bộ version trong 5 manifest (hoặc để workflow tự sync từ tag/input trong runner):
#    - videoflow-app/package.json                         ("version")
#    - videoflow-app/src-tauri/Cargo.toml                 (version = "...")
#    - videoflow-app/src-tauri/tauri.conf.json            ("version")
#    - videoflow-app/sidecar/muse2api/__init__.py         (__version__ = "...")
#    - videoflow-app/sidecar/pyproject.toml               (version = "...")
git add -A && git commit -m "feat: mô tả thay đổi"
git push origin main
git tag -a v1.5.1 -m "VideoFlow Desktop v1.5.1"
git push origin v1.5.1        # ← trigger Release workflow (hoặc chạy tay qua workflow_dispatch với input version)
```

`release.yml` chạy qua 2 giai đoạn trên runner `windows-latest` (khóa `concurrency` nhóm `release-windows`, không chạy song song hai bản phát hành):

- **Job `verify` (Cổng chất lượng)**: chạy `npm ci` + `npm run check` (`svelte-check`), tạo stub `videoflow-muse-x86_64-pc-windows-msvc.exe` để `tauri-build` biên dịch, chạy `cargo clippy --all-targets -- -D warnings` + `cargo test`, và cài dependencies Python để chạy `ruff check .` + `python -m pytest -q` trên đúng commit được gắn tag (hoặc nhánh khi chạy `workflow_dispatch`).
- **Job `build` (chỉ chạy sau khi `verify` xanh)**:
  1. `Resolve version` & `Sync version into manifests` — lấy version từ tag `v*` (hoặc input `version` của `workflow_dispatch`), kiểm tra định dạng semver (`X.Y.Z` hoặc `X.Y.Z-prerelease`), rồi ghi đè version vào cả **5 manifest** trong workspace runner trước khi build:
     - `videoflow-app/package.json` (`version`)
     - `videoflow-app/src-tauri/Cargo.toml` (`version`)
     - `videoflow-app/src-tauri/tauri.conf.json` (`version`)
     - `videoflow-app/sidecar/muse2api/__init__.py` (`__version__`)
     - `videoflow-app/sidecar/pyproject.toml` (`version`)
  2. Build sidecar engine bằng PyInstaller → `videoflow-app/src-tauri/binaries/videoflow-muse-x86_64-pc-windows-msvc.exe` và kiểm tra kích thước (`>= 8 MB`) + SHA-256 (`Verify sidecar artifact`).
  3. Smoke test 2 pha trên exe thật: (1) **Browser driver thật** với Edge headless + kiểm tra `/healthz` và `/admin/status` (`driver=browser, ok=true`); (2) **Contract VideoFlow** với mock driver (`/healthz`, `/admin/status`, `/v1/models`, `POST /admin/accounts`, `/v1/videos`, `/v1/images/edits`).
  4. `npm ci`, kiểm tra secret `TAURI_SIGNING_PRIVATE_KEY` (`Verify updater signing key secret`), rồi chạy `tauri-action` (`--target x86_64-pc-windows-msvc`):
     - Build bộ cài NSIS (`VideoFlow Desktop_<ver>_x64-setup.exe`; trên GitHub Releases được chuẩn hóa tên thành `VideoFlow.Desktop_<ver>_x64-setup.exe`).
     - **Ký updater** bằng `TAURI_SIGNING_PRIVATE_KEY` → `*.exe.sig`.
     - Tạo **Draft Release** (`releaseDraft: true`) kèm `latest.json` để kiểm tra lần cuối trước khi bấm Publish trên GitHub.

## Secrets bắt buộc

| Secret | Nội dung |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | Toàn bộ nội dung file minisign private key (`.updater-key.pem`) |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Mật khẩu của key — lưu trong password manager, **không ghi vào repo/docs** |

File khoá private lưu cục bộ ngoài phạm vi commit (đã chặn bởi `.gitignore` qua `.updater-key.*`), **bắt buộc phải sao lưu an toàn**.
Mất khoá hoặc mật khẩu thì không thể phát hành bản cập nhật tự động cho các bản đã cài
(pubkey nằm cố định trong `tauri.conf.json`). Mật khẩu chỉ lưu trong password manager và
GitHub Secrets; **không bao giờ commit hay ghi vào tài liệu**.

> **Đã xoay khoá 19/11/2026** do mật khẩu cũ từng bị ghi trong tài liệu. Mọi máy đang chạy
> bản cũ hơn `v1.4.1` phải **cài tay một lần** (pubkey mới chỉ có từ v1.4.1); sau đó
> auto-update hoạt động lại bình thường.

Đổi mật khẩu/khoá: `npx tauri signer generate -w <path>` rồi cập nhật `plugins.updater.pubkey`
trong `tauri.conf.json` + cập nhật 2 secrets trên GitHub. **Lưu ý:** đổi khoá làm mọi bản cũ không
tự cập nhật được nữa (phải cài tay 1 lần).

## CI (mọi push/PR lên main)

`.github/workflows/ci.yml` — 3 job chạy song song:

| Job | Nội dung |
|---|---|
| `frontend` | `npm ci` → `npm run check` (`svelte-check`) → `npm run build` |
| `rust` | Build sidecar exe thật bằng PyInstaller → `cargo clippy --all-targets -- -D warnings` → `cargo test -- --nocapture` (gồm test spawn engine thật) → smoke `/admin/status` với `driver=browser, ok=true` trên Edge headless |
| `sidecar` | Cài `requirements.txt` + `requirements-dev.txt` → `python -m compileall -q muse2api` → `python -m ruff check .` → `python -m pytest -q` → chạy API mock kiểm `/healthz`, `/admin/status` và chặn trùng session |

## Code-signing (SmartScreen)

Hiện bộ cài **chưa được Authenticode-sign** nên Windows SmartScreen sẽ cảnh báo
"Windows protected your PC" ở lần chạy đầu — người dùng phải bấm *More info → Run anyway*.
Điều này **không ảnh hưởng auto-update** (updater dùng chữ ký minisign riêng, đã có `.sig`).

Để hết cảnh báo, cần chứng chỉ code-signing (OV ~$200/năm, EV để có reputation ngay):

```yaml
# thêm vào step tauri-action trong release.yml
env:
  TAURI_SIGNING_PRIVATE_KEY: ...
  WINDOWS_CERTIFICATE: ${{ secrets.WINDOWS_CERTIFICATE }}      # base64 .pfx
  WINDOWS_CERTIFICATE_PASSWORD: ${{ secrets.WINDOWS_CERT_PASSWORD }}
# và bundle.windows.certificateThumbprint / digestAlgorithm / timestampUrl trong tauri.conf.json
```

Không có cert thì cứ phát hành bình thường; ghi rõ trong release notes để người dùng biết
phải bấm *Run anyway*.

## Auto-update trong app

- Endpoint: `https://github.com/ngapngap/VideoFlow/releases/latest/download/latest.json`
- Khi mở app: tự kiểm tra (im lặng). Có bản mới → **banner góc dưới-trái** hiện version +
  ghi chú phát hành + nút "Cập nhật & khởi động lại".
- Bấm cập nhật: tải (hiện % tiến độ qua event `update_progress`) → cài → app tự restart.
  Chặn khi pipeline đang chạy (phải STOP trước).
- Nút `v<version>` ở thanh trạng thái: bấm để kiểm tra thủ công; hiện "· có bản mới" khi có.

## Build tay trên máy (cần cho dev)

```bash
cd videoflow-app
taskkill //F //IM videoflow-desktop.exe          # tránh khoá file exe
# build sidecar nếu sửa python: cd sidecar && pyinstaller muse2api.spec --distpath ../src-tauri/binaries --noconfirm
# Không hardcode mật khẩu ở đây: đọc từ biến môi trường của shell (hoặc file .env local đã gitignore)
export TAURI_SIGNING_PRIVATE_KEY="$(cat ../.updater-key.pem)"
: "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:?set biến này trong shell/secret manager trước khi build}"
npx tauri build
```

Không set 2 biến TAURI_SIGNING_* thì build vẫn ra NSIS nhưng **không có `.sig`** →
bản phát hành đó không dùng được cho auto-update.
