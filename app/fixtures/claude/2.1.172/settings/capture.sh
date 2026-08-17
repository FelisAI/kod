#!/bin/bash
# Read stdin and log to file
cat > /tmp/hook-input.jsonl
echo "{\"exit\": 0}" >&2
