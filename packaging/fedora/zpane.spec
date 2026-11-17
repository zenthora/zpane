Name:           zpane
Version:        0.5.2
Release:        1%{?dist}
Summary:        3-pane Miller column command, subcommand, parameter & variable explorer
License:        MIT
URL:            https://github.com/zenthora/zpane
BuildArch:      x86_64
%global __os_install_post %{nil}
%global debug_package %{nil}
%global use_source_date_epoch_as_buildtime 1
%global clamp_mtime_to_source_date_epoch 1

%description
zpane brings the spatial orientation of 3-column Miller file managers
(yazi, ranger, lf) to interactive shell command construction across
PowerShell, Bash, Zsh, Fish, and Termux.

%prep
# Binary pre-built via cargo

%build
# Binary pre-built via cargo

%install
umask 022
install -d -m0755 \
    "%{buildroot}%{_bindir}" \
    "%{buildroot}%{_datadir}/doc/zpane" \
    "%{buildroot}%{_datadir}/licenses/zpane"
install -Dm0755 "%{_sourcedir}/zpane" "%{buildroot}%{_bindir}/zpane"
install -Dm0644 "%{_sourcedir}/README.md" "%{buildroot}%{_datadir}/doc/zpane/README.md"
install -Dm0644 "%{_sourcedir}/LICENSE" "%{buildroot}%{_datadir}/licenses/zpane/LICENSE"

%files
%defattr(-,root,root,-)
%attr(0755,root,root) %{_bindir}/zpane
%dir %attr(0755,root,root) %{_datadir}/doc/zpane
%doc %attr(0644,root,root) %{_datadir}/doc/zpane/README.md
%dir %attr(0755,root,root) %{_datadir}/licenses/zpane
%license %attr(0644,root,root) %{_datadir}/licenses/zpane/LICENSE

%changelog
* Tue Nov 17 2026 zenthora <105382125+zenthora@users.noreply.github.com> - 0.5.2-1
- Release zpane 0.5.2 (preserve Pane 2 selection during background catalog refresh)

* Sun Nov 15 2026 zenthora <105382125+zenthora@users.noreply.github.com> - 0.5.1-1
- Release zpane 0.5.1 (README crates.io installation instructions and maintainer metadata cleanup)

* Sun Nov 15 2026 zenthora <105382125+zenthora@users.noreply.github.com> - 0.5.0-1
- Release zpane 0.5.0 (non-interactive inspect command, --json output, discovery/session/UI optimizations, security hardening)

* Sat Nov 14 2026 zenthora <105382125+zenthora@users.noreply.github.com> - 0.4.1-1
- Release zpane 0.4.1 (discovery, session/model, UI/CLI/packaging optimization and security hardening)

* Sat Nov 14 2026 zenthora <105382125+zenthora@users.noreply.github.com> - 0.4.0-1
- Release zpane 0.4.0 (multi-level subcommand sliding, shell init, cache freshness, inspector overlay)

* Thu Nov 12 2026 zenthora <105382125+zenthora@users.noreply.github.com> - 0.1.0-1
- Initial release of zpane for Fedora / RHEL / openSUSE
