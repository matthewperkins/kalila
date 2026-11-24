#!/bin/sh
# After installing kalila's .deb or .rpm: the realtime group named in
# /etc/security/limits.d/99-kalila-realtime.conf (created if missing; nobody is added), and
# the new udev rule applied to a board already plugged in.
getent group realtime >/dev/null 2>&1 || groupadd -r realtime 2>/dev/null || true
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload 2>/dev/null || true
    udevadm trigger --subsystem-match=usb --attr-match=idVendor=0403 2>/dev/null || true
fi
exit 0
