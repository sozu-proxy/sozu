# For more documentation, See:
# - https://developer.fedoraproject.org/deployment/rpm/about.html
# - https://docs.fedoraproject.org/en-US/packaging-guidelines/Scriptlets/#_systemd

%global	sozu_user	sozu

Summary:	A lightweight, fast, always-up reverse proxy server.
Name:		sozu
Version:	0.14.3
Release:	1%{?dist}
Epoch:		1
License:	AGPL-3.0
Group:		System Environment/Daemons
URL:		https://github.com/sozu-proxy/sozu

Source0:	https://github.com/sozu-proxy/sozu/archive/%{version}.tar.gz

BuildRequires: m4
BuildRequires: selinux-policy-devel
BuildRequires: systemd
# BuildRequires: rust
BuildRoot:	%{_tmppath}/%{name}-%{version}-%{release}-root

%description
%{summary}

%prep

%if "%{_build_mode}" == "RELEASE"
%setup -n %{name}-%{version}
%else
%setup -q
%endif

%build
%if "%{_build_mode}" == "RELEASE"
cargo build --release -p sozu --locked
%else
cargo build -p sozu --locked
%endif

%install
rm -rf %{buildroot}

# service config file
mkdir -p %{buildroot}%{_sysconfdir}/%{name} %{buildroot}%{_unitdir}/
cp os-build/config.toml %{buildroot}%{_sysconfdir}/%{name}/config.toml
cp os-build/systemd/%{name}.service %{buildroot}%{_unitdir}/%{name}.service
cp os-build/systemd/%{name}@.service %{buildroot}%{_unitdir}/%{name}@.service

#service binary file
mkdir -p %{buildroot}%{_bindir}/
%if "%{_build_mode}" == "RELEASE"
cp -p target/release/%{name} %{buildroot}%{_bindir}/
%else
cp -p target/debug/%{name} %{buildroot}%{_bindir}/
%endif

# server assets
mkdir -p %{buildroot}%{_datadir}/sozu/{pki,html}
cp -p lib/assets/{certificate.pem,key.pem,certificate_chain.pem} %{buildroot}%{_datadir}/%{name}/pki
cp -p lib/assets/{404.html,503.html} %{buildroot}%{_datadir}/%{name}/html

#service running directory
mkdir -p %{buildroot}%{_localstatedir}/var/lib/%{name}
touch %{buildroot}%{_localstatedir}/var/lib/%{name}/state.json

# selinux
cd os-build/selinux
make -f /usr/share/selinux/devel/Makefile
bzip2 -z %{name}.pp

mkdir -p %{buildroot}%{_datadir}/selinux/packages
cp -p %{name}.pp.bz2 %{buildroot}%{_datadir}/selinux/packages

%clean
rm -rf %{buildroot}

%post
semodule -i %{_datadir}/selinux/packages/%{name}.pp.bz2

# selinux initial set file types
chcon -t %{name}_unit_file_t %{_localstatedir}/run/%{name}/%{name}.service
chcon -t %{name}_unit_file_t %{_localstatedir}/run/%{name}/%{name}@.service
chcon -t %{name}_exec_t %{_bindir}/%{name}*
chcon -R -t %{name}_var_run_t %{_localstatedir}/var/lib/%{name}/

%postun
semodule -r %{name}

%files
%defattr(-,root,root,-)
%config(noreplace) %{_sysconfdir}/%{name}/config.toml
%{_bindir}/%{name}
%{_localstatedir}/run/%{name}
%{_localstatedir}/var/lib/%{name}
%{_datadir}/%{name}
%{_datadir}/selinux/packages/%{name}.pp.bz2
%{_unitdir}/%{name}.service
%{_unitdir}/%{name}@.service

%doc CHANGELOG.md CONTRIBUTING.md README.md RELEASE.md doc/architecture.md doc/configure.md doc/configure_cli.md doc/debugging_strategies.md doc/design_motivation.md doc/getting_started.md doc/how_to_use.md doc/lexicon.md doc/lifetime_of_a_session.md doc/managing_workers.md doc/recipes.md doc/tools_libraries.md doc/why_you_should_use.md
%license LICENSE

%changelog
* Mon May 22 2023 Florentin Dubois <florentin.dubois@clever-cloud.com>
- release 0.14.3
* Mon Jan 23 2023 Florentin Dubois <florentin.dubois@clever-cloud.com>
- Update packaging
* Sat Jul 31 2021 Igal Alkon <igal.alkon@versatile.ai>
* Mon May 15 2017 Philip Woolford <woolford.philip@gmail.com> 0.1-1
