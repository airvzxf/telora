#!/usr/bin/env bash
# scripts/compatibility/simulate.sh - Full stack simulation (Daemon + UI)
set -eu

# Function to clean up background processes
cleanup() {
    echo ""
    echo "--> Terminating processes..."
    # Use SIGKILL to ensure they stop immediately and dont hang the container session
    [ -n "${DAEMON_PID:-}" ] && kill -9 "$DAEMON_PID" 2>/dev/null || true
    [ -n "${CLIENT_PID:-}" ] && kill -9 "$CLIENT_PID" 2>/dev/null || true
    
    # Global cleanup as fallback
    pkill -9 -f "./bin/telora-daemon" 2>/dev/null || true
    pkill -9 -f "./bin/telora-gui" 2>/dev/null || true
    echo "--> Cleanup done."
}

# Trap for unexpected exits, but we will also call it manually
trap cleanup EXIT

# Ensure environment is set up
if [ -f "./scripts/compatibility/setup-env.sh" ]; then
    ./scripts/compatibility/setup-env.sh
fi

echo "--> Starting Full Stack Simulation..."

# Check if binaries exist
for bin in telora-daemon telora-gui telora; do
    if [ ! -f "bin/$bin" ]; then
        echo "Error: bin/$bin not found! Build them first with ./scripts/build"
        exit 1
    fi
done

echo "--> Launching telora-daemon..."
./bin/telora-daemon > /dev/null 2>&1 &
DAEMON_PID=$!
echo "--> Daemon PID: $DAEMON_PID"

echo "Wait for daemon (3s)..."
sleep 3

echo "--> Launching telora-gui..."
./bin/telora-gui > /dev/null 2>&1 &
CLIENT_PID=$!
echo "--> GUI PID: $CLIENT_PID"

echo "Wait for client (1s)..."
sleep 1

echo "--> Action: Start Recording"
./bin/telora toggle-copy

echo "--> Speaking (4s)..."
sleep 4

echo "--> Action: Stop Recording"
./bin/telora toggle-copy

echo "--> Waiting for finalization (2s)..."
sleep 2

echo "--> Simulation finished successfully."
# Explicit cleanup before exit to avoid trap races
cleanup
trap - EXIT
exit 0
