# kalila from a tarball

`bin/kalila` is the GUI; `bin/kalila-headless` acquires and records without windows (a soak
tool; `--hw --seconds 600 --record DIR`, see the README). Both run from this folder; nothing
needs installing except the system libraries and, once, the board's udev rule:

    sudo install -m 0644 share/60-open-ephys-ft600.rules /etc/udev/rules.d/
    sudo udevadm control --reload && sudo udevadm trigger

Real-time priority for the acquisition thread (recommended):

    sudo install -m 0644 share/99-kalila-realtime.conf /etc/security/limits.d/
    sudo groupadd -r realtime; sudo usermod -aG realtime $USER    # then log out and in

System libraries: libasound (ALSA), libpipewire-0.3 (0.3.50 or newer), the Vulkan loader with your GPU's
Vulkan driver, and libwayland-client + libxkbcommon (Wayland) or the X11 libraries (libX11,
libXcursor, libXrandr, libXi). `BUILD-INFO` lists what the binaries link.

The board is opened with kalila's own FT600 driver: nothing from FTDI is needed.
`KALILA_DRIVER=d3xx` uses liboni's D3XX driver instead (glibc builds only; see the README).

Licences: `licenses/` (kalila GPL-3.0-or-later, liboni MIT, and every Rust crate compiled
in, under `licenses/crates/`).
