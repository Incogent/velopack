# Incogent Velopack Fork

This repository is a pinned packaging dependency for Blackbird. Branch `blackbird-1.2.0` is based
on upstream tag `1.2.0` (`f2edcbcafb81da5b3c884aaea330e225ad91d8b6`) and is not intended to
track every upstream release.

The fork changes only the Windows MSI experience needed by Blackbird:

- show an installation-directory page;
- map the MSI banner and background-logo options to their correct WiX image slots;
- default per-machine installs to `Publisher\Product` under Program Files;
- expose the standard Apps & Features maintenance entry;
- offer Repair and Uninstall without an irrelevant Change option; and
- warn that Blackbird's Library and user-created AppData are preserved during uninstall.

The installation-directory page relies on Windows Installer's native `SetTargetPath` validation.
Velopack 1.2.0's Rust path validator reads `WIXUI_INSTALLDIR` as a literal path even though WiX
stores the indirect directory-property name there, which incorrectly rejects valid directories.

Update from upstream only for a concrete Blackbird requirement, such as a relevant security fix,
package-format or updater compatibility, signing support, platform support, or a blocking defect.
Validate clean install, in-app update, repair, and uninstall before adopting any new upstream base.
