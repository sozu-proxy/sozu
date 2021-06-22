#!/bin/sh

BINDIR="/usr/local/bin"
SYSCONFDIR="/etc"
DATADIR="/var/lib/sozu"
RUNDIR="/run"

substitute() {
    sed -e "s:__BINDIR__:${BINDIR}:"         \
        -e "s:__SYSCONFDIR__:${SYSCONFDIR}:" \
        -e "s:__DATADIR__:${DATADIR}:"       \
        -e "s:__RUNDIR__:${RUNDIR}:"         \
        -e "s:__SOZU_USER__:root:"           \
        -e "s:__SOZU_GROUP__:root:"          \
        "${1}" > "${2}"
}

mkdir -p generated
scriptdir="$(dirname "${0}")"

substitute "${scriptdir}"/../systemd/sozu.service.in generated/sozu.service
substitute "${scriptdir}"/../systemd/sozu.conf.in generated/sozu.conf
substitute "${scriptdir}"/../config.toml.in generated/config.toml
