# Tasks for this machine (the kernel's build and tests: CLAUDE.md, `cd kernel && cargo run`).

# Install the UEFI image as a systemd-boot entry on the ESP and boot it next (asks first)
install:
    scripts/efi-install.sh

# Remove what `install` put on the ESP
uninstall:
    scripts/efi-install.sh --uninstall
