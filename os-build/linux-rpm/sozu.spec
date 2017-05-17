%global	sozu_user	sozu

Summary:	A runtime-configurable HTTP/S reverse proxy.
Name:		sozu
Version:	0.1.1
Release:	1%{?dist}
Epoch:		1
License:	AGPL-3.0
Group:		System Environment/Daemons
URL:		https://github.com/sozu-proxy/sozu

Source0:	https://github.com/sozu-proxy/sozu/archive/%{version}.tar.gz

Requires:	openssl >= 1.0.1

BuildRequires: 	openssl-devel >= 1.0.1
BuildRequires:	m4
# BuildRequires: 	rust
BuildRoot:	%{_tmppath}/%{name}-%{version}-%{release}-root


%description
%{summary}

%package ctl
Group:		System Environment/Daemons
Summary:	Sozu command line binary.
Requires:	sozu

%description ctl
%{summary}

%prep
%setup -q

%build
cd bin/

#build the service binary
cargo build --release

cd ../ctl

#build the control binary
cargo build --release

%install
rm -rf %{buildroot}

#service config file
mkdir -p %{buildroot}%{_sysconfdir}/%{name}/
#cp -p bin/config.toml %{buildroot}%{_sysconfdir}/%{name}/%{name}.toml
m4 -D __DATA_DIR__=%{_datadir}/sozu/ -D __STATE_DIR__=%{_localstatedir}/run/sozu os-build/config.toml.in > %{buildroot}%{_sysconfdir}/%{name}/%{name}.toml

#service binary file
mkdir -p %{buildroot}%{_bindir}/
cp -p target/release/sozu %{buildroot}%{_bindir}/ 

#serverassets
mkdir -p %{buildroot}%{_datadir}/sozu/{ssl,html}
cp -p lib/assets/{certificate.pem,key.pem,certificate_chain.pem} %{buildroot}%{_datadir}/sozu/ssl
cp -p lib/assets/{404.html,503.html}

#service running directory
mkdir -p %{buildroot}%{_localstatedir}/run/sozu

#control binary file
cp -p target/release/sozuctl %{buildroot}%{_bindir}/
#control alias
mkdir -p %{buildroot}%{_sysconfdir}/profile.d/
echo 'alias sozuctl="`which sozuctl` --config %{_sysconfdir}/%{name}/%{name}.toml"' > %{buildroot}%{_sysconfdir}/profile.d/%{name}.sh

%clean
rm -rf %{buildroot}


%files
%defattr(-,root,root,-)
%config(noreplace) %{_sysconfdir}/%{name}/%{name}.toml
%{_bindir}/sozu
%{_localstatedir}/run/sozu
%{_datadir}/sozu

%files ctl
%{_bindir}/sozuctl
%{_sysconfdir}/profile.d/%{name}.sh

%changelog
* Mon May 15 2017 Philip Woolford <woolford.philip@gmail.com> 0.1-1
- First RPM build
