"""Exercise the updater in the shipped Windows app on a clean runner."""
import os
import subprocess
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

APP = Path(os.environ["LOCALAPPDATA"]) / "VideoFlow Desktop" / "videoflow-desktop.exe"
CDP = "http://127.0.0.1:19226"
os.environ["WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"] = "--remote-debugging-port=19226"


def attach(chromium, version, seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            browser = chromium.connect_over_cdp(CDP, timeout=3000)
            for context in browser.contexts:
                for page in context.pages:
                    if page.evaluate("() => window.__TAURI_INTERNALS__?.invoke('app_version')") == version:
                        return browser, page
        except Exception:
            pass
        time.sleep(1)
    raise TimeoutError(f"Desktop {version} did not open its application window")


def installed_version():
    return subprocess.check_output(
        ["powershell", "-NoProfile", "-Command",
         "(Get-Item -LiteralPath $env:VF_TEST_APP).VersionInfo.ProductVersion"],
        text=True,
    ).strip()


with sync_playwright() as pw:
    os.environ["VF_TEST_APP"] = str(APP)
    assert installed_version() == "1.5.5", installed_version()
    subprocess.Popen([APP])
    before, old_page = attach(pw.chromium, "1.5.5", 90)
    update = old_page.evaluate("() => window.__TAURI_INTERNALS__.invoke('check_app_update')")
    print("Official installed updater check:", update, flush=True)
    assert update["available"] and update["version"] == "1.6.0"
    old_page.evaluate("""() => {window.__TAURI_INTERNALS__.invoke('install_app_update')
        .catch(error => window.updateTestError = JSON.stringify(error));}""")
    deadline = time.monotonic() + 300
    while time.monotonic() < deadline:
        if APP.is_file() and installed_version() == "1.6.0":
            break
        try:
            error = old_page.evaluate("() => window.updateTestError")
        except Exception:
            error = None  # Old WebView exits during the installer.
        if error:
            raise RuntimeError(error)
        time.sleep(2)
    else:
        raise TimeoutError("Updater did not install 1.6.0")
    try:
        after, new_page = attach(pw.chromium, "1.6.0", 40)
    except TimeoutError:
        subprocess.Popen([APP])
        after, new_page = attach(pw.chromium, "1.6.0", 90)
    new_page.get_by_test_id("nav-reels").click(timeout=30000)
    result = new_page.evaluate("""() => window.__TAURI_INTERNALS__.invoke(
        'research', {action:'search_list',payload:{limit:20}})""")
    assert isinstance(result.get("items"), list), result
    settings = new_page.evaluate("() => window.__TAURI_INTERNALS__.invoke('get_settings')")
    assert settings["settings"]["prompt_source"] == "gateway"
    assert settings["settings"]["reels_min_likes"] == 500
    new_page.screenshot(path="reels-after-update.png")
    print("PASS: signed updater installed 1.6.0, new desktop opened Reels", flush=True)
