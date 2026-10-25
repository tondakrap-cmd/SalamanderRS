# RPM for AlmaLinux / RHEL 9 and 10 (GNOME), built from a prebuilt binary by
# packaging/rpm/build-rpm.sh. libsodium is linked statically (EPEL 10 has another soname),
# so only packages from BaseOS and AppStream are required.

%global debug_package %{nil}
%global __os_install_post %{nil}

Name:           salamander-rs
Version:        %{pkg_version}
Release:        1
Summary:        Dual-pane file manager inspired by Open Salamander
Summary(cs):    Dvoupanelový správce souborů inspirovaný Open Salamanderem
License:        GPL-3.0-or-later
URL:            https://github.com/tondakrap-cmd/SalamanderRS
ExclusiveArch:  x86_64
AutoReqProv:    no

Requires:       glibc >= 2.34
Requires:       gtk4 >= 4.12
Requires:       glib2 >= 2.68
Requires:       libarchive >= 3.5
Requires:       openssh-clients
Requires:       hicolor-icon-theme
Provides:       salamander-rs = %{version}-%{release}

%description
Salamander RS is a dual-pane file manager for the GNOME desktop inspired by
Open Salamander: keyboard-driven panels, copy/move with conflict handling,
archives (zip, tar, 7z, ...), POSIX permissions, ACLs and extended attributes,
and remote servers over SSH/SFTP with an encrypted password vault and an
audit log.

%description -l cs
Salamander RS je dvoupanelový správce souborů pro GNOME inspirovaný Open
Salamanderem: ovládání klávesnicí, kopírování a přesun s řešením konfliktů,
archivy (zip, tar, 7z, …), oprávnění, ACL a rozšířené atributy a vzdálené
servery přes SSH/SFTP se šifrovaným trezorem hesel a auditním logem.

%install
install -Dm0755 %{_sourcedir}/salamander-rs %{buildroot}%{_bindir}/salamander-rs
install -Dm0644 %{_sourcedir}/cz.k4.salamander-rs.desktop %{buildroot}%{_datadir}/applications/cz.k4.salamander-rs.desktop
install -Dm0644 %{_sourcedir}/salamander-rs.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/salamander-rs.svg
install -Dm0644 %{_sourcedir}/LICENSE %{buildroot}%{_datadir}/licenses/%{name}/LICENSE
install -Dm0644 %{_sourcedir}/README.md %{buildroot}%{_docdir}/%{name}/README.md

%files
%{_bindir}/salamander-rs
%{_datadir}/applications/cz.k4.salamander-rs.desktop
%{_datadir}/icons/hicolor/scalable/apps/salamander-rs.svg
%license %{_datadir}/licenses/%{name}/LICENSE
%doc %{_docdir}/%{name}/README.md

%changelog
* Sun Oct 25 2026 Salamander RS contributors - 0.5.1-1
- First RPM build for AlmaLinux 9 and 10
