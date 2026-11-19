# VideoFlow Desktop — CI/CD & phát hành

## Quy trình phát hành (tự động hoàn toàn)

```bash
cd D:/VideoFlow
# 1) đổi version trong 3 file (hoặc để workflow tự sync, xem dưới)
#    videoflow-app/package.json, src-tauri/Cargo.toml, src-tauri/tauri.conf.json
git add -A && git commit -m "feat: mô tả thay đổi"
git push origin main
git tag -a v1.5.0 -m "VideoFlow Desktop v1.5.0"
git push origin v1.5.0        # ← trigger Release workflow
```

`release.yml` sẽ trên runner `windows-latest`:

1. `Sync version into manifests` — ghi version từ tag vào package.json / Cargo.toml / tauri.conf.json
2. Build sidecar engine bằng PyInstaller → `src-tauri/binaries/videoflow-muse-x86_64-pc-windows-msvc.exe`
3. Smoke test exe thật với mock driver (`/healthz`, `/admin/status`, `/v1/models`)
4. `npm ci` rồi `tauri-action`:
   - build NSIS (`VideoFlow Desktop_<ver>_x64-setup.exe`)
   - **ký updater** bằng `TAURI_SIGNING_PRIVATE_KEY` → `*.exe.sig`
   - publish Release + `latest.json` (endpoint app đọc để tự cập nhật)

## Secrets bắt buộc

| Secret | Nội dung |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | Toàn bộ nội dung file `.updater-key.pem` (minisign private key) |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Mật khẩu của key — lưu trong password manager, **không ghi vào repo/docs** |

Khoá nằm ở **`D:\VideoFlow\.updater-key.pem`** — đã gitignore, **phải backup**.
Mất khoá/password thì không thể phát hành bản cập nhật cho các bản đã cài
(pubkey nằm cố định trong `tauri.conf.json`). Mật khẩu chỉ lưu trong password manager +
GitHub secret; **không bao giờ commit hay ghi vào tài liệu**.

> **Đã xoay khoá 19/11/2026** do mật khẩu cũ từng bị ghi trong tài liệu. Mọi máy đang chạy
> bản cũ hơn `v1.4.1` phải **cài tay một lần** (pubkey mới chỉ có từ v1.4.1); sau đó
> auto-update hoạt động lại bình thường.

Đổi mật khẩu/khoá: `npx tauri signer generate -w <path>` rồi cập nhật `plugins.updater.pubkey`
trong `tauri.conf.json` + set lại 2 secrets. **Lưu ý:** đổi khoá làm mọi bản cũ không
tự cập nhật được nữa (phải cài tay 1 lần).

## CI (mọi push/PR lên main)

`.github/workflows/ci.yml` — 3 job song song:

| Job | Nội dung |
|---|---|
| Frontend | `npm ci` → `npm run check` (svelte-check) → `npm run build` |
| Rust | stub sidecar → `cargo clippy --all-targets -D warnings` → `cargo test` |
| Sidecar | cài `requirements.txt` (đã pin) → `compileall` → pytest (`tests/`: quota enforcement, chặn trùng session ở POST **và** PATCH) → chạy API mock kiểm `/healthz`, `/admin/status` |
| Rust | build engine exe thật → `cargo test` (gồm test spawn engine qua **browser driver thật** với Edge headless) → smoke `/admin/status` xác nhận `driver=browser, ok=true` |

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
- Khi mở app: tự kiểm tra (im lặng). Có bản mới → **banner góc dưới-phải** hiện version +
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
