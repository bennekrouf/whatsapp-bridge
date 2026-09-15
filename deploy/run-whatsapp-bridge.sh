#!/bin/bash
# =============================================================================
# How the bridge starts in production.
#
# THIS FILE IS THE SOURCE OF TRUTH. The copy at /opt/api0/run-whatsapp-bridge.sh
# on the production host should be a symlink to this one:
#
#   sudo mv /opt/api0/run-whatsapp-bridge.sh /opt/api0/run-whatsapp-bridge.sh.bak
#   sudo ln -s /opt/api0/src/whatsapp-bridge/deploy/run-whatsapp-bridge.sh \
#              /opt/api0/run-whatsapp-bridge.sh
#
# Without that symlink the deployed script and this file drift apart, and the
# one in git stops describing what actually runs — which is exactly how a
# four-month-old binary kept serving traffic while every rebuild looked fine.
#
# pm2 runs THIS, not the binary:
#   pm2 start /opt/api0/run-whatsapp-bridge.sh --name api0-whatsapp-bridge
#
# Note the paths below resolve relative to this script's own location, so with
# the symlink in place they still mean /opt/api0/... — $0 is the symlink path,
# not its target.
# =============================================================================
set -a; source "$(dirname "$0")/whatsapp-bridge.env"; set +a
exec "$(dirname "$0")/bin/whatsapp-bridge"
