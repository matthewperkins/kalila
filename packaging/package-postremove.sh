#!/bin/sh
# After removing kalila: udev forgets its rule. The realtime group is left, since other
# programs may use it.
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload 2>/dev/null || true
fi
exit 0
