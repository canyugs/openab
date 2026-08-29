#!/bin/sh
set -eu

openab_version="$(openab --version)"
acp_version="$(claude-agent-acp --version)"
claude_version="$(claude --version)"
sdk_version="$(node -p "require('/opt/claude-latest-runtime/node_modules/@anthropic-ai/claude-agent-sdk/package.json').version")"

printf 'canary openab=%s\n' "$openab_version"
printf 'canary acp=%s\n' "$acp_version"
printf 'canary claude=%s\n' "$claude_version"
printf 'canary sdk=%s\n' "$sdk_version"

init_response="$(printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"zeabur-canary","version":"1"}}}' | claude-agent-acp)"
printf '%s' "$init_response" | node -e '
  let input = "";
  process.stdin.on("data", (chunk) => { input += chunk; });
  process.stdin.on("end", () => {
    const response = JSON.parse(input);
    const result = response.result || {};
    if (response.id !== 1 || result.protocolVersion !== 1) process.exit(1);
    if (result.agentInfo?.version !== "0.70.0") process.exit(1);
    process.stdout.write("canary acp_initialize=PASS\n");
  });
'

export CANARY_OPENAB_VERSION="$openab_version"
export CANARY_ACP_VERSION="$acp_version"
export CANARY_CLAUDE_VERSION="$claude_version"
export CANARY_SDK_VERSION="$sdk_version"

exec node -e '
  const http = require("http");
  const body = JSON.stringify({
    ok: true,
    openab: process.env.CANARY_OPENAB_VERSION,
    acp: process.env.CANARY_ACP_VERSION,
    claude: process.env.CANARY_CLAUDE_VERSION,
    sdk: process.env.CANARY_SDK_VERSION,
    acpInitialize: "PASS"
  });
  http.createServer((request, response) => {
    response.writeHead(request.url === "/health" ? 200 : 404, {"content-type": "application/json"});
    response.end(request.url === "/health" ? body : JSON.stringify({ok: false}));
  }).listen(8080, "0.0.0.0", () => process.stdout.write("canary health=http://0.0.0.0:8080/health\n"));
'
