#!/bin/sh
# A one-shot coding-agent fixture for the text generation integration test.
# Real harnesses receive the title prompt on stdin and answer with one line.
cat >/dev/null
printf 'Fixture title\n'
