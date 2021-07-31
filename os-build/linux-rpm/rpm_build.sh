#!/bin/bash

set -e

release=0
args=("release")

options=$(getopt \
    --longoptions "$(printf "%s::," "${args[@]}")" \
    --name "$(basename "$0")" \
    --options "" \
    -- "$@"
)

if [[ "${options}" == *"--release"* ]]; then
      release=1
fi

spec_file="sozu.spec"

# get required packages
echo 'installing build dependencies ...'
sudo dnf builddep "${spec_file}"

# define internal variables
arch=$(uname -m)
output_dir=$(pwd)
rpmbuild_root=$(mktemp -d)
version=$(grep -P "^Version:" sozu.spec | cut -f2)

# create RPM build environment
echo "building in ${rpmbuild_root} ..."
mkdir -p "${rpmbuild_root}"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS/tmp}

# copy sources
cp -p "${spec_file}" "${rpmbuild_root}"/SPECS

# check if --release option was set, and build accordingly
if [ "${release}" -eq "1" ]; then
  cd "${rpmbuild_root}"
  pwd
  rpmbuild -bb \
    --define "_build_mode RELEASE" \
    --define "_topdir ${rpmbuild_root}" \
    --undefine=_disable_source_fetch \
    --define "debug_package %{nil}" \
    SPECS/"${spec_file}"
else
  cd ..
  tar -czf "${rpmbuild_root}/SOURCES/${version}.tar.gz"  --transform "s,^,sozu-${version}/," ../*
  cd "${rpmbuild_root}"
  pwd
  rpmbuild -bb \
  --define "_build_mode %{nil}" \
  --define "_topdir ${rpmbuild_root}" \
  SPECS/"${spec_file}"
fi

# copy *.rpm files from build root to output directory (current)
cp "${rpmbuild_root}"/RPMS/"${arch}"/*.rpm "${output_dir}"
rm -rf "${rpmbuild_root}"
set +e
