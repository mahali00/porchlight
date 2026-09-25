#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([^,}]*\).*/\1/p')
  [ -z "$id" ] && continue
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize) result='{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}' ;;
    tools/list) result='{"tools":[{"name":"echo","annotations":{"readOnlyHint":true}},{"name":"wipe"}]}' ;;
    tools/call) result="{\"content\":[{\"type\":\"text\",\"text\":\"pid $$\"}]}" ;;
    *) result='{}' ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
