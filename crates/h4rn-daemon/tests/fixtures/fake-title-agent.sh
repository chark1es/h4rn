#!/bin/sh
# A one-shot coding-agent fixture for the text generation tests.
#
# It records the arguments and the stdin it was given beside itself, then
# answers with one line the way a real harness would. Copy it into a temp
# directory before running it so the record files do not land in the repo.
dir=$(dirname "$0")
printf '%s\n' "$@" > "$dir/argv.txt"
cat > "$dir/stdin.txt"
printf 'Fixture title\n'
