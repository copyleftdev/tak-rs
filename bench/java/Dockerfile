# Upstream TAK Server messaging service, configured for the M5 firehose bench.
#
# Build context: bench/java/. Expects scripts/bench-java-baseline.sh
# to have already run the gradle build inside .scratch/takserver-java/
# and copied takserver-core-*.jar into bench/java/tak/.
#
# We only run the messaging service (firehose / dispatch path); the
# upstream multi-process orchestrator (config + messaging + API +
# plugin manager) is overkill for measuring streaming-TCP CoT input
# throughput. Spring Boot fat jar + messaging profile = listener on
# the configured ports.

FROM eclipse-temurin:17-jdk-jammy

RUN apt-get update -qq && \
    apt-get install -y -qq --no-install-recommends \
        net-tools netcat-openbsd openssl ca-certificates && \
    rm -rf /var/lib/apt/lists/*

RUN useradd --system --home /opt/tak --shell /bin/false tak && \
    mkdir -p /opt/tak/data/logs /opt/tak/data/certs /opt/tak/logs

WORKDIR /opt/tak

# Bring in the bootJar produced by the upstream gradle build.
COPY tak/takserver-core-*.jar /opt/tak/takserver.jar

# Configs.
COPY CoreConfig.xml /opt/tak/CoreConfig.xml
COPY TAKIgniteConfig.xml /opt/tak/TAKIgniteConfig.xml
COPY UserAuthenticationFile.xml /opt/tak/UserAuthenticationFile.xml

# Optional cert scaffolding (the messaging service tries to load
# certs even when no TLS input is configured — the security block in
# CoreConfig.xml references files that need to exist).
COPY tak/certs /opt/tak/certs
RUN chmod -R +x /opt/tak/certs/*.sh 2>/dev/null || true

# Multi-profile launcher (config + messaging in the same container).
COPY start.sh /opt/tak/start.sh
RUN chmod +x /opt/tak/start.sh

EXPOSE 8087 8088 8443

ENTRYPOINT ["/opt/tak/start.sh"]
