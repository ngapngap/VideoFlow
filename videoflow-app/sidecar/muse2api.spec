# -*- mode: python ; coding: utf-8 -*-
# PyInstaller spec: muse2api sidecar onefile cho VideoFlow Desktop.
# Build:  pyinstaller muse2api.spec --distpath ../src-tauri/binaries --workpath build --noconfirm
# Data `muse2api/web/**` (dashboard.html + extension) được resolve lúc runtime qua
# `Path(__file__).resolve().parents[2] / "web"` — onefile giải nén vào _MEIPASS nên
# đặt data thành `muse2api/web` khớp chính xác cấu trúc đó.

import os

block_cipher = None

datas = [
    (os.path.join("muse2api", "web"), "muse2api/web"),
]

hiddenimports = [
    # Driver lazy-import trong registry.create_driver — PyInstaller không thấy được
    "muse2api.drivers.mock",
    "muse2api.drivers.http",
    "muse2api.drivers.http.driver",
    "muse2api.drivers.browser.driver",
    "muse2api.drivers.browser.dom",
    "muse2api.drivers.browser.chromium",
    "muse2api.drivers.browser.cdp",
    "muse2api.drivers.browser.interactive",
    "muse2api.drivers.browser.proxy_relay",
    # SOCKS proxy support for httpx
    "socksio",
    "httpcore._async.socks_proxy",
    "httpcore._sync.socks_proxy",
    # uvicorn server
    "uvicorn.logging",
    "uvicorn.loops",
    "uvicorn.loops.auto",
    "uvicorn.loops.asyncio",
    "uvicorn.protocols",
    "uvicorn.protocols.http",
    "uvicorn.protocols.http.h11_impl",
    "uvicorn.protocols.http.auto",
    "uvicorn.protocols.http.httptools_impl",
    "uvicorn.protocols.websockets",
    "uvicorn.protocols.websockets.websockets_impl",
    "uvicorn.protocols.websockets.auto",
    "uvicorn.lifespan",
    "uvicorn.lifespan.on",
    "uvicorn.lifespan.off",
    # pydantic v2
    "pydantic_settings",
    "email.mime.multipart",
    "python_multipart",
]

a = Analysis(
    ["run_muse2api.py"],
    pathex=["."],
    binaries=[],
    datas=datas,
    hiddenimports=hiddenimports,
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[],
    cipher=block_cipher,
    noarchive=False,
)

pyz = PYZ(a.pure, a.zipped_data, cipher=block_cipher)

exe = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.zipfiles,
    a.datas,
    [],
    name="videoflow-muse-x86_64-pc-windows-msvc",
    console=True,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
)
