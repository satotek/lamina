Lamina
======

A lightweight git client.

Run lamina.exe to open the welcome screen, or pass a path to open the
repository containing it:

    lamina.exe C:\src\lamina

Lamina runs the git command line for every write and reads repositories
directly, so Git for Windows has to be installed and on PATH.

Settings live in %APPDATA%\Lamina.

This build is not code signed, so Windows SmartScreen may ask for confirmation
the first time it runs.

This is the portable build: unpack it anywhere and run lamina.exe. The release
also carries Lamina-<version>-windows-x64-setup.exe, which installs Lamina
into %LOCALAPPDATA%\Programs\Lamina with Start-menu shortcuts and an
uninstaller, without administrator rights.

Licences
--------

Lamina is licensed under MIT OR Apache-2.0; see LICENSE-MIT and LICENSE-APACHE.
THIRD-PARTY-NOTICES.md lists every bundled third-party component and its
licence terms.
