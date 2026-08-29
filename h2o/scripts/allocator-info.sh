#!/bin/sh
set -eu

if test -r /usr/share/doc/h2o/allocator.txt; then
  tr -d '\n' </usr/share/doc/h2o/allocator.txt
  printf '\n'
  exit 0
fi

printf 'allocator=unknown\n'
