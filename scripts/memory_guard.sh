#!/bin/bash
# Memory guard script for Rush builds
# Monitors system memory and kills rush processes if memory exceeds threshold

set -e

# Configuration
THRESHOLD_GB=40
CHECK_INTERVAL=5  # seconds between checks

# Convert threshold to KB for comparison
THRESHOLD_KB=$((THRESHOLD_GB * 1024 * 1024))

echo "Memory Guard: Starting monitoring (threshold: ${THRESHOLD_GB}GB, interval: ${CHECK_INTERVAL}s)"
echo "Will kill 'rush' processes if memory usage exceeds threshold"
echo "Press Ctrl+C to stop"
echo ""

while true; do
    # Get current memory usage in KB (used = total - available)
    TOTAL_MEM=$(grep MemTotal /proc/meminfo | awk '{print $2}')
    AVAIL_MEM=$(grep MemAvailable /proc/meminfo | awk '{print $2}')
    USED_MEM=$((TOTAL_MEM - AVAIL_MEM))

    # Convert to GB for display
    USED_GB=$(awk "BEGIN {printf \"%.2f\", $USED_MEM / 1024 / 1024}")
    THRESHOLD_DISPLAY=$(awk "BEGIN {printf \"%.2f\", $THRESHOLD_KB / 1024 / 1024}")

    # Check if threshold exceeded
    if [ $USED_MEM -gt $THRESHOLD_KB ]; then
        echo "[$(date '+%Y-%m-%d %H:%M:%S')] THRESHOLD EXCEEDED: ${USED_GB}GB / ${THRESHOLD_DISPLAY}GB"

        # Find rush processes
        RUSH_PIDS=$(pgrep -f 'rush' || true)

        if [ -n "$RUSH_PIDS" ]; then
            echo "Killing rush processes: $RUSH_PIDS"

            # Kill each process
            for PID in $RUSH_PIDS; do
                if kill -9 $PID 2>/dev/null; then
                    echo "  Killed PID $PID"
                else
                    echo "  PID $PID already terminated"
                fi
            done

            echo "Memory guard: Rush processes terminated"
            echo ""

            # Wait a bit for memory to be released
            sleep 2

            # Show new memory usage
            AVAIL_MEM=$(grep MemAvailable /proc/meminfo | awk '{print $2}')
            USED_MEM=$((TOTAL_MEM - AVAIL_MEM))
            USED_GB=$(awk "BEGIN {printf \"%.2f\", $USED_MEM / 1024 / 1024}")
            echo "[$(date '+%Y-%m-%d %H:%M:%S')] Memory after cleanup: ${USED_GB}GB"
        else
            echo "No rush processes found to kill"
        fi

        echo ""
    else
        # Normal monitoring - show status every check
        echo -ne "\r[$(date '+%Y-%m-%d %H:%M:%S')] Memory: ${USED_GB}GB / ${THRESHOLD_DISPLAY}GB - OK"
    fi

    sleep $CHECK_INTERVAL
done
