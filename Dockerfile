# syntax=docker/dockerfile:1.7

FROM ghcr.io/openabdev/openab:0.10.0-beta.3-claude@sha256:2b9fca58d898fdc5bceb2d48bcdd774287ece3352d6e3efbad7a213072232a89

USER root

ARG CLAUDE_AGENT_ACP_VERSION=0.70.0
ARG CLAUDE_CODE_VERSION=2.1.251
ARG CLAUDE_AGENT_SDK_VERSION=0.3.251

COPY claude-latest-runtime/package.json claude-latest-runtime/package-lock.json \
     /opt/claude-latest-runtime/

COPY --chmod=755 claude-latest-runtime/canary-entrypoint.sh \
     /usr/local/bin/claude-latest-canary

RUN cd /opt/claude-latest-runtime \
 && npm ci --omit=dev --ignore-scripts \
 && npm install -g "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" --retry 3 \
 && test "$(node -p "require('./node_modules/@agentclientprotocol/claude-agent-acp/package.json').version")" = \
      "${CLAUDE_AGENT_ACP_VERSION}" \
 && test "$(node -p "require('./node_modules/@anthropic-ai/claude-agent-sdk/package.json').version")" = \
      "${CLAUDE_AGENT_SDK_VERSION}" \
 && test "$(claude --version | awk '{print $1}')" = "${CLAUDE_CODE_VERSION}"

ENV PATH="/opt/claude-latest-runtime/node_modules/.bin:${PATH}"
ENV CLAUDE_CODE_EXECUTABLE=/usr/local/bin/claude

USER node

CMD ["claude-latest-canary"]
