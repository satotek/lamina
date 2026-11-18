Strata
======

A lightweight git client.

Run strata.exe to open the welcome screen, or pass a path to open the
repository containing it:

    strata.exe C:\src\strata

Strata runs the git command line for every write and reads repositories
directly, so Git for Windows has to be installed and on PATH.

Settings live in %APPDATA%\Strata.

This build is not code signed, so Windows SmartScreen may ask for confirmation
the first time it runs.

This is the portable build: unpack it anywhere and run strata.exe. The release
also carries Strata-<version>-windows-x64-setup.exe, which installs Strata
into %LOCALAPPDATA%\Programs\Strata with Start-menu shortcuts and an
uninstaller, without administrator rights.

Licences
--------

Strata is licensed under MIT OR Apache-2.0; see LICENSE-MIT and LICENSE-APACHE.
THIRD-PARTY-NOTICES.md lists every bundled third-party component and its
licence terms.
