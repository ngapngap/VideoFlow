# Entry point PyInstaller cho muse2api sidecar — gọi thẳng main() của __main__.
from muse2api.__main__ import main

if __name__ == "__main__":
    main()
