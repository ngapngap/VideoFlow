# VideoFlow Desktop — CI/CD & phát hành

## Quy trình phát hành (tự động hoàn toàn)

```bash
cd D:/VideoFlow
# 1) đổi version trong 3 file (hoặc để workflow tự sync, xem dưới)
#    videoflow-app/package.json, src-tauri/Cargo.toml, src-tauri/tauri.conf.json
git add -A && git commit -m "feat: mô tả thay đổi"
git push origin main
git tag -a v1.4.1 -m "VideoFlow Desktop v1.4.1"
git push origin v1.4.1        # ← trigger Release workflow
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
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Mật khẩu của key (hiện dùng `videofLowUpdater2026`) |

Khoá nằm ở **`D:\VideoFlow\.updater-key.pem`** — đã gitignore, **phải backup**.
Mất khoá/password thì không thể phát hành bản cập nhật cho các bản đã cài
(pubkey nằm cố định trong `tauri.conf.json`).

Đổi mật khẩu/khoá: `npx tauri signer generate -w <path>` rồi cập nhật `plugins.updater.pubkey`
trong `tauri.conf.json` + set lại 2 secrets. **Lưu ý:** đổi khoá làm mọi bản cũ không
tự cập nhật được nữa (phải cài tay 1 lần).

## CI (mọi push/PR lên main)

`.github/workflows/ci.yml` — 3 job song song:

| Job | Nội dung |
|---|---|
| Frontend | `npm ci` → `npm run check` (svelte-check) → `npm run build` |
| Rust | stub sidecar → `cargo clippy --all-targets -D warnings` → `cargo test` |
| Sidecar | `compileall` → chạy API mock → kiểm `/healthz`, `/admin/status`, chặn cookie trùng session |

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
export TAURI_SIGNING_PRIVATE_KEY="$(cat ../.updater-key.pem)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="videofLowUpdater2026"
npx tauri build
```

Không set 2 biến TAURI_SIGNING_* thì build vẫn ra NSIS nhưng **không có `.sig`** →
bản phát hành đó không dùng được cho auto-update.
