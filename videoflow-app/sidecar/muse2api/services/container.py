"""Service container wired once at startup and stored on ``app.state.services``."""

from __future__ import annotations

from dataclasses import dataclass

from ..accounts.pool import AccountPool
from ..accounts.store import JsonAccountStore
from ..auth.keys import KeyStore
from ..config import Settings
from ..core.matting import Matting
from ..core.media import MediaStore
from ..drivers.base import MuseDriver
from ..drivers.browser.interactive import InteractiveBrowserManager
from ..drivers.registry import create_driver
from .gateway import Gateway
from .request_log import RequestLog, current_record
from .research_store import ResearchStore
from .tasks import Task, TaskManager
from .transport import MuseResearchTransport


@dataclass
class Services:
    settings: Settings
    driver: MuseDriver
    pool: AccountPool
    gateway: Gateway
    tasks: TaskManager
    media: MediaStore
    matting: Matting
    keys: KeyStore
    requests: RequestLog
    interactive: InteractiveBrowserManager | None = None
    research_store: ResearchStore | None = None
    research: object | None = None
    research_transport: MuseResearchTransport | None = None

    @classmethod
    def build(cls, settings: Settings, driver: MuseDriver | None = None) -> Services:
        settings.ensure_dirs()
        driver = driver or create_driver(settings)
        pool = AccountPool(
            JsonAccountStore(settings.accounts_file),
            strategy=settings.pool_strategy,
            max_concurrency=settings.account_max_concurrency,
            cooldown=settings.account_cooldown,
            acquire_timeout=settings.pool_acquire_timeout,
        )
        requests = RequestLog(settings.requests_db)

        async def record_outcome(task: Task) -> None:
            record = current_record.get()
            await requests.finish_task(task.id, task.status.value, task.updated_at,
                                       (task.error or {}).get("message"),
                                       record.get("account_id") if record else None)

        interactive = InteractiveBrowserManager(settings, pool, driver)
        pool.is_blocked_cb = interactive.is_session_active
        if hasattr(driver, "set_account_guards"):
            driver.set_account_guards(
                lookup=pool.get,
                is_blocked=interactive.is_session_active,
            )

        instance = cls(
            settings=settings,
            driver=driver,
            pool=pool,
            gateway=Gateway(pool, driver, max_failover=settings.max_failover),
            tasks=TaskManager(settings.tasks_file, on_finish=record_outcome),
            media=MediaStore(settings.media_dir),
            matting=Matting(settings.matting_model),
            keys=KeyStore(settings.keys_file),
            requests=requests,
            interactive=interactive,
        )
        from .research_service import ResearchService
        instance.research = ResearchService(settings.data_dir.parent / "research", instance)
        instance.research_store = instance.research.store
        instance.research_transport = MuseResearchTransport(settings, pool, driver)
        return instance
