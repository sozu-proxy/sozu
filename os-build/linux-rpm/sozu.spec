%global	sozu_user	sozu

Summary:	A lightweight, fast, always-up reverse proxy server.
Name:		sozu
Version:	0.14.1
Release:	1%{?dist}
Epoch:		1
License:	AGPL-3.0
Group:		System Environment/Daemons
URL:		https://github.com/sozu-proxy/sozu

Source0:	https://github.com/sozu-proxy/sozu/archive/%{version}.tar.gz

BuildRequires:	m4
BuildRequires:	selinux-policy-devel
# BuildRequires: 	rust
BuildRoot:	%{_tmppath}/%{name}-%{version}-%{release}-root


%description
%{summary}

%package ctl
Group:		System Environment/Daemons
Summary:	Command-line control tool for the sozu proxy server.
Requires:	sozu

%description ctl
%{summary}

%prep

%if "%{_build_mode}" == "RELEASE"
%setup -n %{name}-%{version}
%else
%setup -q
%endif

%build
cargo build --release --all --locked

%install
rm -rf %{buildroot}

#service config file
mkdir -p %{buildroot}%{_sysconfdir}/%{name}/
#cp -p bin/config.toml %{buildroot}%{_sysconfdir}/%{name}/%{name}.toml
m4 -D __DATADIR__=%{_datadir}/sozu -D __RUNDIR__=%{_localstatedir}/run os-build/config.toml.in > %{buildroot}%{_sysconfdir}/%{name}/%{name}.toml

#service binary file
mkdir -p %{buildroot}%{_bindir}/
cp -p target/release/sozu %{buildroot}%{_bindir}/ 

#serverassets
mkdir -p %{buildroot}%{_datadir}/sozu/{ssl,html}
cp -p lib/assets/{certificate.pem,key.pem,certificate_chain.pem} %{buildroot}%{_datadir}/sozu/ssl
cp -p lib/assets/{404.html,503.html} %{buildroot}%{_datadir}/sozu/html

#service running directory
mkdir -p %{buildroot}%{_localstatedir}/run/sozu
touch %{buildroot}%{_localstatedir}/run/sozu/state.json

#control binary file
cp -p target/release/sozuctl %{buildroot}%{_bindir}/
#control alias
mkdir -p %{buildroot}%{_sysconfdir}/profile.d/
echo 'alias sozuctl="`which sozuctl` --config %{_sysconfdir}/%{name}/%{name}.toml"' > %{buildroot}%{_sysconfdir}/profile.d/%{name}.sh

# service file - no check for _libdir as it's a systemd constant.
mkdir -p %{buildroot}/usr/lib/systemd/system/
m4 -D __BINDIR__=%{_bindir} -D __SYSCONFDIR__=%{_sysconfdir} os-build/systemd/sozu.service.in > %{buildroot}%{_localstatedir}/run/sozu/sozu.service

# selinux
cd os-build/selinux
m4 -D __BIN_DIR__=%{_bindir} -D __STATE_DIR__=%{_localstatedir}/run/sozu sozu.fc.in > sozu.fc
make -f /usr/share/selinux/devel/Makefile
bzip2 -z sozu.pp

mkdir -p %{buildroot}%{_datadir}/selinux/packages
cp -p sozu.pp.bz2 %{buildroot}%{_datadir}/selinux/packages


%clean
rm -rf %{buildroot}


%post
semodule -i %{_datadir}/selinux/packages/sozu.pp.bz2

# selinux initial set file types
chcon -t sozu_unit_file_t %{_localstatedir}/run/sozu/sozu.service
chcon -t sozu_exec_t %{_bindir}/sozu*
chcon -R -t sozu_var_run_t %{_localstatedir}/run/sozu/

%postun
semodule -r sozu

%files
%defattr(-,root,root,-)
%config(noreplace) %{_sysconfdir}/%{name}/%{name}.toml
%{_bindir}/sozu
%{_localstatedir}/run/sozu
%{_datadir}/sozu
%{_datadir}/selinux/packages/sozu.pp.bz2

%files ctl
%{_bindir}/sozuctl
%{_sysconfdir}/profile.d/%{name}.sh

%changelog
* Sat Jul 31 2021 Igal Alkon <igal.alkon@versatile.ai>
* Mon May 15 2017 Philip Woolford <woolford.philip@gmail.com> 0.1-1
