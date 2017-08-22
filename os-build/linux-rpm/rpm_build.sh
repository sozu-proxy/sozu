#!/bin/bash

set -e
cd `dirname $BASH_SOURCE`

# get required packages
echo 'installing packages'
sudo dnf install openssl-devel m4 selinux-policy-devel

# define internal variables
arch=`uname -m`
output_dir=`pwd`
rpmbuild_root=`mktemp -d`
version=`grep -P "^Version:" linux-rpm/sozu.spec | cut -f2`

# create RPM build environment
echo "building in $rpmbuild_root"
mkdir -p $rpmbuild_root/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS/tmp}

# copy sources
cp -p linux-rpm/sozu.spec $rpmbuild_root/SPECS
cd ..
tar -czf $rpmbuild_root/SOURCES/$version.tar.gz  --transform 's,^,sozu-0.1.1/,' ./*

cd $rpmbuild_root
echo `pwd`
rpmbuild -bb --define "_topdir ${rpmbuild_root}" SPECS/sozu.spec

cp  ${rpmbuild_root}/RPMS/${arch}/*.rpm $output_dir

rm -rf $rpmbuild_root
set +e
