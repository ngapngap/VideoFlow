"""Drive the installed Desktop's own signed updater on a clean Windows runner."""
import base64
import json
import os
import subprocess
import time
import urllib.request
from pathlib import Path

from websockets.sync.client import connect

APP = Path(os.environ["LOCALAPPDATA"]) / "VideoFlow Desktop" / "videoflow-desktop.exe"
PORT = 19226
os.environ["WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"] = f"--remote-debugging-port={PORT}"
os.environ["WEBVIEW2_USER_DATA_FOLDER"] = str(Path(os.environ["TEMP"]) / "vf-upgrade-webview")
os.environ["VF_TEST_APP"] = str(APP)


def command(ws, method, params):
    ws.send(json.dumps({"id": 1, "method": method, "params": params}))
    while True:
        message = json.loads(ws.recv(timeout=90))
        if message.get("id") == 1:
            if "error" in message:
                raise RuntimeError(message["error"])
            return message["result"]


def evaluate(ws, expression):
    result = command(ws, "Runtime.evaluate", {"expression": expression, "awaitPromise": True,
                                               "returnByValue": True})
    if "exceptionDetails" in result:
        raise RuntimeError(result["exceptionDetails"])
    return result["result"].get("value")


def attach(version, seconds):
    deadline = time.monotonic() + seconds
    last = ""
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{PORT}/json/list",timeout=3) as response:
                targets = json.load(response)
            for target in targets:
                if target["type"] != "page":
                    continue
                ws = connect(target["webSocketDebuggerUrl"],open_timeout=5)
                if evaluate(ws,"window.__TAURI_INTERNALS__?.invoke('app_version')") == version:
                    return ws
                ws.close()
            last = repr([(t["type"],t.get("url")) for t in targets])
        except Exception as exc:
            last = str(exc)
        time.sleep(1)
    subprocess.run(["tasklist", "/fi", "imagename eq videoflow-desktop.exe"])
    subprocess.run(["tasklist", "/fi", "imagename eq msedgewebview2.exe"])
    raise TimeoutError(f"Cannot attach to installed Desktop {version}: {last}")


def installed_version():
    return subprocess.check_output(
        ["powershell", "-NoProfile", "-Command",
         "(Get-Item -LiteralPath $env:VF_TEST_APP).VersionInfo.ProductVersion"], text=True,
    ).strip()


def main():
    assert installed_version() == "1.5.5", installed_version()
    proc = subprocess.Popen([APP])
    print("Started Desktop process", proc.pid, flush=True)
    old = attach("1.5.5", 75)
    update = evaluate(old,"window.__TAURI_INTERNALS__.invoke('check_app_update')")
    print("Official installed updater check:", update, flush=True)
    assert update["available"] and update["version"] == "1.6.0"
    evaluate(old,"void window.__TAURI_INTERNALS__.invoke('install_app_update').catch(e => window.updateTestError=JSON.stringify(e))")
    deadline = time.monotonic() + 300
    while time.monotonic() < deadline:
        if APP.is_file() and installed_version() == "1.6.0":
            break
        try:
            error = evaluate(old,"window.updateTestError")
        except Exception:
            error = None
        if error:
            raise RuntimeError(error)
        time.sleep(2)
    else:
        raise TimeoutError("Signed updater did not install 1.6.0")
    old.close()
    print("Installed executable ProductVersion:", installed_version(), flush=True)
    try:
        new = attach("1.6.0", 25)
    except TimeoutError:
        subprocess.Popen([APP])
        new = attach("1.6.0", 75)
    deadline = time.monotonic() + 30
    while not evaluate(new,"Boolean(document.querySelector('[data-testid=nav-reels]'))"):
        if time.monotonic() > deadline:
            raise TimeoutError("Reels sidebar entry missing")
        time.sleep(1)
    evaluate(new,"document.querySelector('[data-testid=nav-reels]').click()")
    result = evaluate(new,"window.__TAURI_INTERNALS__.invoke('research',{action:'search_list',payload:{limit:20}})")
    assert isinstance(result.get("items"),list), result
    assert evaluate(new,"Boolean(document.querySelector('#research-topic'))")
    photo = command(new,"Page.captureScreenshot",{"format":"png"})
    Path("reels-after-update.png").write_bytes(base64.b64decode(photo["data"]))
    new.close()
    print("PASS: installed 1.5.5 updated to 1.6.0 and opened Reels using its packaged backend", flush=True)


if __name__ == "__main__":
    main()
