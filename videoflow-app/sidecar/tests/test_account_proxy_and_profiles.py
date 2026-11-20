from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from httpx import ASGITransport, AsyncClient
from pydantic import ValidationError

from muse2api.accounts.model import Account, mask_proxy_url
from muse2api.accounts.pool import AccountPool
from muse2api.accounts.store import JsonAccountStore
from muse2api.api.schemas import AccountCreate, AccountUpdate
from muse2api.app import create_app
from muse2api.config import Settings
from muse2api.drivers.base import MuseDriver
from muse2api.drivers.browser.chromium import (
    ChromiumProcess,
    _build_wmi_filter,
)
from muse2api.drivers.browser.driver import (
    BrowserDriver,
    resolve_account_profile_dir,
    resolve_proxy,
)

# =========================================================================
# 1. Account model & Schema proxy support (P2-3)
# =========================================================================

def test_account_model_proxy_field():
    # Default is None
    acc_default = Account(id="acc_default")
    assert acc_default.proxy is None
    assert acc_default.model_dump()["proxy"] is None
    assert acc_default.public()["proxy"] is None

    # Custom proxy
    acc_proxy = Account(id="acc_p1", proxy="http://127.0.0.1:8080")
    assert acc_proxy.proxy == "http://127.0.0.1:8080"
    data = acc_proxy.model_dump()
    assert data["proxy"] == "http://127.0.0.1:8080"
    assert acc_proxy.public()["proxy"] == "http://127.0.0.1:8080"

    # Deserialization from JSON dict
    restored = Account.model_validate(data)
    assert restored.proxy == "http://127.0.0.1:8080"


def test_account_create_and_update_schemas_proxy():
    # AccountCreate
    create_payload = AccountCreate(label="acc1", proxy="socks5://proxy.lan:1080")
    assert create_payload.proxy == "socks5://proxy.lan:1080"

    # AccountUpdate
    update_payload = AccountUpdate(proxy="http://new-proxy:3128")
    assert update_payload.proxy == "http://new-proxy:3128"


# =========================================================================
# 2. Proxy Resolution Hierarchy (P2-3)
# =========================================================================

def test_proxy_resolution_hierarchy():
    global_proxy = "http://global-proxy.local:8080"

    # Case 1: Account has dedicated proxy -> account proxy wins
    acc_dedicated = Account(id="acc_1", proxy="http://account-proxy:8888")
    assert resolve_proxy(acc_dedicated, global_proxy) == "http://account-proxy:8888"

    # Case 2: Account has no proxy (None) -> fallback to global browser_proxy
    acc_none = Account(id="acc_2", proxy=None)
    assert resolve_proxy(acc_none, global_proxy) == global_proxy

    # Case 3: Account has empty/whitespace proxy -> fallback to global browser_proxy
    acc_empty = Account(id="acc_3", proxy="   ")
    assert resolve_proxy(acc_empty, global_proxy) == global_proxy

    # Case 4: Neither account proxy nor global proxy configured -> direct connection ("")
    assert resolve_proxy(acc_none, "") == ""
    assert resolve_proxy(None, "") == ""

    # Case 5: None account with global proxy
    assert resolve_proxy(None, global_proxy) == global_proxy


# =========================================================================
# 3. Profile Directory Isolation (P2-3)
# =========================================================================

def test_account_profile_directory_isolation(tmp_path: Path):
    settings = Settings(data_dir=tmp_path / "data")

    # Isolated directory per account: muse/profiles/<account_id>
    p1 = resolve_account_profile_dir(settings, "acc_1")
    p2 = resolve_account_profile_dir(settings, "acc_2")

    assert p1 != p2
    assert p1 == tmp_path / "data" / "profiles" / "acc_1"
    assert p2 == tmp_path / "data" / "profiles" / "acc_2"
    assert p1.name == "acc_1"
    assert p2.name == "acc_2"

    # When account_id is None, returns default profile_dir
    default_p = resolve_account_profile_dir(settings, None)
    assert default_p == settings.profile_dir
    assert default_p != p1


# =========================================================================
# 4. ChromiumProcess --proxy-server argument generation
# =========================================================================

@pytest.mark.asyncio
async def test_chromium_process_proxy_argument_passing(tmp_path: Path):
    profile_dir = tmp_path / "profile_test"
    profile_dir.mkdir(parents=True)

    # Process with proxy
    proc_proxy = ChromiumProcess(
        executable="chrome",
        port=19210,
        profile_dir=profile_dir,
        headless=True,
        proxy="http://10.10.10.10:8080",
    )

    with patch("asyncio.create_subprocess_exec") as mock_exec:
        mock_proc = MagicMock()
        mock_proc.returncode = None
        mock_exec.return_value = mock_proc

        with patch.object(proc_proxy, "browser_ws_url", new_callable=AsyncMock, return_value="ws://dummy"):
            await proc_proxy._launch()

            assert mock_exec.called
            args = mock_exec.call_args[0]
            assert "--proxy-server=http://10.10.10.10:8080" in args

    # Process without proxy (direct connection)
    proc_direct = ChromiumProcess(
        executable="chrome",
        port=19211,
        profile_dir=profile_dir,
        headless=True,
        proxy="",
    )

    with patch("asyncio.create_subprocess_exec") as mock_exec:
        mock_proc = MagicMock()
        mock_proc.returncode = None
        mock_exec.return_value = mock_proc

        with patch.object(proc_direct, "browser_ws_url", new_callable=AsyncMock, return_value="ws://dummy"):
            await proc_direct._launch()

            assert mock_exec.called
            args = mock_exec.call_args[0]
            assert not any(arg.startswith("--proxy-server") for arg in args)


# =========================================================================
# 5. Multi-Driver Per-Account Management in BrowserDriver (P2-3)
# =========================================================================

def test_browser_driver_manages_distinct_account_drivers(tmp_path: Path):
    settings = Settings(
        data_dir=tmp_path / "data",
        browser_proxy="http://default-proxy:8080",
    )
    main_driver = BrowserDriver(settings)
    assert main_driver.account is None
    assert len(main_driver.account_drivers) == 0

    acc1 = Account(id="acc_1", proxy="http://proxy1:8080")
    acc2 = Account(id="acc_2", proxy="socks5://proxy2:1080")
    acc3 = Account(id="acc_3", proxy=None)  # should fall back to default-proxy

    drv1 = main_driver.get_account_driver(acc1)
    drv2 = main_driver.get_account_driver(acc2)
    drv3 = main_driver.get_account_driver(acc3)

    # Distinct driver instances
    assert drv1 is not drv2
    assert drv2 is not drv3
    assert len(main_driver.account_drivers) == 3

    # Distinct proxies
    assert drv1.proxy == "http://proxy1:8080"
    assert drv2.proxy == "socks5://proxy2:1080"
    assert drv3.proxy == "http://default-proxy:8080"

    # Distinct profile directories
    assert drv1.profile_dir != drv2.profile_dir
    assert drv1.profile_dir.name == "acc_1"
    assert drv2.profile_dir.name == "acc_2"
    assert drv3.profile_dir.name == "acc_3"

    # Distinct ports
    assert drv1.port != drv2.port
    assert drv2.port != drv3.port


@pytest.mark.asyncio
async def test_browser_driver_replaces_driver_when_proxy_changes(tmp_path: Path):
    settings = Settings(data_dir=tmp_path / "data")
    main_driver = BrowserDriver(settings)

    acc = Account(id="acc_dyn", proxy="http://proxy-v1:8080")
    drv1 = main_driver.get_account_driver(acc)
    assert drv1.proxy == "http://proxy-v1:8080"

    # Update account proxy
    acc_updated = Account(id="acc_dyn", proxy="http://proxy-v2:9090")
    with patch.object(drv1, "shutdown", new_callable=AsyncMock) as mock_shutdown:
        drv2 = main_driver.get_account_driver(acc_updated)
        assert drv2 is not drv1
        assert drv2.proxy == "http://proxy-v2:9090"
        await asyncio.sleep(0.05)
        assert mock_shutdown.called


# =========================================================================
# 6. CDP Reconnect & Process Cleanup Isolated Per Account Driver (P2-3)
# =========================================================================

@pytest.mark.asyncio
async def test_cdp_recovery_isolated_to_account_driver(tmp_path: Path):
    settings = Settings(data_dir=tmp_path / "data")
    main_driver = BrowserDriver(settings)

    acc1 = Account(id="acc_isolated_1", proxy="http://proxy1:8080")
    acc2 = Account(id="acc_isolated_2", proxy="http://proxy2:8080")

    drv1 = main_driver.get_account_driver(acc1)
    drv2 = main_driver.get_account_driver(acc2)

    # Create dummy lock files in both profiles
    drv1.profile_dir.mkdir(parents=True, exist_ok=True)
    drv2.profile_dir.mkdir(parents=True, exist_ok=True)

    lock1 = drv1.profile_dir / "SingletonLock"
    lock2 = drv2.profile_dir / "SingletonLock"
    lock1.write_text("lock1")
    lock2.write_text("lock2")

    with patch("muse2api.drivers.browser.driver._kill_stale_browsers") as mock_kill, \
         patch("muse2api.drivers.browser.driver._is_port_in_use", return_value=False), \
         patch.object(drv1, "startup", new_callable=AsyncMock) as mock_startup1, \
         patch.object(drv2, "startup", new_callable=AsyncMock) as mock_startup2:

        # Recover drv1 only
        await drv1._recover_browser()

        # drv1 cleaned up its own lock file, but drv2's lock file is untouched!
        assert not lock1.exists()
        assert lock2.exists()

        # mock_kill was called with drv1's profile and port
        assert mock_kill.called
        call_args = mock_kill.call_args[0]
        assert call_args[0] == drv1.profile_dir
        assert call_args[1] == drv1.port

        # Only drv1 restarted
        assert mock_startup1.called
        assert not mock_startup2.called


# =========================================================================
# 7. Admin Endpoints & Store Persistence for Proxy (P2-3)
# =========================================================================

@pytest.mark.asyncio
async def test_admin_api_account_proxy_save_and_update(tmp_path: Path):
    settings = Settings(
        data_dir=tmp_path / "data",
        media_dir=tmp_path / "media",
        admin_key="test-admin-key",
    )
    settings.ensure_dirs()

    store = JsonAccountStore(settings.accounts_file)
    AccountPool(store, strategy="round_robin", max_concurrency=1)

    class MockTestDriver(MuseDriver):
        name = "mock_test"
        async def chat_stream(self, account, req):
            yield "test"

    app = create_app(settings, MockTestDriver())

    async with AsyncClient(
        transport=ASGITransport(app=app),
        base_url="http://test",
        headers={"Authorization": f"Bearer {settings.effective_admin_key}"},
    ) as client:
        # 1. POST /admin/accounts with proxy
        resp = await client.post(
            "/admin/accounts",
            json={
                "label": "Account With Proxy",
                "proxy": "http://user:pass@192.168.1.50:8080",
                "cookies": {"hatch_sess": "sess1", "hatch_native_auth_device": "dev1"},
            },
        )
        assert resp.status_code == 200, resp.text
        data = resp.json()["account"]
        acc_id = data["id"]
        assert data["proxy"] == "http://***@192.168.1.50:8080"

        # Verify persisted unmasked in accounts.json
        loaded = await store.load()
        matched = next(a for a in loaded if a.id == acc_id)
        assert matched.proxy == "http://user:pass@192.168.1.50:8080"

        # 2. PATCH /admin/accounts/{id} update proxy
        patch_resp = await client.patch(
            f"/admin/accounts/{acc_id}",
            json={"proxy": "socks5://10.0.0.1:1080"},
        )
        assert patch_resp.status_code == 200, patch_resp.text
        updated_data = patch_resp.json()["account"]
        assert updated_data["proxy"] == "socks5://10.0.0.1:1080"

        # Verify persisted again
        loaded2 = await store.load()
        matched2 = next(a for a in loaded2 if a.id == acc_id)
        assert matched2.proxy == "socks5://10.0.0.1:1080"

        # 3. PATCH /admin/accounts/{id} clear proxy (direct connection)
        clear_resp = await client.patch(
            f"/admin/accounts/{acc_id}",
            json={"proxy": ""},
        )
        assert clear_resp.status_code == 200
        assert clear_resp.json()["account"]["proxy"] is None

        loaded3 = await store.load()
        matched3 = next(a for a in loaded3 if a.id == acc_id)
        assert matched3.proxy is None


# =========================================================================
# 8. Stale Browser Cleanup Process Isolation (P2-3 Remediation)
# =========================================================================

def test_wmi_filter_prevents_cross_account_process_termination():
    """Verify that WMI filter generated for Account A strictly isolates to Account A
    and NEVER matches Account B's command line (Fixing Finding 1)."""
    base_dir = Path(r"C:\Users\User\AppData\Roaming\VideoFlowDesktop\muse\profiles")
    profile_a = base_dir / "acc_A"
    profile_b = base_dir / "acc_B"
    port_a = 19211
    port_b = 19212

    filter_a = _build_wmi_filter(profile_a, port_a)

    # Command lines for Account A and Account B
    cmd_a = (
        r'"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe" '
        rf'--remote-debugging-port={port_a} --remote-allow-origins=* '
        rf'--user-data-dir="{profile_a}" --no-first-run'
    )
    cmd_b = (
        r'"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe" '
        rf'--remote-debugging-port={port_b} --remote-allow-origins=* '
        rf'--user-data-dir="{profile_b}" --no-first-run'
    )

    # Filter generated for Account A must NOT contain Account B's profile or port
    assert "acc_B" not in filter_a
    assert str(port_b) not in filter_a

    # Must contain Account A's profile and port
    assert str(profile_a) in filter_a
    assert f"--remote-debugging-port={port_a}" in filter_a
    assert "acc_A" in filter_a

    # Simulate WMI condition evaluation for both command lines
    import re

    def eval_filter(wmi_cond: str, cmd: str) -> bool:
        branches = wmi_cond.split(" -or ")
        for branch in branches:
            branch = branch.strip()
            if branch.startswith("$_.CommandLine.Contains('"):
                needle = branch[len("$_.CommandLine.Contains('") : -len("')")]
                if needle in cmd:
                    return True
            elif branch.startswith("($_.CommandLine.Contains('VideoFlowDesktop')"):
                if "VideoFlowDesktop" in cmd and profile_a.name in cmd:
                    return True
            elif "-imatch" in branch:
                m_esc = re.search(r"\[regex\]::Escape\('([^']+)'\)", branch)
                if m_esc:
                    pat = r'--user-data-dir="?' + re.escape(m_esc.group(1)) + r'("|\s|$)'
                    if re.search(pat, cmd, re.IGNORECASE):
                        return True
                m_port = re.search(r"-imatch '(--remote-debugging-port=\d+\(\\s\|\$\))'", branch)
                if m_port and re.search(m_port.group(1), cmd, re.IGNORECASE):
                    return True
        return False

    # Account A's filter MUST match Account A
    assert eval_filter(filter_a, cmd_a) is True

    # Account A's filter MUST NOT match Account B
    assert eval_filter(filter_a, cmd_b) is False


# =========================================================================
# 9. Quota Cache Eviction In Multi-Driver Architecture (P2-3 Remediation)
# =========================================================================

def test_forget_quota_evicts_cache_in_sub_driver(tmp_path: Path):
    """Verify that forget_quota on root driver clears cached snapshots on both
    the root driver and the delegated sub-driver (Fixing Finding 2)."""
    settings = Settings(data_dir=tmp_path / "data")
    root_driver = BrowserDriver(settings)

    acc = Account(id="acc_quota_test", proxy="http://proxy:8080")
    sub_drv = root_driver.get_account_driver(acc)

    # Simulate cached quota in both root and sub-driver
    dummy_snapshot = {
        "plan": "Pro",
        "weekly_used_pct": 50,
        "source": "browser",
        "fetched_at": 100000.0,
    }
    root_driver._quota_cache[acc.id] = dict(dummy_snapshot)
    sub_drv._quota_cache[acc.id] = dict(dummy_snapshot)

    assert acc.id in root_driver._quota_cache
    assert acc.id in sub_drv._quota_cache

    # Call forget_quota on root driver
    root_driver.forget_quota(acc.id)

    # Must be evicted from BOTH root driver and sub-driver
    assert acc.id not in root_driver._quota_cache
    assert acc.id not in sub_drv._quota_cache


def test_account_public_masks_proxy_credentials_and_strips_auth_tokens():
    acc = Account(
        id="acc_sec",
        proxy="http://myuser:supersecret@198.51.100.7:8080",
        meta={
            "quota": {"plan": "Pro", "weekly_used_pct": 12},
            "auth_headers": {"Authorization": "Bearer secret-jwt"},
            "auth_tokens": {"jwt": "secret-token"},
        },
    )
    pub = acc.public()
    assert pub["proxy"] == "http://***@198.51.100.7:8080"
    assert "supersecret" not in str(pub)
    assert "auth_headers" not in pub["meta"]
    assert "auth_tokens" not in pub["meta"]
    assert pub["meta"]["quota"]["plan"] == "Pro"


@pytest.mark.asyncio
async def test_chromium_process_uses_local_relay_for_auth_socks5_and_socks5h(tmp_path: Path):
    for proxy_url, expect_relay in [
        ("socks5://alice:s3cr3t@10.0.0.1:1080", True),
        ("socks5h://10.0.0.1:1080", True),
        ("socks5://10.0.0.1:1080", False),
    ]:
        profile_dir = tmp_path / f"profile_{expect_relay}_{proxy_url.split(':')[0]}"
        profile_dir.mkdir(parents=True, exist_ok=True)
        proc = ChromiumProcess(
            executable="msedge.exe",
            port=19310,
            profile_dir=profile_dir,
            headless=True,
            proxy=proxy_url,
        )
        launched_args: list[str] = []

        async def fake_exec(*args, _buf=launched_args, **kwargs):
            _buf.extend(args)
            m = MagicMock()
            m.returncode = None
            m.terminate = MagicMock()
            m.wait = AsyncMock(return_value=0)
            return m

        with (
            patch("asyncio.create_subprocess_exec", side_effect=fake_exec),
            patch("muse2api.drivers.browser.chromium._kill_stale_browsers", return_value=0),
            patch("muse2api.drivers.browser.chromium._is_port_in_use", return_value=False),
            patch.object(
                proc,
                "browser_ws_url",
                new=AsyncMock(return_value="ws://127.0.0.1:19310/devtools/browser/1"),
            ),
        ):
            ws = await proc.start()
            assert ws == "ws://127.0.0.1:19310/devtools/browser/1"

        proxy_args = [a for a in launched_args if a.startswith("--proxy-server=")]
        assert len(proxy_args) == 1
        assert "s3cr3t" not in " ".join(launched_args)
        if expect_relay:
            assert proxy_args[0].startswith("--proxy-server=http://127.0.0.1:")
            assert proc._proxy_relay is not None
        else:
            assert proxy_args[0] == f"--proxy-server={proxy_url}"
            assert proc._proxy_relay is None

        await proc.stop()
        assert proc._proxy_relay is None


@pytest.mark.asyncio
async def test_chromium_process_refuses_blind_attach_and_reallocates_occupied_port(
    tmp_path: Path,
):
    profile_dir = tmp_path / "profile_occupied"
    profile_dir.mkdir(parents=True)

    async def foreign_cdp(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        await reader.readuntil(b"\r\n\r\n")
        body = (
            f'{{"webSocketDebuggerUrl":"ws://127.0.0.1:{busy_port}/devtools/browser/FOREIGN"}}'
        ).encode()
        writer.write(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
            + f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode()
            + body
        )
        await writer.drain()
        writer.close()
        await writer.wait_closed()

    foreign_srv = await asyncio.start_server(foreign_cdp, "127.0.0.1", 0)
    busy_port = foreign_srv.sockets[0].getsockname()[1]

    proc = ChromiumProcess(
        executable="msedge.exe",
        port=busy_port,
        profile_dir=profile_dir,
        headless=True,
    )
    launched_args: list[str] = []

    async def fake_exec(*args, **kwargs):
        launched_args.extend(args)
        m = MagicMock()
        m.returncode = None
        m.terminate = MagicMock()
        m.wait = AsyncMock(return_value=0)
        return m

    async def own_ws_url() -> str:
        return f"ws://127.0.0.1:{proc.port}/devtools/browser/OWN"

    try:
        with (
            patch("asyncio.create_subprocess_exec", side_effect=fake_exec),
            patch("muse2api.drivers.browser.chromium._kill_stale_browsers", return_value=0),
            patch.object(proc, "browser_ws_url", side_effect=own_ws_url),
        ):
            ws = await proc.start()

        assert proc.port != busy_port
        assert ws == f"ws://127.0.0.1:{proc.port}/devtools/browser/OWN"
        assert "FOREIGN" not in ws
        assert f"--remote-debugging-port={proc.port}" in launched_args
        await proc.stop()
    finally:
        foreign_srv.close()
        await foreign_srv.wait_closed()


@pytest.mark.asyncio
async def test_chromium_process_launch_checks_returncode_before_ws_url(tmp_path: Path):
    profile_dir = tmp_path / "profile_exited"
    profile_dir.mkdir(parents=True)
    proc = ChromiumProcess(
        executable="msedge.exe",
        port=19320,
        profile_dir=profile_dir,
        headless=True,
    )
    m_proc = MagicMock()
    m_proc.returncode = 1
    ws_mock = AsyncMock(return_value="ws://127.0.0.1:19320/devtools/browser/FOREIGN")

    with (
        patch("asyncio.create_subprocess_exec", return_value=m_proc),
        patch.object(proc, "browser_ws_url", new=ws_mock),
    ):
        result = await proc._launch()

    assert result is None
    ws_mock.assert_not_awaited()


def test_mask_proxy_url_edge_cases():
    assert mask_proxy_url(None) is None
    assert mask_proxy_url("") is None
    assert mask_proxy_url("   ") is None

    # @ inside password must not leak suffix
    assert mask_proxy_url("http://user:p@ss@10.0.0.1:8080") == "http://***@10.0.0.1:8080"
    assert mask_proxy_url("http://user:p@ss:123@10.0.0.1:8080") == "http://***@10.0.0.1:8080"

    # IPv6 brackets preserved (auth & unauth, with & without port)
    assert mask_proxy_url("http://user:p@ss@[::1]:8080") == "http://***@[::1]:8080"
    assert (
        mask_proxy_url("socks5h://user:pass@[2001:db8::1]:1080")
        == "socks5h://***@[2001:db8::1]:1080"
    )
    assert mask_proxy_url("http://[::1]:8080") == "http://[::1]:8080"
    assert mask_proxy_url("socks5h://[::1]") == "socks5h://[::1]"

    # Password-only, username-only, empty password, bare @
    assert mask_proxy_url("http://:secret@10.0.0.1:8080") == "http://***@10.0.0.1:8080"
    assert mask_proxy_url("http://alice@10.0.0.1:8080") == "http://***@10.0.0.1:8080"
    assert mask_proxy_url("http://alice:@10.0.0.1:8080") == "http://***@10.0.0.1:8080"
    assert mask_proxy_url("http://@10.0.0.1:8080") == "http://***@10.0.0.1:8080"

    # Host casing preserved for Rust parity
    assert (
        mask_proxy_url("http://user:pass@Proxy.Example.COM:8080")
        == "http://***@Proxy.Example.COM:8080"
    )

    # Path kept, query/fragment dropped
    assert (
        mask_proxy_url("http://user:pass@10.0.0.1:8080/proxy?token=secret#frag")
        == "http://***@10.0.0.1:8080/proxy"
    )

    # Malformed / unparseable inputs return "***" without leaking credentials
    for bad in (
        "http://user:secret@[bad:8080",
        "http://user:secret@10.0.0.1:99999",
        "http://user:secret@",
        "user:secret@10.0.0.1:8080",
    ):
        masked = mask_proxy_url(bad)
        assert masked == "***"
        assert "secret" not in masked


def test_account_public_masks_at_password_and_ipv6():
    acc = Account(
        id="acc_ipv6",
        proxy="socks5h://alice:p@ss:w0rd@[2001:db8::1]:1080",
    )
    pub = acc.public()
    assert pub["proxy"] == "socks5h://***@[2001:db8::1]:1080"
    assert "p@ss" not in str(pub)
    assert "w0rd" not in str(pub)


def test_proxy_url_validation_schemes_ipv6_and_control_chars():
    for valid in (
        None,
        "",
        "http://127.0.0.1:8080",
        "https://user:pass@proxy.example.com:8443",
        "socks5://10.0.0.1:1080",
        "socks5h://user:p%40ss@[::1]:1080",
        "http://[::1]",
        "http://127.0.0.1:8080/",
    ):
        assert AccountCreate(proxy=valid).proxy == valid
        assert AccountUpdate(proxy=valid).proxy == valid

    for invalid in (
        "socks4://127.0.0.1:1080",
        "SOCKS4://127.0.0.1:1080",
        "http://127.0.0.1:8080\r\n",
        "http://user:pa\nss@10.0.0.1:8080",
        "http://10.0.0.1\t:8080",
        "http://10.0.0.1:8080\x00",
        "http://10.0.0.1:8080\x1f",
        "http://10.0.0.1:8080\x7f",
        "http://127.0.0.1 :8080",
        "http://user:p ass@127.0.0.1:8080",
        "http://127.0.0.1:0",
        "http://127.0.0.1:65536",
        "http://127.0.0.1:",
        "http://::1:8080",
        "http://[::1]extra:8080",
        "http://[127.0.0.1]:8080",
        "http://127.0.0.1:8080?x=1",
        "http://127.0.0.1:8080#frag",
        "http://127.0.0.1:8080/path",
        "http://" + ("a" * 2045) + ":8080",
    ):
        with pytest.raises(ValidationError):
            AccountCreate(proxy=invalid)
        with pytest.raises(ValidationError):
            AccountUpdate(proxy=invalid)


