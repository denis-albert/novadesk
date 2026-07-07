# Spec RPM NovaDesk — empaquette le bundle Flutter déjà construit.
# On ne compile pas ici : le bundle est fourni via --define "stagedir <chemin>".
# Build : voir build-rpm.sh (rpmbuild -bb).

Name:           novadesk
Version:        0.1.0
Release:        1%{?dist}
Summary:        Bureau à distance NovaDesk (client)

License:        LicenseRef-proprietary
URL:            https://novadesk.example
BuildArch:      x86_64

Requires:       gtk3
Requires:       pipewire
Requires:       libX11
Recommends:     xdg-desktop-portal
BuildRequires:  systemd-rpm-macros
%{?systemd_requires}

%description
NovaDesk est une solution de bureau à distance : cœur Rust (réseau, crypto,
média chiffré de bout en bout) piloté par une interface Flutter. Ce paquet
installe le client et une unité systemd « novadesk.service » (accès non
surveillé, désactivée par défaut). Capture Wayland via xdg-desktop-portal +
PipeWire.

# Pas de %prep/%build : la source est un bundle déjà compilé (stagedir).

%install
rm -rf %{buildroot}
install -d %{buildroot}%{_prefix}/lib/novadesk
install -d %{buildroot}%{_bindir}
install -d %{buildroot}%{_datadir}/applications
install -d %{buildroot}%{_unitdir}
cp -R %{stagedir}/. %{buildroot}%{_prefix}/lib/novadesk/
ln -sf ../lib/novadesk/novadesk %{buildroot}%{_bindir}/novadesk
install -m 0644 %{desktopfile} %{buildroot}%{_datadir}/applications/novadesk.desktop
install -m 0644 %{unitfile} %{buildroot}%{_unitdir}/novadesk.service

%files
%{_prefix}/lib/novadesk
%{_bindir}/novadesk
%{_datadir}/applications/novadesk.desktop
%{_unitdir}/novadesk.service

%post
%systemd_post novadesk.service

%preun
%systemd_preun novadesk.service

%postun
%systemd_postun_with_restart novadesk.service

%changelog
* Sat Jul 04 2026 Équipe NovaDesk <contact@novadesk.example> - 0.1.0-1
- Paquet initial (squelette de packaging).
