Name:           zpane
Version:        0.4.0
Release:        1%{?dist}
Summary:        3-pane Miller column command, subcommand, parameter & variable explorer
License:        MIT
URL:            https://github.com/zenthora/zpane
BuildArch:      x86_64
%global __os_install_post %{nil}
%global debug_package %{nil}

%description
zpane brings the spatial orientation of 3-column Miller file managers
(yazi, ranger, lf) to interactive shell command construction across
PowerShell, Bash, Zsh, Fish, and Termux.

%prep
# Binary pre-built via cargo

%build
# Binary pre-built via cargo

%install
install -Dm0755 "%{_sourcedir}/zpane" "%{buildroot}%{_bindir}/zpane"
install -Dm0644 "%{_sourcedir}/README.md" "%{buildroot}%{_datadir}/doc/zpane/README.md"
install -Dm0644 "%{_sourcedir}/LICENSE" "%{buildroot}%{_datadir}/licenses/zpane/LICENSE"

%files
%{_bindir}/zpane
%doc %{_datadir}/doc/zpane/README.md
%license %{_datadir}/licenses/zpane/LICENSE

%changelog
* Sat Nov 14 2026 zenthora <105382125+zenthora@users.noreply.github.com> - 0.4.0-1
- Release zpane 0.4.0 (multi-level subcommand sliding, shell init, cache freshness, inspector overlay)

* Thu Nov 12 2026 zenthora <105382125+zenthora@users.noreply.github.com> - 0.1.0-1
- Initial release of zpane for Fedora / RHEL / openSUSE
