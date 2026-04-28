#!/usr/bin/env bash
# Multi-profile launcher for the upstream TAK Server Spring Boot fat
# jar. Generates self-signed certs at startup, then launches config
# + messaging in parallel — they self-coordinate via Ignite cluster
# discovery. Sequential startup hangs because config does not fully
# come up until at least one peer (messaging) has joined the
# topology.
set -e

cd /opt/tak

# --------------------------------------------------------------------
# 1. Self-signed cert generation (one-time per container).
# --------------------------------------------------------------------
mkdir -p /opt/tak/certs/files
if [[ ! -f /opt/tak/certs/files/takserver.jks ]]; then
    echo "[start] generating self-signed RSA cert + JKS keystores..."
    openssl req -x509 -newkey rsa:2048 -nodes \
        -keyout /opt/tak/certs/files/takserver.key \
        -out /opt/tak/certs/files/takserver.crt \
        -days 365 \
        -subj "/CN=takserver-bench/O=tak-rs-bench/C=US" \
        2> /dev/null

    openssl pkcs12 -export \
        -inkey /opt/tak/certs/files/takserver.key \
        -in /opt/tak/certs/files/takserver.crt \
        -out /opt/tak/certs/files/takserver.p12 \
        -name takserver \
        -passout pass:atakatak

    keytool -importkeystore -noprompt \
        -srckeystore /opt/tak/certs/files/takserver.p12 \
        -srcstorepass atakatak -srcstoretype PKCS12 \
        -destkeystore /opt/tak/certs/files/takserver.jks \
        -deststorepass atakatak -deststoretype JKS 2> /dev/null

    keytool -import -noprompt -alias root \
        -file /opt/tak/certs/files/takserver.crt \
        -keystore /opt/tak/certs/files/truststore-root.jks \
        -storepass atakatak 2> /dev/null

    chmod 644 /opt/tak/certs/files/*.jks
    echo "[start] certs ready"
fi

# --------------------------------------------------------------------
# 2. Launch config + messaging in parallel (Ignite self-coordinates).
# --------------------------------------------------------------------
JAVA_OPTS="-server -XX:+UseG1GC -XX:+AlwaysPreTouch \
    -Dlogging.level.root=WARN \
    -Dlogging.level.com.bbn=INFO \
    -Dlogging.level.tak=INFO"

echo "[start] launching config profile..."
java $JAVA_OPTS -Xms512m -Xmx1g \
    -Dspring.profiles.active=config \
    -jar takserver.jar > logs/config.log 2>&1 &
CONFIG_PID=$!

# Brief beat so config registers its Ignite node before messaging
# joins. Avoids a transient "Failed to find deployed service" race
# observed when the two start within the same millisecond.
sleep 5

echo "[start] launching messaging profile..."
java $JAVA_OPTS -Xms2g -Xmx4g \
    -Dspring.profiles.active=messaging \
    -jar takserver.jar > logs/messaging.log 2>&1 &
MESSAGING_PID=$!

echo "[start] config pid=$CONFIG_PID  messaging pid=$MESSAGING_PID"

# Stream both wrapper log files to docker stdout in the background.
(tail -F logs/config.log    | sed 's/^/[config]    /') &
(tail -F logs/messaging.log | sed 's/^/[messaging] /') &

trap "kill $CONFIG_PID $MESSAGING_PID 2>/dev/null; exit 0" SIGTERM SIGINT

wait $MESSAGING_PID
echo "[start] messaging exited; killing config"
kill $CONFIG_PID 2> /dev/null || true
