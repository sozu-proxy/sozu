#!/bin/bash

set -e

# get required packages
echo 'installing build dependencies ...'
sudo dnf builddep sozu.spec

# define internal variables
arch=$(uname -m)
output_dir=$(pwd)
rpmbuild_root=$(mktemp -d)
version=$(grep -P "^Version:" sozu.spec | cut -f2)

# create RPM build environment
echo "building in ${rpmbuild_root} ..."
mkdir -p "${rpmbuild_root}"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS/tmp}

# copy sources
cp -p sozu.spec "${rpmbuild_root}"/SPECS
cd ..
tar -czf "${rpmbuild_root}/SOURCES/${version}.tar.gz"  --transform "s,^,sozu-${version}/," ../*

cd "${rpmbuild_root}"
pwd
rpmbuild -bb --define "_topdir ${rpmbuild_root}" SPECS/sozu.spec

cp "${rpmbuild_root}"/RPMS/"${arch}"/*.rpm "${output_dir}"
rm -rf "${rpmbuild_root}"
set +e
