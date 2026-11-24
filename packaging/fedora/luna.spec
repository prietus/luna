# Rust packaging guidelines:
# https://docs.fedoraproject.org/en-US/packaging-guidelines/Rust/
#
# The crates luna uses (gtk4 0.11, libadwaita 0.9, ksni...) are not all in
# Fedora, so they are vendored: Source1 is made with
#   cargo vendor && tar cJf luna-%%{version}-vendor.tar.xz vendor/

Name:           luna
Version:        0.1.1
Release:        1%{?dist}
Summary:        GTK4 client for Borg backup repositories

SourceLicense:  MIT
# Licenses of luna and the vendored crates (%%cargo_license_summary).
License:        MIT AND Apache-2.0 AND BSD-3-Clause AND CDLA-Permissive-2.0 AND ISC AND Unicode-3.0 AND Unlicense AND (Apache-2.0 OR MIT) AND (0BSD OR MIT OR Apache-2.0) AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT) AND (MIT OR Apache-2.0 OR LGPL-2.1-or-later) AND (MIT OR Zlib OR Apache-2.0) AND (Unlicense OR MIT)
URL:            https://github.com/prietus/luna
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
# The crates are the same as in 0.1.0, so its vendor tarball still applies.
Source1:        %{name}-0.1.0-vendor.tar.xz

ExclusiveArch:  %{rust_arches}

BuildRequires:  cargo-rpm-macros >= 26
BuildRequires:  pkgconfig(gtk4) >= 4.14
BuildRequires:  pkgconfig(libadwaita-1) >= 1.5
BuildRequires:  pkgconfig(gtk4-layer-shell-0)
BuildRequires:  pkgconfig(dbus-1)
BuildRequires:  desktop-file-utils

Requires:       hicolor-icon-theme
Requires:       borgbackup
# secret-tool: passphrases live in the keyring.
Requires:       libsecret
Recommends:     gtk4-layer-shell
Recommends:     libnotify
Recommends:     openssh-clients
Recommends:     fuse3
Recommends:     xdg-utils

%description
Luna is a native Linux client for Borg backup repositories, written in
Rust with GTK4/libadwaita. It drives the borg command line tool, schedules
backups as systemd user timers, has a tray icon and panels for BorgBase
and self-hosted BorgBox servers.


%prep
%autosetup -n %{name}-%{version} -a1
%cargo_prep -v vendor


%build
%cargo_build
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies
%{cargo_vendor_manifest}


%install
install -Dpm 0755 target/rpm/%{name} %{buildroot}%{_bindir}/%{name}
install -Dpm 0644 data/us.priet.Luna.desktop \
  %{buildroot}%{_datadir}/applications/us.priet.Luna.desktop
for s in 16 32 128 256 512; do
  install -Dpm 0644 data/hicolor/${s}x${s}/apps/us.priet.Luna.png \
    %{buildroot}%{_datadir}/icons/hicolor/${s}x${s}/apps/us.priet.Luna.png
done


%check
desktop-file-validate %{buildroot}%{_datadir}/applications/us.priet.Luna.desktop
%cargo_test


%files
%license LICENSE LICENSE.dependencies cargo-vendor.txt
%doc README.md
%{_bindir}/%{name}
%{_datadir}/applications/us.priet.Luna.desktop
%{_datadir}/icons/hicolor/*/apps/us.priet.Luna.png


%changelog
* Tue Nov 24 2026 Carlos Prieto Ortiz <prietus@live.com> - 0.1.1-1
- Update to 0.1.1
- New app and tray icon: crescent moon with a backup disk

* Tue Nov 24 2026 Carlos Prieto Ortiz <prietus@live.com> - 0.1.0^20261124git3518ca1-1
- Update to 3518ca1
- luna --background waits for the tray host instead of falling back to the window
- Colour tray icon that shows the backup state

* Mon Nov 23 2026 Carlos Prieto Ortiz <prietus@live.com> - 0.1.0^20261116git44a4e9f-1
- First Fedora package
