#!/bin/sh
accepted=${0##*/}
accepted=${accepted#candidate-}
accepted=${accepted%.sh}
printf '%s\n' "$1 $2 $3" >> "${0%/*}/asked-$accepted.log"
[ "$1" = upgrade-probe ] && [ "$3" = "$accepted" ]
