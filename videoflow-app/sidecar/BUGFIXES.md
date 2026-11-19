# muse2api sidecar — bản vá lỗi cho VideoFlow Desktop

Thư mục này chứa bản sao source `muse2api` v0.4.0 (upstream: https://github.com/www222fff/muse2api)
được đóng gói thành engine onefile `videoflow-muse.exe` (tên file dev: `videoflow-muse-x86_64-pc-windows-msvc.exe`) cho VideoFlow Desktop.

Bản sao này **không giống 100% upstream** — nó có 3 bản vá lỗi bên dưới. Khi cập nhật
muse2api từ upstream, phải áp lại (hoặc re-apply) các vá này.

## Fix 1 — Báo đúng lỗi khi muse.ai cắt VM (workspace bị ngắt)

**File:** `muse2api/errors.py`, `muse2api/accounts/pool.py`

**Vấn đề:** khi muse.ai ngắt workspace của account (`vm_state=DISABLED`), lỗi nổi lên
dưới dạng khó hiểu (`failed to attach image: no-file-input`) — gây hiểu lầm là lỗi attach ảnh.

**Sửa:**
- Thêm class `UpstreamVmDisabled(UpstreamError)`: HTTP 503, `code = "upstream_vm_disabled"`,
  `retryable = False` (fail-over sang account khác vô nghĩa khi workspace đã bị cắt), message:
  `muse.ai disconnected this account's workspace (vm_state=DISABLED); it needs a support ticket on muse.ai to reconnect.`
- `AccountPool._apply_result`: `UpstreamVmDisabled` → đánh dấu account `INVALID`
  (giống `UpstreamAuthError`) để pool không lease lại account chết.

## Fix 2 — Race lúc hydrate + nhận diện trang "Muse is disconnected"

**File:** `muse2api/drivers/browser/dom.py`, `muse2api/drivers/browser/driver.py`

**Vấn đề a (race):** trên thread mới, textarea mount trước `input[type=file]` ~0.5s.
Attach ảnh ngay sau khi trang ready làm **fail khoảng 1/3 lần** trên account khoẻ.

**Vấn đề b:** trang "We disconnected ... to protect your information" (workspace bị cắt)
không được nhận diện — phải chờ hết timeout 20s rồi báo nhầm "page did not become ready".

**Sửa:**
- `PAGE_STATE` trả thêm 2 field:
  - `hasFileInput: !!document.querySelector('input[type=file]')`
  - `vmDisabled: /We disconnected .* to protect your information/i.test(body)`
- `_attach()`: trước khi attach, poll chờ `hasFileInput` tối đa `_ATTACH_READY_TIMEOUT = 3.0s`
  (sleep 0.15s mỗi vòng) — hết giờ vẫn attach như cũ để không phá flow.
- `_new_thread()`: trong vòng poll, gặp `vmDisabled` → raise `UpstreamVmDisabled` **ngay**
  kèm hướng dẫn mở ticket, không chờ hết `page_ready_timeout`.

## Fix 3 — Chặn import trùng session

**File:** `muse2api/api/routes/admin.py`

**Vấn đề:** import cùng một `hatch_sess` hai lần tạo 2 account cùng tranh nhau cookie
rotation — session tự đá nhau và cả hai cùng chết.

**Sửa:**
- Thêm `_check_duplicate_session(acc, svc)`: so `hatch_sess` với mọi account hiện có.
- `POST /admin/accounts` và `POST /admin/accounts/import` trả **400** khi trùng:
  `hatch_sess already belongs to account 'acc_xxx' (label); log in again in the browser and export the fresh cookies`
- Batch import chỉ đánh fail **entry trùng**, các entry khác vẫn vào.
- Cờ thoát: `?allow_duplicate_session=true` (dùng khi người dùng chủ động bỏ qua cảnh báo).

## Fix 4 — Engine chết vì Edge mồ côi giữ profile (`exit code 3`)

**File:** `muse2api/drivers/browser/chromium.py` (+ `videoflow-app/src-tauri/src/muse_sidecar.rs`)

**Vấn đề:** khi một Edge mồ côi (do engine/app bị kill cứng) còn giữ `--user-data-dir`
của profile i, lần launch Edge mới sẽ "chuyển tiếp" sang instance cũ rồi thoát ngay —
port DevTools không bao giờ mở → `RuntimeError: Chromium did not expose a DevTools endpoint`
→ uvicorn thoát **exit code 3** (`MUSE_SIDECAR_EXITED`).

**Sửa:**
- `chromium.py`: thêm `_kill_stale_browsers(profile_dir)` — taskkill `/T /F` mọi `msedge.exe`
  khớp **2 needle** (đường dẫn profile đầy đủ, HOẶC bộ ba `VideoFlowDesktop` +
  `muse` + `browser-profile`) vì process con của Edge có thể báo path dạng 8.3
  (`NAMDM~1.DAN`) nên chỉ so path đầy đủ sẽ bỏ sót. Không bao giờ khớp browser của
  người dùng. Chờ 1.5s cho Windows nhả singleton mutex.
- `start()`: 3 lần thử — lần 1 dọn stale + xoá lock; lần 3 **xoá hẳn profile** (cookie
  nằm ở `accounts.json` riêng, profile chỉ là cache của browser); mỗi lần chờ CDP 60s.
- stderr của Edge giờ ghi ra `<data>/chromium-stderr.log` và **log 800 ký tự cuối khi
  Edge thoát sớm** — trước đây nuốt vào DEVNULL nên không biết nguyên nhân.
- Rust: `start()` tự thử lại **1 lần** (dọn orphan + chờ 1.5s) trước khi báo lỗi; thông
  báo lỗi gắn thêm dòng cuối của `chromium-stderr.log` / `engine-stderr.log` để user
  thấy nguyên nhân thật thay vì chỉ "exit code 3". `stop()` dọn orphan sau `taskkill /T`.

## Build lại sidecar

Yêu cầu: Python 3.10+ với các dependency của muse2api + PyInstaller.

```powershell
cd videoflow-app/sidecar
pip install "fastapi>=0.115" "uvicorn[standard]>=0.32" "pydantic>=2.9" "pydantic-settings>=2.5" "httpx[socks]>=0.27" "websockets>=13.0" "python-multipart>=0.0.9" pyinstaller
pyinstaller muse2api.spec --distpath ../src-tauri/binaries --workpath build --noconfirm
```

Đầu ra: `videoflow-app/src-tauri/binaries/videoflow-muse-x86_64-pc-windows-msvc.exe`
(khai báo `bundle.externalBin` trong `tauri.conf.json` → NSIS bundle kèm).

## Kiểm thử nhanh

```powershell
# Mock driver — không cần Chromium, kiểm API surface
$env:MUSE2API_DRIVER="mock"; $env:MUSE2API_PORT="18699"; $env:MUSE2API_DATA_DIR="$env:TEMP\m2a"
$env:MUSE2API_API_KEY="testkey123"
.\src-tauri\binaries\videoflow-muse-x86_64-pc-windows-msvc.exe
curl http://127.0.0.1:18699/healthz
curl -H "Authorization: Bearer testkey123" http://127.0.0.1:18699/admin/status

# Browser driver — cần Edge/Chrome
$env:MUSE2API_DRIVER="browser"; $env:MUSE2API_CHROMIUM_PATH="C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
```
