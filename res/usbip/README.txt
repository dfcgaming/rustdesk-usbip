Vendored installers for Remote USB forwarding on Windows (auto-installed
on first use, see src/platform/windows_usbip.rs):

- usbipd-win_5.3.0_x64.msi  — https://github.com/dorssel/usbipd-win (GPL-3.0)
  USB/IP server side; serves devices on TCP 3240.
- USBip-0.9.8.1-x64.exe     — https://github.com/vadimgrn/usbip-win2 (BSD-2-Clause)
  USB/IP client side (vhci/UDE driver + usbip.exe); its installation
  restarts USB hubs and shows one elevation prompt.

Keep these in sync with the versions the wrapper code expects (`x64` only;
ARM64 requires swapping in the arm64 counterparts).
