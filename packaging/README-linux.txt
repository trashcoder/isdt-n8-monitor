ISDT N8 Monitor 0.1 — Linux x86_64

AppImage: make the file executable if needed, then launch it:
  chmod +x ISDT-N8-Monitor-x86_64.AppImage
  ./ISDT-N8-Monitor-x86_64.AppImage

tar.gz: extract the archive and run ./isdt-n8-monitor.

Connect the ISDT N8 over USB. If access is denied, use the app's permission
setup prompt; it installs a device-specific udev rule and asks for your
administrator password through the desktop authorization dialog.
