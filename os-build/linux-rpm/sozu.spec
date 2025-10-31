%global	sozu_user	sozu

Summary:	A lightweight, fast, always-up reverse proxy server.
Name:		sozu
Version:	1.1.0
Release:	1%{?dist}
Epoch:		1
License:	AGPL-3.0
Group:		System Environment/Daemons
URL:		https://github.com/sozu-proxy/sozu

Source0:	https://github.com/sozu-proxy/sozu/archive/%{version}.tar.gz

BuildRequires: m4
BuildRequires: selinux-policy-devel
BuildRequires: systemd
BuildRequires: protobuf
BuildRequires: rust
BuildRequires: cargo
BuildRequires: protobuf-compiler
BuildRequires: gcc

BuildRoot:	%{_tmppath}/%{name}-%{version}-%{release}-root

%description
%{summary}

%prep
%setup -n %{name}-%{version}

%build
cargo build --release

%install
rm -rf %{buildroot}

# service config file
mkdir -p %{buildroot}%{_sysconfdir}/%{name} %{buildroot}%{_unitdir}/
cp os-build/config.toml %{buildroot}%{_sysconfdir}/%{name}/config.toml
cp os-build/systemd/%{name}.service %{buildroot}%{_unitdir}/%{name}.service
cp os-build/systemd/%{name}@.service %{buildroot}%{_unitdir}/%{name}@.service

#service binary file
mkdir -p %{buildroot}%{_bindir}/
cp -p target/release/%{name} %{buildroot}%{_bindir}/

# server assets
mkdir -p %{buildroot}%{_datadir}/sozu/{pki,html}
cp -p lib/assets/{certificate.pem,key.pem,certificate_chain.pem} %{buildroot}%{_datadir}/%{name}/pki
cp -p command/assets/custom_404.html %{buildroot}%{_datadir}/%{name}/html/404.html
cp -p command/assets/custom_503.html %{buildroot}%{_datadir}/%{name}/html/503.html

#service running directory
mkdir -p %{buildroot}%{_localstatedir}/var/lib/%{name}
touch %{buildroot}%{_localstatedir}/var/lib/%{name}/state.json

# runtime directory
mkdir -p %{buildroot}%{_localstatedir}/run/%{name}

# selinux
cd os-build/selinux
make -f /usr/share/selinux/devel/Makefile
bzip2 -z %{name}.pp

mkdir -p %{buildroot}%{_datadir}/selinux/packages
cp -p %{name}.pp.bz2 %{buildroot}%{_datadir}/selinux/packages

# Return to source root for license and documentation installation
cd ../..

# Install license and documentation
install -d %{buildroot}%{_licensedir}/%{name}
install -m 644 LICENSE %{buildroot}%{_licensedir}/%{name}/LICENSE

install -d %{buildroot}%{_docdir}/%{name}
install -m 644 README.md %{buildroot}%{_docdir}/%{name}/
install -m 644 CHANGELOG.md %{buildroot}%{_docdir}/%{name}/
install -m 644 CONTRIBUTING.md %{buildroot}%{_docdir}/%{name}/
install -m 644 RELEASE.md %{buildroot}%{_docdir}/%{name}/
install -d %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/architecture.md %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/configure.md %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/configure_cli.md %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/debugging_strategies.md %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/design_motivation.md %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/getting_started.md %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/how_to_use.md %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/lexicon.md %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/lifetime_of_a_session.md %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/recipes.md %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/README.md %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/tools_libraries.md %{buildroot}%{_docdir}/%{name}/doc
install -m 644 doc/why_you_should_use.md %{buildroot}%{_docdir}/%{name}/doc

%clean
rm -rf %{buildroot}

%post
semodule -i %{_datadir}/selinux/packages/%{name}.pp.bz2

# selinux initial set file types
chcon -t %{name}_unit_file_t %{_unitdir}/%{name}.service
chcon -t %{name}_unit_file_t %{_unitdir}/%{name}@.service
chcon -t %{name}_exec_t %{_bindir}/%{name}*
chcon -R -t %{name}_var_run_t %{_sharedstatedir}/%{name}/
chcon -R -t %{name}_var_run_t %{_rundir}/%{name}/

%postun
semodule -r %{name}

%files
%defattr(-,root,root,-)
%config(noreplace) %{_sysconfdir}/%{name}/config.toml
%{_bindir}/%{name}
%{_rundir}/%{name}
%{_sharedstatedir}/%{name}
%{_datadir}/%{name}
%{_datadir}/selinux/packages/%{name}.pp.bz2
%{_unitdir}/%{name}.service
%{_unitdir}/%{name}@.service

%doc
%doc %{_docdir}/%{name}/README.md
%doc %{_docdir}/%{name}/CHANGELOG.md
%doc %{_docdir}/%{name}/CONTRIBUTING.md
%doc %{_docdir}/%{name}/RELEASE.md
%doc %{_docdir}/%{name}/doc/architecture.md
%doc %{_docdir}/%{name}/doc/configure.md
%doc %{_docdir}/%{name}/doc/configure_cli.md
%doc %{_docdir}/%{name}/doc/debugging_strategies.md
%doc %{_docdir}/%{name}/doc/design_motivation.md
%doc %{_docdir}/%{name}/doc/getting_started.md
%doc %{_docdir}/%{name}/doc/how_to_use.md
%doc %{_docdir}/%{name}/doc/lexicon.md
%doc %{_docdir}/%{name}/doc/lifetime_of_a_session.md
%doc %{_docdir}/%{name}/doc/recipes.md
%doc %{_docdir}/%{name}/doc/README.md
%doc %{_docdir}/%{name}/doc/tools_libraries.md
%doc %{_docdir}/%{name}/doc/why_you_should_use.md

%license %{_licensedir}/%{name}/LICENSE

%changelog
* Wed Oct 29 2025 Florentin Dubois <florentin.dubois@clever-cloud.com>
- release: v1.1.0
* Thu Dec 05 2024 Eloi Démolis <eloi.demolis@clever-cloud.com>
- release 1.0.6
* Mon Oct 14 2024 Florentin Dubois <florentin.dubois@clever-cloud.com>
- release 1.0.5
* Thu Jul 25 2024 Emmanuel Bosquet <bjokac@gmail.com>
- release 1.0.4
* Wed Jul 17 2024 Emmanuel Bosquet <bjokac@gmail.com>
- release 1.0.3
* Wed May 29 2024 Florentin Dubois <florentin.dubois@clever-cloud.com>
- release 1.0.1
* Tue Apr 16 2024 Florentin Dubois <florentin.dubois@clever-cloud.com>
- release 1.0.0
* Fri Apr 05 2024 Florentin Dubois <florentin.dubois@clever-cloud.com>
- release 1.0.0-rc.2
* Tue Mar 19 2024 Florentin Dubois <florentin.dubois@clever-cloud.com>
- release 1.0.0-rc.1
* Wed Aug 09 2023 Florentin Dubois <florentin.dubois@clever-cloud.com>
- release 0.15.3
* Tue Jul 11 2023 Florentin Dubois <florentin.dubois@clever-cloud.com>
- release 0.15.1
* Mon May 22 2023 Florentin Dubois <florentin.dubois@clever-cloud.com>
- release 0.14.3
* Mon Jan 23 2023 Florentin Dubois <florentin.dubois@clever-cloud.com>
- Update packaging
* Sat Jul 31 2021 Igal Alkon <igal.alkon@versatile.ai>
- 0.13.0-1
* Mon May 15 2017 Philip Woolford <woolford.philip@gmail.com>
- 0.1-1
