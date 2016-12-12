#!/usr/bin/env bash

printf '%s\0' "$1" | socat UNIX-CONNECT:./command_folder/sock STDIO
