---
name: Bug report
about: Report a fault in kalila, including a board or recording problem
labels: bug
---

Do not attach recordings, or anything with personal data. Describe the problem instead, and
check logs and config files for paths and names before you attach them.

**What happened**

<!-- What you saw, and what you expected instead. -->

**Steps to reproduce**

1.
2.

**kalila**

- Version or commit:
- How installed: release package (.deb, .rpm, tarball), Mac app, or source build:
- Simulated board or hardware:

**System**

- Linux distribution, or macOS version:
- GPU and driver:

**Board** (if it involves hardware)

- Gateware version:
- Board ID:
- Headstage on port A:
- Headstage on port B:

**Log**

Run kalila with `KALILA_USB_TRACE=1 RUST_LOG=debug` set, and attach the log. From a terminal
it goes to stderr. A desktop launch on Linux writes `~/.local/state/kalila/kalila.log`; an app
started from Finder on macOS writes `~/Library/Logs/kalila/kalila.log`.
