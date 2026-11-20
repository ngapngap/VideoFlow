# Entry point PyInstaller cho muse2api sidecar — gọi thẳng main() của __main__.
import multiprocessing

from muse2api.__main__ import main

if __name__ == "__main__":
    multiprocessing.freeze_support()
    main()
